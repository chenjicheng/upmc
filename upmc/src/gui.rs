// ============================================================
// gui.rs — 原生 Windows GUI 窗口
// ============================================================
// 使用 native-windows-gui (nwg) 创建一个小窗口，包含：
//   - 状态文本 (显示当前操作)
//   - 进度条
//   - "启动 PCL" / "启用 Discord 代理" 按钮
//
// 更新逻辑运行在后台线程中，通过 nwg::Notice 机制
// 线程安全地通知 GUI 更新进度。
// ============================================================

use native_windows_derive as nwd;
use native_windows_gui as nwg;

use nwd::NwgUi;
use nwg::NativeUi;

use std::cell::RefCell;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::config::{self, ChannelConfig};
use crate::discord_proxy;
use crate::update::{self, Progress, UpdateResult};

/// 更新完成后的结果状态。
#[derive(Debug, Clone)]
enum FinishState {
    /// 更新成功，proxy_running = 代理是否已自动启动
    Success { proxy_running: bool },
    /// 更新器已自更新并重启新进程，当前进程仅需退出
    SelfUpdateRestarting,
    /// Java 未安装，显示友好安装指引
    JavaNotFound,
    /// 更新出错
    Error(String),
    /// Discord 代理设置成功
    ProxySuccess,
    /// Discord 代理设置失败
    ProxyError(String),
}

/// 共享的进度状态，后台线程写入，GUI 线程读取。
#[derive(Debug, Clone, Default)]
struct SharedState {
    progress: Progress,
    log: Vec<String>,
    finish: Option<FinishState>,
}

/// RAII guard：后台线程 panic 时自动设置错误状态并通知 GUI，防止窗口挂起。
struct PanicGuard {
    state: Arc<Mutex<SharedState>>,
    sender: nwg::NoticeSender,
    completed: bool,
}

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if !self.completed {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.finish.is_none() {
                s.log
                    .push("[错误] 更新器内部错误（线程异常退出）".to_string());
                s.finish = Some(FinishState::Error(
                    "更新器内部错误（线程异常退出）".to_string(),
                ));
            }
            drop(s);
            self.sender.notice();
        }
    }
}

/// GUI 窗口定义
#[derive(Default, NwgUi)]
pub struct UpdaterApp {
    // ── 嵌入资源 ──
    #[nwg_resource]
    embed: nwg::EmbedResource,

    #[nwg_resource(source_embed: Some(&data.embed), source_embed_id: 1)]
    app_icon: nwg::Icon,

    // ── 窗口 ──
    #[nwg_control(
        title: "",
        size: (420, 195),
        position: (300, 300),
        flags: "WINDOW|VISIBLE",
        center: true,
        icon: Some(&data.app_icon)
    )]
    #[nwg_events(OnWindowClose: [UpdaterApp::on_close])]
    window: nwg::Window,

    // ── 状态文本 ──
    #[nwg_control(
        text: "正在初始化...",
        size: (380, 25),
        position: (20, 20),
        flags: "VISIBLE"
    )]
    status_label: nwg::Label,

    // ── 进度条 ──
    #[nwg_control(
        size: (380, 25),
        position: (20, 55),
        range: 0..100,
        pos: 0
    )]
    progress_bar: nwg::ProgressBar,

    // ── 底部提示 ──
    #[nwg_control(
        text: "",
        size: (380, 20),
        position: (20, 95),
        flags: "VISIBLE"
    )]
    hint_label: nwg::Label,

    // ── 启动 PCL 按钮（初始隐藏） ──
    #[nwg_control(
        text: "启动 PCL",
        size: (140, 35),
        position: (20, 120)
    )]
    #[nwg_events(OnButtonClick: [UpdaterApp::on_launch_pcl])]
    btn_launch_pcl: nwg::Button,

    // ── 启用 Discord 代理按钮（初始隐藏） ──
    #[nwg_control(
        text: "启用 Discord 代理",
        size: (175, 35),
        position: (165, 120)
    )]
    #[nwg_events(OnButtonClick: [UpdaterApp::on_enable_discord_proxy])]
    btn_discord_proxy: nwg::Button,

    // ── 设置按钮（初始隐藏） ──
    #[nwg_control(
        text: "设置",
        size: (55, 35),
        position: (345, 120)
    )]
    #[nwg_events(OnButtonClick: [UpdaterApp::on_settings])]
    btn_settings: nwg::Button,

    // ── 布局管理器 ──
    #[nwg_layout(parent: window, spacing: 1, max_row: Some(5), max_column: Some(1))]
    layout: nwg::GridLayout,

    // ── Notice（后台线程 → GUI） ──
    #[nwg_control]
    #[nwg_events(OnNotice: [UpdaterApp::on_progress_update])]
    progress_notice: nwg::Notice,

    // ── 内部状态 ──
    shared_state: Arc<Mutex<SharedState>>,
    base_dir: RefCell<PathBuf>,
}

impl UpdaterApp {
    /// 启动更新器 GUI。
    pub fn run(base_dir: PathBuf, channel_config: ChannelConfig) {
        nwg::init().expect("初始化 Windows GUI 失败");
        nwg::Font::set_global_family("Microsoft YaHei UI").expect("设置字体失败");

        let app = UpdaterApp {
            shared_state: Arc::new(Mutex::new(SharedState {
                progress: Progress {
                    percent: 0,
                    message: "正在初始化...".to_string(),
                },
                log: Vec::new(),
                finish: None,
            })),
            base_dir: RefCell::new(base_dir),
            ..Default::default()
        };

        let app = UpdaterApp::build_ui(app).expect("构建 UI 失败");

        // 设置窗口标题
        let title = config::window_title(channel_config.channel);
        app.window.set_text(&title);
        app.hint_label.set_text("请勿关闭此窗口...");

        app.btn_launch_pcl.set_visible(false);
        app.btn_discord_proxy.set_visible(false);
        app.btn_settings.set_visible(false);

        // 启动后台更新线程
        let state = Arc::clone(&app.shared_state);
        let notice_sender = app.progress_notice.sender();
        let base_dir = app.base_dir.borrow().clone();
        let channel_config_clone = channel_config;

        thread::spawn(move || {
            let mut guard = PanicGuard {
                state: Arc::clone(&state),
                sender: notice_sender,
                completed: false,
            };

            let result =
                update::run_update(&base_dir, &channel_config_clone, &|progress: Progress| {
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.log
                        .push(format!("[{}%] {}", progress.percent, progress.message));
                    s.progress = progress;
                    drop(s);
                    notice_sender.notice();
                });

            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.finish = Some(match result {
                Ok(UpdateResult::SelfUpdateRestarting) => {
                    s.log.push("[重启] 更新器已更新，正在重启...".to_string());
                    FinishState::SelfUpdateRestarting
                }
                Ok(UpdateResult::Success { proxy_running }) => {
                    s.log.push("[完成] 更新成功".to_string());
                    FinishState::Success { proxy_running }
                }
                Ok(UpdateResult::Offline) => {
                    s.log.push("[完成] 离线模式".to_string());
                    FinishState::Success { proxy_running: false }
                }
                Err(e) => {
                    let err_msg = format!("{e:#}");
                    s.log.push(format!("[错误] {err_msg}"));
                    if e.downcast_ref::<config::JavaNotFound>().is_some() {
                        FinishState::JavaNotFound
                    } else {
                        FinishState::Error(err_msg)
                    }
                }
            });
            drop(s);
            notice_sender.notice();
            guard.completed = true;
        });

        nwg::dispatch_thread_events();
    }

    /// 后台线程发来进度通知时调用
    fn on_progress_update(&self) {
        let (percent, message, finish, log_text) = {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            let percent = state.progress.percent;
            let message = state.progress.message.clone();
            let finish = state.finish.take();
            let log_text = if matches!(
                finish,
                Some(FinishState::Error(_) | FinishState::ProxyError(_))
            ) {
                Some(state.log.join("\r\n"))
            } else {
                None
            };
            (percent, message, finish, log_text)
        };

        self.progress_bar.set_pos(percent);
        self.status_label.set_text(&message);

        let finish = match finish {
            Some(f) => f,
            None => return,
        };

        match finish {
            FinishState::Success { proxy_running } => {
                if proxy_running {
                    self.show_action_buttons("更新完成，代理已就绪", Some("Xray 已在后台运行"));
                    self.btn_discord_proxy.set_text("停止代理");
                } else {
                    self.show_action_buttons("更新完成", None);
                    self.btn_discord_proxy.set_text("启用 Discord 代理");
                }
            }
            FinishState::SelfUpdateRestarting => {
                nwg::stop_thread_dispatch();
            }
            FinishState::JavaNotFound => {
                self.progress_bar.set_pos(0);
                self.status_label.set_text("需要安装 Java");
                self.hint_label.set_text("请安装 Java 后重新运行程序");
                nwg::modal_info_message(
                    &self.window,
                    "需要安装 Java",
                    &format!(
                        "未检测到系统 Java 环境。\n\
                         请安装 Java 后重新运行程序。\n\n\
                         下载地址（如未自动打开请手动访问）：\n{}",
                        config::JAVA_DOWNLOAD_URL
                    ),
                );
                nwg::stop_thread_dispatch();
            }
            FinishState::Error(ref error_text) => {
                self.progress_bar.set_pos(0);
                self.status_label
                    .set_text(&format!("更新失败: {error_text}"));
                self.hint_label.set_text("请截图联系管理员");
                show_error_log_dialog(&self.window, log_text.as_deref().unwrap_or(""));
                nwg::stop_thread_dispatch();
            }
            FinishState::ProxySuccess => {
                self.show_action_buttons(
                    "Discord 代理已启用",
                    Some("Xray 已在后台运行，Discord 已配置代理"),
                );
                self.btn_discord_proxy.set_text("停止代理");
            }
            FinishState::ProxyError(ref error_text) => {
                self.progress_bar.set_pos(0);
                self.status_label
                    .set_text(&format!("代理设置失败: {error_text}"));
                self.hint_label.set_text("请检查网络后重试");
                self.hint_label.set_visible(true);
                self.btn_launch_pcl.set_visible(true);
                self.btn_discord_proxy.set_visible(true);
                self.btn_settings.set_visible(true);
                if let Some(log) = log_text.as_deref() {
                    show_error_log_dialog(&self.window, log);
                }
            }
        }
    }

    /// 显示操作按钮，可选设置提示文本。
    fn show_action_buttons(&self, status: &str, hint: Option<&str>) {
        self.status_label.set_text(status);
        self.progress_bar.set_pos(100);
        if let Some(h) = hint {
            self.hint_label.set_text(h);
            self.hint_label.set_visible(true);
        } else {
            self.hint_label.set_visible(false);
        }
        self.btn_launch_pcl.set_visible(true);
        self.btn_discord_proxy.set_visible(true);
        self.btn_settings.set_visible(true);
    }

    /// 「启动 PCL」按钮点击
    fn on_launch_pcl(&self) {
        let base_dir = self.base_dir.borrow();
        let pcl2_path = base_dir.join(config::PCL2_EXE);

        if pcl2_path.exists() {
            if let Err(e) = std::process::Command::new(&pcl2_path)
                .current_dir(pcl2_path.parent().unwrap_or(&base_dir))
                .creation_flags(config::CREATE_NO_WINDOW)
                .spawn()
            {
                nwg::modal_info_message(&self.window, "错误", &format!("启动器启动失败: {e}"));
                return;
            }
        } else {
            nwg::modal_info_message(
                &self.window,
                "错误",
                &format!("找不到启动器: {}", pcl2_path.display()),
            );
            return;
        }

        nwg::stop_thread_dispatch();
    }

    /// 「启用 Discord 代理」/「停止代理」按钮点击
    fn on_enable_discord_proxy(&self) {
        let btn_text = self.btn_discord_proxy.text();
        let base_dir = self.base_dir.borrow().clone();

        // 如果当前是"停止代理"，同步执行停止操作
        if btn_text.contains("停止") {
            discord_proxy::stop(&base_dir);
            self.show_action_buttons("代理已停止", None);
            self.btn_discord_proxy.set_text("启用 Discord 代理");
            return;
        }

        // 否则执行启用/配置流程
        self.btn_launch_pcl.set_visible(false);
        self.btn_discord_proxy.set_visible(false);
        self.btn_settings.set_visible(false);
        self.hint_label.set_visible(false);
        self.progress_bar.set_pos(0);
        self.status_label.set_text("正在设置 Discord 代理...");

        {
            let mut s = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            s.log.clear();
            s.finish = None;
        }

        let state = Arc::clone(&self.shared_state);
        let notice_sender = self.progress_notice.sender();

        thread::spawn(move || {
            let mut guard = PanicGuard {
                state: Arc::clone(&state),
                sender: notice_sender,
                completed: false,
            };

            let result = discord_proxy::setup(&base_dir, &|progress: Progress| {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.log
                    .push(format!("[代理][{}%] {}", progress.percent, progress.message));
                s.progress = progress;
                drop(s);
                notice_sender.notice();
            });

            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.finish = Some(match result {
                Ok(()) => FinishState::ProxySuccess,
                Err(e) => {
                    let msg = format!("{e:#}");
                    s.log.push(format!("[代理][错误] {msg}"));
                    FinishState::ProxyError(msg)
                }
            });
            drop(s);
            notice_sender.notice();
            guard.completed = true;
        });
    }

    /// 「设置」按钮点击
    fn on_settings(&self) {
        let base_dir = self.base_dir.borrow().clone();
        show_settings_dialog(&self.window, &base_dir);
    }

    /// 窗口关闭事件
    fn on_close(&self) {
        nwg::stop_thread_dispatch();
    }
}

/// 弹出一个包含可复制日志文本的错误窗口。
fn show_error_log_dialog(parent: &nwg::Window, log_text: &str) {
    let mut window = Default::default();
    nwg::Window::builder()
        .title("错误日志")
        .size((620, 460))
        .position((200, 200))
        .center(true)
        .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
        .parent(Some(parent))
        .build(&mut window)
        .expect("创建错误日志窗口失败");

    let mut label = Default::default();
    nwg::Label::builder()
        .text("以下是完整日志（可全选复制）：")
        .size((560, 22))
        .position((20, 10))
        .parent(&window)
        .build(&mut label)
        .expect("创建标签失败");

    let mut text_box = Default::default();
    nwg::TextBox::builder()
        .text(log_text)
        .size((580, 330))
        .position((20, 38))
        .flags(
            nwg::TextBoxFlags::VISIBLE
                | nwg::TextBoxFlags::VSCROLL
                | nwg::TextBoxFlags::AUTOVSCROLL
                | nwg::TextBoxFlags::TAB_STOP,
        )
        .readonly(true)
        .parent(&window)
        .build(&mut text_box)
        .expect("创建文本框失败");

    if let Some(hwnd) = text_box.handle.hwnd() {
        use winapi::um::winuser::{EM_SCROLLCARET, EM_SETSEL, SendMessageW};
        unsafe {
            let end = -1isize;
            SendMessageW(hwnd, EM_SETSEL as u32, end as usize, end);
            SendMessageW(hwnd, EM_SCROLLCARET as u32, 0, 0);
        }
    }

    let mut copy_btn = Default::default();
    nwg::Button::builder()
        .text("复制日志")
        .size((100, 32))
        .position((380, 380))
        .parent(&window)
        .build(&mut copy_btn)
        .expect("创建按钮失败");

    let mut close_btn = Default::default();
    nwg::Button::builder()
        .text("关闭")
        .size((100, 32))
        .position((500, 380))
        .parent(&window)
        .build(&mut close_btn)
        .expect("创建按钮失败");

    let log_for_copy = log_text.to_string();

    let window_handle_clone = window.handle;
    let copy_btn_handle = copy_btn.handle;
    let close_btn_handle = close_btn.handle;

    let handler = nwg::full_bind_event_handler(
        &window_handle_clone,
        move |evt, _evt_data, handle| match evt {
            nwg::Event::OnButtonClick => {
                if handle == copy_btn_handle {
                    nwg::Clipboard::set_data_text(window_handle_clone, &log_for_copy);
                    let _ =
                        nwg::modal_info_message(window_handle_clone, "提示", "日志已复制到剪贴板");
                } else if handle == close_btn_handle {
                    nwg::stop_thread_dispatch();
                }
            }
            nwg::Event::OnWindowClose => {
                if handle == window_handle_clone {
                    nwg::stop_thread_dispatch();
                }
            }
            _ => {}
        },
    );

    nwg::dispatch_thread_events();
    nwg::unbind_event_handler(&handler);
}

/// 设置窗口：更新通道 + UDP 代理开关。
fn show_settings_dialog(parent: &nwg::Window, base_dir: &std::path::Path) {
    use crate::config::{
        ChannelConfig, UpdateChannel, UserSettings,
        load_user_settings, save_channel_config, save_user_settings,
    };

    let channel_cfg_path = base_dir.join(config::CHANNEL_CONFIG_FILE);
    let current_channel = std::fs::read_to_string(&channel_cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str::<ChannelConfig>(&s).ok())
        .unwrap_or_default()
        .channel;
    let current_settings = load_user_settings(base_dir);

    let mut window = Default::default();
    nwg::Window::builder()
        .title("设置")
        .size((340, 190))
        .center(true)
        .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
        .parent(Some(parent))
        .build(&mut window)
        .expect("创建设置窗口失败");

    // 更新通道
    let mut channel_label = Default::default();
    nwg::Label::builder()
        .text("更新通道:")
        .size((80, 22))
        .position((20, 22))
        .parent(&window)
        .build(&mut channel_label)
        .expect("label");

    let mut channel_combo = Default::default();
    nwg::ComboBox::builder()
        .size((170, 25))
        .position((105, 20))
        .collection(vec!["stable".to_string(), "dev".to_string()])
        .selected_index(Some(if current_channel == UpdateChannel::Dev { 1 } else { 0 }))
        .parent(&window)
        .build(&mut channel_combo)
        .expect("combo");

    // UDP 开关
    let mut udp_check = Default::default();
    nwg::CheckBox::builder()
        .text("代理 UDP 流量（Discord 语音走代理）")
        .size((300, 25))
        .position((20, 65))
        .check_state(if current_settings.proxy_udp {
            nwg::CheckBoxState::Checked
        } else {
            nwg::CheckBoxState::Unchecked
        })
        .parent(&window)
        .build(&mut udp_check)
        .expect("checkbox");

    // 保存按钮
    let mut save_btn = Default::default();
    nwg::Button::builder()
        .text("保存")
        .size((100, 35))
        .position((80, 110))
        .parent(&window)
        .build(&mut save_btn)
        .expect("button");

    // 取消按钮
    let mut cancel_btn = Default::default();
    nwg::Button::builder()
        .text("取消")
        .size((100, 35))
        .position((200, 110))
        .parent(&window)
        .build(&mut cancel_btn)
        .expect("button");

    let win_handle = window.handle;
    let save_handle = save_btn.handle;
    let cancel_handle = cancel_btn.handle;
    let base_dir = base_dir.to_path_buf();

    // 用 RefCell 包装控件以便在闭包中读取值
    let channel_combo = std::cell::RefCell::new(channel_combo);
    let udp_check = std::cell::RefCell::new(udp_check);

    let handler = nwg::full_bind_event_handler(&win_handle, move |evt, _, handle| match evt {
        nwg::Event::OnButtonClick => {
            if handle == save_handle {
                let channel = if channel_combo.borrow().selection() == Some(1) {
                    UpdateChannel::Dev
                } else {
                    UpdateChannel::Stable
                };
                let _ = save_channel_config(&base_dir, &ChannelConfig { channel });

                let udp = channel_combo.borrow(); // just to keep the borrow checker happy
                drop(udp);
                let udp = udp_check.borrow().check_state() == nwg::CheckBoxState::Checked;
                let _ = save_user_settings(&base_dir, &UserSettings { proxy_udp: udp });

                nwg::modal_info_message(win_handle, "提示", "设置已保存，下次启动时生效");
                nwg::stop_thread_dispatch();
            } else if handle == cancel_handle {
                nwg::stop_thread_dispatch();
            }
        }
        nwg::Event::OnWindowClose => {
            if handle == win_handle {
                nwg::stop_thread_dispatch();
            }
        }
        _ => {}
    });

    nwg::dispatch_thread_events();
    nwg::unbind_event_handler(&handler);
}
