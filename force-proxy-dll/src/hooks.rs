//! Winsock2 function hooks via `minhook`.
//!
//! Hooks 9 functions: connect, bind, closesocket, sendto, recvfrom,
//! WSASendTo, WSARecvFrom, ioctlsocket, WSAEventSelect.

use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::mem;
use std::sync::{Mutex, OnceLock};

use minhook::MinHook;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Networking::WinSock::*;
use windows_sys::Win32::System::LibraryLoader::*;

use crate::socks5::{self, UdpAssociation};

// ── proxy config ───────────────────────────────────────────────────────────

pub struct ProxyConfig {
    pub address: u32, // network-byte-order IPv4
    pub port: u16,    // network-byte-order port
    pub timeout: u32, // seconds
    pub login: String,
    pub password: String,
    pub udp: bool,    // 是否劫持 UDP 流量
}

static CONFIG: OnceLock<ProxyConfig> = OnceLock::new();

fn cfg() -> &'static ProxyConfig {
    CONFIG.get().expect("ProxyConfig not initialized")
}

// ── global state ───────────────────────────────────────────────────────────

struct SocketState {
    udp_assoc: HashMap<usize, UdpAssociation>,
    non_blocking: HashMap<usize, bool>,
}

fn state() -> &'static Mutex<SocketState> {
    static S: OnceLock<Mutex<SocketState>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(SocketState {
            udp_assoc: HashMap::new(),
            non_blocking: HashMap::new(),
        })
    })
}

// ── type aliases ───────────────────────────────────────────────────────────

type FnConnect = unsafe extern "system" fn(SOCKET, *const SOCKADDR, c_int) -> c_int;
type FnBind = unsafe extern "system" fn(SOCKET, *const SOCKADDR, c_int) -> c_int;
type FnClosesocket = unsafe extern "system" fn(SOCKET) -> c_int;
type FnSendto =
    unsafe extern "system" fn(SOCKET, *const u8, c_int, c_int, *const SOCKADDR, c_int) -> c_int;
type FnRecvfrom =
    unsafe extern "system" fn(SOCKET, *mut u8, c_int, c_int, *mut SOCKADDR, *mut c_int) -> c_int;
type FnIoctlsocket = unsafe extern "system" fn(SOCKET, c_int, *mut u32) -> c_int;
type FnWSAEventSelect = unsafe extern "system" fn(SOCKET, HANDLE, i32) -> c_int;
type FnWSASendTo = unsafe extern "system" fn(
    SOCKET,
    *const WSABUF,
    u32,
    *mut u32,
    u32,
    *const SOCKADDR,
    c_int,
    *mut c_void, /* OVERLAPPED */
    *mut c_void,
) -> c_int;
type FnWSARecvFrom = unsafe extern "system" fn(
    SOCKET,
    *const WSABUF,
    u32,
    *mut u32,
    *mut u32,
    *mut SOCKADDR,
    *mut c_int,
    *mut c_void, /* OVERLAPPED */
    *mut c_void,
) -> c_int;

// ── original function pointers (filled by MH_CreateHook) ──────────────────

static mut REAL_CONNECT: Option<FnConnect> = None;
static mut REAL_BIND: Option<FnBind> = None;
static mut REAL_CLOSESOCKET: Option<FnClosesocket> = None;
static mut REAL_SENDTO: Option<FnSendto> = None;
static mut REAL_RECVFROM: Option<FnRecvfrom> = None;
static mut REAL_IOCTLSOCKET: Option<FnIoctlsocket> = None;
static mut REAL_WSA_EVENT_SELECT: Option<FnWSAEventSelect> = None;
static mut REAL_WSA_SEND_TO: Option<FnWSASendTo> = None;
static mut REAL_WSA_RECV_FROM: Option<FnWSARecvFrom> = None;

// ── call-through helpers (used by socks5.rs) ───────────────────────────────

pub unsafe fn call_real_connect(s: SOCKET, name: *const SOCKADDR, namelen: c_int) -> c_int {
    (REAL_CONNECT.unwrap())(s, name, namelen)
}

pub unsafe fn call_real_ioctlsocket(s: SOCKET, cmd: c_int, argp: *mut u32) -> c_int {
    (REAL_IOCTLSOCKET.unwrap())(s, cmd, argp)
}

// ── helper predicates ──────────────────────────────────────────────────────

unsafe fn is_udp_socket(s: SOCKET) -> bool {
    let mut val: i32 = 0;
    let mut len: i32 = mem::size_of::<i32>() as i32;
    if getsockopt(s, SOL_SOCKET as i32, SO_TYPE as i32, &mut val as *mut _ as *mut u8, &mut len)
        != 0
    {
        return false;
    }
    val == SOCK_DGRAM as i32
}

fn is_non_blocking(s: SOCKET) -> bool {
    state()
        .lock()
        .unwrap()
        .non_blocking
        .contains_key(&(s as usize))
}

fn has_udp_assoc(s: SOCKET) -> bool {
    state()
        .lock()
        .unwrap()
        .udp_assoc
        .contains_key(&(s as usize))
}

fn get_udp_assoc(s: SOCKET) -> Option<UdpAssociation> {
    state()
        .lock()
        .unwrap()
        .udp_assoc
        .get(&(s as usize))
        .copied()
}

unsafe fn is_multicast(addr: *const SOCKADDR) -> bool {
    let ip = u32::from_be((*addr.cast::<SOCKADDR_IN>()).sin_addr.S_un.S_addr);
    (ip & 0xF000_0000) == 0xE000_0000
}

unsafe fn is_loopback_or_proxy(addr: *const SOCKADDR_IN) -> bool {
    let ip = (*addr).sin_addr.S_un.S_addr;
    let c = cfg();
    ip == c.address || ip == u32::from_ne_bytes([127, 0, 0, 1]) || ip == 0
}

// ── hook implementations ───────────────────────────────────────────────────

unsafe extern "system" fn detour_connect(
    s: SOCKET,
    name: *const SOCKADDR,
    namelen: c_int,
) -> c_int {
    let addr = &*(name as *const SOCKADDR_IN);

    if is_udp_socket(s) || is_loopback_or_proxy(addr) || addr.sin_family != AF_INET as u16 {
        return (REAL_CONNECT.unwrap())(s, name, namelen);
    }

    socks5::connect_through_socks5(s, addr, cfg(), is_non_blocking(s))
}

unsafe extern "system" fn detour_bind(
    s: SOCKET,
    addr: *const SOCKADDR,
    namelen: c_int,
) -> c_int {
    if cfg().udp && is_udp_socket(s) {
        // Check + insert under one logical operation to avoid TOCTOU race.
        // Lock is released during the blocking init_udp_association call.
        let needs_assoc = !state().lock().unwrap().udp_assoc.contains_key(&(s as usize));
        if needs_assoc {
            if let Some(entry) = socks5::init_udp_association(cfg()) {
                state().lock().unwrap().udp_assoc.insert(s as usize, entry);
            }
        }
    }
    (REAL_BIND.unwrap())(s, addr, namelen)
}

unsafe extern "system" fn detour_closesocket(s: SOCKET) -> c_int {
    {
        let mut st = state().lock().unwrap();
        if let Some(entry) = st.udp_assoc.remove(&(s as usize)) {
            closesocket(entry.proxy_socket);
        }
        st.non_blocking.remove(&(s as usize));
    }
    (REAL_CLOSESOCKET.unwrap())(s)
}

unsafe extern "system" fn detour_sendto(
    s: SOCKET,
    buf: *const u8,
    len: c_int,
    flags: c_int,
    to: *const SOCKADDR,
    tolen: c_int,
) -> c_int {
    if !to.is_null() && !is_multicast(to) {
        if let Some(entry) = get_udp_assoc(s) {
            let encap =
                socks5::encapsulate_udp(buf, len as usize, &*(to as *const SOCKADDR_IN));
            return (REAL_SENDTO.unwrap())(
                s,
                encap.as_ptr(),
                encap.len() as c_int,
                0,
                &entry.udp_proxy_addr as *const SOCKADDR_IN as *const SOCKADDR,
                mem::size_of::<SOCKADDR_IN>() as c_int,
            );
        }
    }
    (REAL_SENDTO.unwrap())(s, buf, len, flags, to, tolen)
}

unsafe extern "system" fn detour_recvfrom(
    s: SOCKET,
    buf: *mut u8,
    len: c_int,
    flags: c_int,
    from: *mut SOCKADDR,
    fromlen: *mut c_int,
) -> c_int {
    let received = (REAL_RECVFROM.unwrap())(s, buf, len, flags, from, fromlen);
    if received != SOCKET_ERROR && has_udp_assoc(s) {
        if received < 10 {
            return SOCKET_ERROR;
        }
        socks5::extract_udp_sender(buf, from as *mut SOCKADDR_IN);
        std::ptr::copy(buf.add(10), buf, (received - 10) as usize);
        return received - 10;
    }
    received
}

unsafe extern "system" fn detour_ioctlsocket(
    s: SOCKET,
    cmd: c_int,
    argp: *mut u32,
) -> c_int {
    if cmd == FIONBIO as i32 {
        let mut st = state().lock().unwrap();
        if *argp != 0 {
            st.non_blocking.insert(s as usize, true);
        } else {
            st.non_blocking.remove(&(s as usize));
        }
    }
    (REAL_IOCTLSOCKET.unwrap())(s, cmd, argp)
}

unsafe extern "system" fn detour_wsa_event_select(
    s: SOCKET,
    event: HANDLE,
    events: i32,
) -> c_int {
    {
        let mut st = state().lock().unwrap();
        if event != 0 && events != 0 {
            st.non_blocking.insert(s as usize, true);
        } else {
            st.non_blocking.remove(&(s as usize));
        }
    }
    (REAL_WSA_EVENT_SELECT.unwrap())(s, event, events)
}

unsafe extern "system" fn detour_wsa_send_to(
    s: SOCKET,
    bufs: *const WSABUF,
    buf_count: u32,
    bytes_sent: *mut u32,
    flags: u32,
    to: *const SOCKADDR,
    tolen: c_int,
    overlapped: *mut c_void, /* OVERLAPPED */
    completion: *mut c_void,
) -> c_int {
    if !to.is_null() && !is_multicast(to) {
        if let Some(entry) = get_udp_assoc(s) {
            if buf_count > 0 && !bufs.is_null() && (*bufs).len > 0 {
                let encap = socks5::encapsulate_udp(
                    (*bufs).buf,
                    (*bufs).len as usize,
                    &*(to as *const SOCKADDR_IN),
                );
                let wsabuf = WSABUF {
                    len: encap.len() as u32,
                    buf: encap.as_ptr() as *mut u8,
                };
                return (REAL_WSA_SEND_TO.unwrap())(
                    s,
                    &wsabuf,
                    1,
                    bytes_sent,
                    0,
                    &entry.udp_proxy_addr as *const SOCKADDR_IN as *const SOCKADDR,
                    mem::size_of::<SOCKADDR_IN>() as c_int,
                    overlapped,
                    completion,
                );
            }
        }
    }
    (REAL_WSA_SEND_TO.unwrap())(
        s, bufs, buf_count, bytes_sent, flags, to, tolen, overlapped, completion,
    )
}

unsafe extern "system" fn detour_wsa_recv_from(
    s: SOCKET,
    bufs: *const WSABUF,
    buf_count: u32,
    bytes_recvd: *mut u32,
    flags: *mut u32,
    from: *mut SOCKADDR,
    fromlen: *mut c_int,
    overlapped: *mut c_void, /* OVERLAPPED */
    completion: *mut c_void,
) -> c_int {
    let mut local_recvd: u32 = 0;
    let recvd_ptr = if bytes_recvd.is_null() {
        &mut local_recvd as *mut u32
    } else {
        bytes_recvd
    };

    let status = (REAL_WSA_RECV_FROM.unwrap())(
        s, bufs, buf_count, recvd_ptr, flags, from, fromlen, overlapped, completion,
    );

    if status == 0 && has_udp_assoc(s) {
        if *recvd_ptr < 10 {
            return SOCKET_ERROR;
        }
        socks5::extract_udp_sender((*bufs).buf, from as *mut SOCKADDR_IN);
        let new_len = *recvd_ptr - 10;
        std::ptr::copy((*bufs).buf.add(10), (*bufs).buf, new_len as usize);
        *recvd_ptr = new_len;
    }

    status
}

// ── init / destroy ─────────────────────────────────────────────────────────

fn read_env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

unsafe fn get_proc(module: &[u8], name: &[u8]) -> *mut c_void {
    let m = GetModuleHandleA(module.as_ptr());
    GetProcAddress(m, name.as_ptr()).unwrap() as *mut c_void
}

/// Helper: create a hook, store the trampoline via raw pointer.
unsafe fn hook<F>(target: *mut c_void, detour: *mut c_void, original: *mut Option<F>) {
    let trampoline = MinHook::create_hook(target, detour).expect("MinHook::create_hook failed");
    original.write(Some(mem::transmute_copy(&trampoline)));
}

/// Called once when the DLL is loaded.
pub unsafe fn init() {
    // Parse config from environment
    let addr_str = read_env("SOCKS5_PROXY_ADDRESS", "127.0.0.1");
    let port_str = read_env("SOCKS5_PROXY_PORT", "12334");
    let timeout_str = read_env("SOCKS5_PROXY_TIMEOUT", "30");

    let mut in_addr: IN_ADDR = mem::zeroed();
    let addr_cstr: Vec<u8> = addr_str.bytes().chain(std::iter::once(0)).collect();
    inet_pton(
        AF_INET as c_int,
        addr_cstr.as_ptr(),
        &mut in_addr as *mut IN_ADDR as *mut c_void,
    );

    let port: u16 = port_str.parse().unwrap_or(12334);

    let udp_str = read_env("SOCKS5_PROXY_UDP", "true");
    let _ = CONFIG.set(ProxyConfig {
        address: in_addr.S_un.S_addr,
        port: htons(port),
        timeout: timeout_str.parse().unwrap_or(30),
        login: read_env("SOCKS5_PROXY_LOGIN", ""),
        password: read_env("SOCKS5_PROXY_PASSWORD", ""),
        udp: udp_str.eq_ignore_ascii_case("true"),
    });

    let ws2 = b"ws2_32.dll\0";

    macro_rules! install_hook {
        ($name:literal, $detour:expr, $real:ident) => {
            hook(
                get_proc(ws2, concat!($name, "\0").as_bytes()),
                $detour as *mut c_void,
                std::ptr::addr_of_mut!($real),
            )
        };
    }

    install_hook!("connect", detour_connect, REAL_CONNECT);
    install_hook!("bind", detour_bind, REAL_BIND);
    install_hook!("closesocket", detour_closesocket, REAL_CLOSESOCKET);
    install_hook!("sendto", detour_sendto, REAL_SENDTO);
    install_hook!("recvfrom", detour_recvfrom, REAL_RECVFROM);
    install_hook!("ioctlsocket", detour_ioctlsocket, REAL_IOCTLSOCKET);
    install_hook!("WSAEventSelect", detour_wsa_event_select, REAL_WSA_EVENT_SELECT);
    install_hook!("WSASendTo", detour_wsa_send_to, REAL_WSA_SEND_TO);
    install_hook!("WSARecvFrom", detour_wsa_recv_from, REAL_WSA_RECV_FROM);

    // Enable all hooks at once
    MinHook::enable_all_hooks().expect("MinHook::enable_all_hooks failed");
}

pub unsafe fn destroy() {
    let _ = MinHook::disable_all_hooks();
}
