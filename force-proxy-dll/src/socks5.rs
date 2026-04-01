//! SOCKS5 protocol implementation (TCP CONNECT + UDP ASSOCIATE).

use std::ffi::c_int;
use std::mem;
use windows_sys::Win32::Networking::WinSock::*;

use crate::hooks::{self, ProxyConfig};

// ── helpers ────────────────────────────────────────────────────────────────

pub unsafe fn wait_for_write(s: SOCKET, timeout_sec: i32) -> bool {
    let mut set: FD_SET = mem::zeroed();
    set.fd_count = 1;
    set.fd_array[0] = s;
    let mut tv = TIMEVAL {
        tv_sec: timeout_sec,
        tv_usec: 0,
    };
    select(0, std::ptr::null_mut(), &mut set, std::ptr::null_mut(), &mut tv) > 0
}

pub unsafe fn wait_for_read(s: SOCKET, timeout_sec: i32) -> bool {
    let mut set: FD_SET = mem::zeroed();
    set.fd_count = 1;
    set.fd_array[0] = s;
    let mut tv = TIMEVAL {
        tv_sec: timeout_sec,
        tv_usec: 0,
    };
    select(0, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv) > 0
}

pub unsafe fn set_non_blocking(s: SOCKET, non_blocking: bool) -> bool {
    let mut mode: u32 = if non_blocking { 1 } else { 0 };
    hooks::call_real_ioctlsocket(s, FIONBIO as i32, &mut mode) == 0
}

// ── connect to proxy server ────────────────────────────────────────────────

unsafe fn connect_to_proxy(s: SOCKET, cfg: &ProxyConfig, non_blocking: bool) -> c_int {
    let mut addr: SOCKADDR_IN = mem::zeroed();
    addr.sin_family = AF_INET as u16;
    addr.sin_addr.S_un.S_addr = cfg.address;
    addr.sin_port = cfg.port;

    let ret = hooks::call_real_connect(
        s,
        &addr as *const SOCKADDR_IN as *const SOCKADDR,
        mem::size_of::<SOCKADDR_IN>() as c_int,
    );

    if ret == SOCKET_ERROR && WSAGetLastError() != WSAEWOULDBLOCK {
        return ret;
    }

    if non_blocking {
        wait_for_write(s, cfg.timeout as i32);
    }

    0 // SUCCESS
}

unsafe fn send_socks5_handshake(s: SOCKET, cfg: &ProxyConfig, non_blocking: bool) -> c_int {
    let has_auth = !cfg.login.is_empty() && cfg.login != "empty";

    // Greeting: version, n_methods, methods...
    let (request, req_len): ([u8; 4], usize) = if has_auth {
        ([0x05, 0x02, 0x00, 0x02], 4) // NO_AUTH + USERNAME/PASSWORD
    } else {
        ([0x05, 0x01, 0x00, 0x00], 3) // NO_AUTH only
    };

    if non_blocking {
        wait_for_write(s, cfg.timeout as i32);
    }
    if send(s, request.as_ptr() as *const u8, req_len as c_int, 0) == SOCKET_ERROR {
        return SOCKET_ERROR;
    }

    // Response
    let mut response = [0u8; 2];
    if non_blocking {
        wait_for_read(s, cfg.timeout as i32);
    }
    if recv(s, response.as_mut_ptr(), 2, 0) <= 0 {
        return SOCKET_ERROR;
    }
    if response[0] != 0x05 {
        return SOCKET_ERROR;
    }

    match response[1] {
        0x00 => {} // No auth required
        0x02 => {
            // Username/password auth (RFC 1929)
            if !has_auth {
                return SOCKET_ERROR;
            }
            let login = cfg.login.as_bytes();
            let password = cfg.password.as_bytes();
            if login.len() > 255 || password.len() > 255 {
                return SOCKET_ERROR;
            }

            let mut auth = [0u8; 1 + 1 + 255 + 1 + 255];
            let mut pos = 0;
            auth[pos] = 0x01; // sub-negotiation version
            pos += 1;
            auth[pos] = login.len() as u8;
            pos += 1;
            auth[pos..pos + login.len()].copy_from_slice(login);
            pos += login.len();
            auth[pos] = password.len() as u8;
            pos += 1;
            auth[pos..pos + password.len()].copy_from_slice(password);
            pos += password.len();

            if non_blocking {
                wait_for_write(s, cfg.timeout as i32);
            }
            if send(s, auth.as_ptr() as *const u8, pos as c_int, 0) == SOCKET_ERROR {
                return SOCKET_ERROR;
            }

            let mut auth_resp = [0u8; 2];
            if non_blocking {
                wait_for_read(s, cfg.timeout as i32);
            }
            if recv(s, auth_resp.as_mut_ptr(), 2, 0) <= 0 {
                return SOCKET_ERROR;
            }
            if auth_resp[0] != 0x01 || auth_resp[1] != 0x00 {
                return SOCKET_ERROR; // auth failed
            }
        }
        _ => return SOCKET_ERROR, // unsupported / 0xFF
    }

    0
}

// ── public API ─────────────────────────────────────────────────────────────

/// TCP CONNECT through SOCKS5.
pub unsafe fn connect_through_socks5(
    s: SOCKET,
    target: &SOCKADDR_IN,
    cfg: &ProxyConfig,
    non_blocking: bool,
) -> c_int {
    if connect_to_proxy(s, cfg, non_blocking) != 0 {
        return SOCKET_ERROR;
    }
    if send_socks5_handshake(s, cfg, non_blocking) != 0 {
        return SOCKET_ERROR;
    }

    // CONNECT request: VER CMD RSV ATYP ADDR PORT
    let mut req = [0u8; 10];
    req[0] = 0x05; // SOCKS5
    req[1] = 0x01; // CONNECT
    req[2] = 0x00; // reserved
    req[3] = 0x01; // IPv4
    std::ptr::copy_nonoverlapping(
        &target.sin_addr as *const IN_ADDR as *const u8,
        req.as_mut_ptr().add(4),
        4,
    );
    std::ptr::copy_nonoverlapping(
        &target.sin_port as *const u16 as *const u8,
        req.as_mut_ptr().add(8),
        2,
    );

    if non_blocking {
        wait_for_write(s, cfg.timeout as i32);
    }
    send(s, req.as_ptr() as *const u8, 10, 0);

    let mut resp = [0u8; 10];
    if non_blocking {
        wait_for_read(s, cfg.timeout as i32);
    }
    recv(s, resp.as_mut_ptr(), 10, 0);

    if resp[1] != 0x00 {
        return SOCKET_ERROR;
    }

    if non_blocking {
        WSASetLastError(WSAEWOULDBLOCK);
        return SOCKET_ERROR;
    }

    0
}

/// UDP ASSOCIATE entry.
#[derive(Clone, Copy)]
pub struct UdpAssociation {
    pub proxy_socket: SOCKET,
    pub udp_proxy_addr: SOCKADDR_IN,
}

/// Establish a SOCKS5 UDP ASSOCIATE.
pub unsafe fn init_udp_association(cfg: &ProxyConfig) -> Option<UdpAssociation> {
    let proxy_socket = socket(AF_INET as c_int, SOCK_STREAM as c_int, IPPROTO_TCP as c_int);
    if proxy_socket == INVALID_SOCKET {
        return None;
    }

    set_non_blocking(proxy_socket, true);

    if connect_to_proxy(proxy_socket, cfg, true) != 0 {
        closesocket(proxy_socket);
        return None;
    }
    if send_socks5_handshake(proxy_socket, cfg, true) != 0 {
        closesocket(proxy_socket);
        return None;
    }

    // UDP ASSOCIATE: VER CMD RSV ATYP DST.ADDR DST.PORT (all zeros = relay anything)
    let req = [0x05u8, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    wait_for_write(proxy_socket, cfg.timeout as i32);
    send(proxy_socket, req.as_ptr() as *const u8, 10, 0);

    let mut resp = [0u8; 10];
    wait_for_read(proxy_socket, cfg.timeout as i32);
    recv(proxy_socket, resp.as_mut_ptr(), 10, 0);

    if resp[1] != 0x00 {
        closesocket(proxy_socket);
        return None;
    }

    let mut addr: SOCKADDR_IN = mem::zeroed();
    addr.sin_family = AF_INET as u16;
    std::ptr::copy_nonoverlapping(resp.as_ptr().add(4), &mut addr.sin_addr as *mut IN_ADDR as *mut u8, 4);
    std::ptr::copy_nonoverlapping(resp.as_ptr().add(8), &mut addr.sin_port as *mut u16 as *mut u8, 2);

    Some(UdpAssociation {
        proxy_socket,
        udp_proxy_addr: addr,
    })
}

/// Prepend SOCKS5 UDP header (10 bytes) to a packet.
///   [0..2] reserved, [2] frag=0, [3] atyp=1 (IPv4), [4..8] addr, [8..10] port
pub unsafe fn encapsulate_udp(buf: *const u8, len: usize, to: &SOCKADDR_IN) -> Vec<u8> {
    let mut out = vec![0u8; 10 + len];
    out[0] = 0; // RSV
    out[1] = 0; // RSV
    out[2] = 0; // FRAG
    out[3] = 1; // IPv4
    std::ptr::copy_nonoverlapping(
        &to.sin_addr as *const IN_ADDR as *const u8,
        out.as_mut_ptr().add(4),
        4,
    );
    std::ptr::copy_nonoverlapping(
        &to.sin_port as *const u16 as *const u8,
        out.as_mut_ptr().add(8),
        2,
    );
    std::ptr::copy_nonoverlapping(buf, out.as_mut_ptr().add(10), len);
    out
}

/// Extract the original sender address from a SOCKS5 UDP header.
pub unsafe fn extract_udp_sender(buf: *const u8, from: *mut SOCKADDR_IN) {
    std::ptr::copy_nonoverlapping(buf.add(4), &mut (*from).sin_addr as *mut IN_ADDR as *mut u8, 4);
    std::ptr::copy_nonoverlapping(buf.add(8), &mut (*from).sin_port as *mut u16 as *mut u8, 2);
}
