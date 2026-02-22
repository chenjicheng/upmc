// ============================================================
// gui.rs — 原生 Windows GUI 窗口
// ============================================================
// 使用 native-windows-gui (nwg) 创建一个小窗口，包含：
//   - 标题文字
//   - 状态文本 (显示当前操作)
//   - 进度条
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

use crate::config;
use crate::update::{self, Progress, UpdateResult};

/// 更新完成后的结果状态。
///
/// 用枚举而非多个 bool 标记，编译器会在 match 时强制穷举检查，
/// 新增状态不会被遗漏。
#[derive(Debug, Clone)]
enum FinishState {
    /// 更新成功，准备启动 PCL2
    Success,
    /// 更新器已自更新并重启新进程，当前进程仅需退出
    SelfUpdateRestarting,
    /// Java 未安装，显示友好安装指引
    JavaNotFound,
    /// 其他错误，显示技术日志
    Error(String),
}

/// 共享的进度状态，后台线程写入，GUI 线程读取。
/// 用 Arc<Mutex<>> 实现线程安全。
#[derive(Debug, Clone, Default)]
struct SharedState {
    progress: Progress,
    /// 完整日志记录，每一步都追加
    log: Vec<String>,
    /// 更新完成后的状态，None 表示尚未完成
    finish: Option<FinishState>,
}

/// GUI 窗口定义
/// 使用 NwgUi 派生宏自动生成窗口绑定代码。
#[derive(Default, NwgUi)]
pub struct UpdaterApp {
    // ── 窗口 ──
    #[nwg_control(
        title: "",       // 运行时设置
        size: (420, 165),
        position: (300, 300),
        flags: "WINDOW|VISIBLE",
        // 居中显示
        center: true
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

    // ── 布局管理器 (简单网格) ──
    #[nwg_layout(parent: window, spacing: 1, max_row: Some(5), max_column: Some(1))]
    layout: nwg::GridLayout,

    // ── 用于后台线程通知 GUI 更新的 Notice ──
    #[nwg_control]
    #[nwg_events(OnNotice: [UpdaterApp::on_progress_update])]
    progress_notice: nwg::Notice,

    // ── 定时器：更新完成后延迟启动 PCL2 ──
    #[nwg_control(interval: std::time::Duration::from_millis(1500), active: false)]
    #[nwg_events(OnTimerTick: [UpdaterApp::on_launch_timer])]
    launch_timer: nwg::AnimationTimer,

    // ── 内部状态 ──
    /// 共享进度（后台线程写，GUI 读）
    shared_state: Arc<Mutex<SharedState>>,

    /// exe 所在的根目录
    base_dir: RefCell<PathBuf>,
}

impl UpdaterApp {
    /// 启动更新器 GUI。这是外部调用的唯一入口。
    pub fn run(base_dir: PathBuf) {
        // 初始化 nwg
        nwg::init().expect("初始化 Windows GUI 失败");

        // 设置默认字体（微软雅黑，适合中文显示）
        nwg::Font::set_global_family("Microsoft YaHei UI").expect("设置字体失败");

        // 创建应用实例
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

        // 构建 UI
        let app = UpdaterApp::build_ui(app).expect("构建 UI 失败");

        // 设置窗口标题
        let title = config::window_title();
        app.window.set_text(&title);
        app.hint_label.set_text("请勿关闭此窗口...");

        // 启动后台更新线程
        let state = Arc::clone(&app.shared_state);
        let notice_sender = app.progress_notice.sender();
        let base_dir = app.base_dir.borrow().clone();

        thread::spawn(move || {
            // RAII guard：确保无论正常返回还是 panic，都发送 finish 通知。
            // 如果后台线程 panic 且未设置 finish，
            // guard 的 drop 会写入 Error 状态并通知 GUI，避免窗口永久挂起。
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
                            s.log.push("[错误] 更新器内部错误（线程异常退出）".to_string());
                            s.finish = Some(FinishState::Error(
                                "更新器内部错误（线程异常退出）".to_string(),
                            ));
                        }
                        drop(s);
                        self.sender.notice();
                    }
                }
            }

            let mut guard = PanicGuard {
                state: Arc::clone(&state),
                sender: notice_sender,
                completed: false,
            };

            // 执行更新，通过回调报告进度
            let result = update::run_update(&base_dir, &|progress: Progress| {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                // 记录日志
                s.log.push(format!("[{}%] {}", progress.percent, progress.message));
                s.progress = progress;
                drop(s); // 先释放锁再通知
                // 通知 GUI 线程刷新
                notice_sender.notice();
            });

            // 更新完成，标记状态
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.finish = Some(match result {
                Ok(UpdateResult::SelfUpdateRestarting) => {
                    s.log.push("[重启] 更新器已更新，正在重启...".to_string());
                    FinishState::SelfUpdateRestarting
                }
                Ok(UpdateResult::Success | UpdateResult::Offline) => {
                    s.log.push("[完成] 更新成功".to_string());
                    FinishState::Success
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
            drop(s); // 先释放锁再通知，避免 GUI 线程等锁
            notice_sender.notice();
            guard.completed = true;
        });

        // 运行 GUI 事件循环（阻塞直到窗口关闭）
        nwg::dispatch_thread_events();
    }

    /// 后台线程发来进度通知时调用
    fn on_progress_update(&self) {
        // 使用 lock + unwrap_or_else 处理 mutex poisoning，
        // 后台线程 panic 时仍然能拿到锁内数据。
        //
        // 先复制所有需要的数据再释放锁，最小化临界区。
        let (percent, message, finish, log_text) = {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            let percent = state.progress.percent;
            let message = state.progress.message.clone();
            // 用 .take() 取出并置 None，防止多次 notice 导致重复处理
            let finish = state.finish.take();
            // 仅在需要日志的分支提取，避免不必要的堆分配
            let log_text = if matches!(finish, Some(FinishState::Error(_))) {
                Some(state.log.join("\r\n"))
            } else {
                None
            };
            (percent, message, finish, log_text)
        }; // 锁在此处释放

        // 更新进度条和状态文本
        self.progress_bar.set_pos(percent);
        self.status_label.set_text(&message);

        let finish = match finish {
            Some(f) => f,
            None => return, // 尚未完成，仅刷新进度
        };

        match finish {
            FinishState::Success => {
                // 成功：启动延迟定时器，1.5秒后打开 PCL2
                self.hint_label.set_text("即将启动游戏...");
                self.launch_timer.start();
            }
            FinishState::SelfUpdateRestarting => {
                // 更新器已自更新并重启新进程，直接关闭窗口
                nwg::stop_thread_dispatch();
            }
            FinishState::JavaNotFound => {
                // Java 未安装：显示友好提示（下载页已尝试自动打开）
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
                // 其他错误：显示错误摘要和可复制的日志窗口
                self.progress_bar.set_pos(0);
                self.status_label
                    .set_text(&format!("更新失败: {error_text}"));
                self.hint_label.set_text("请截图联系管理员");
                show_error_log_dialog(&self.window, log_text.as_deref().unwrap_or(""));
                nwg::stop_thread_dispatch();
            }
        }
    }

    /// 延迟定时器触发：启动 PCL2 并关闭更新器
    fn on_launch_timer(&self) {
        self.launch_timer.stop();

        let base_dir = self.base_dir.borrow();
        let pcl2_path = base_dir.join(config::PCL2_EXE);

        if pcl2_path.exists() {
            // 启动 PCL2（不等待它退出）
            if let Err(e) = std::process::Command::new(&pcl2_path)
                .current_dir(pcl2_path.parent().unwrap_or(&base_dir))
                .creation_flags(config::CREATE_NO_WINDOW)
                .spawn()
            {
                nwg::modal_info_message(
                    &self.window,
                    "错误",
                    &format!("启动器启动失败: {e}"),
                );
            }
        } else {
            nwg::modal_info_message(
                &self.window,
                "错误",
                &format!("找不到启动器: {}", pcl2_path.display()),
            );
        }

        // 关闭更新器窗口
        nwg::stop_thread_dispatch();
    }

    /// 窗口关闭事件
    fn on_close(&self) {
        nwg::stop_thread_dispatch();
    }
}

/// 弹出一个包含可复制日志文本的错误窗口。
///
/// 使用原生 Win32 窗口，包含：
///   - 只读多行 TextBox（可以全选复制）
///   - "复制日志" 按钮
///   - "关闭" 按钮
fn show_error_log_dialog(parent: &nwg::Window, log_text: &str) {
    // 构建窗口
    let mut window = Default::default();
    nwg::Window::builder()
        .title("更新失败 — 错误日志")
        .size((600, 420))
        .position((200, 200))
        .center(true)
        .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
        .parent(Some(parent))
        .build(&mut window)
        .expect("创建错误日志窗口失败");

    // 提示标签
    let mut label = Default::default();
    nwg::Label::builder()
        .text("更新过程中发生错误，以下是完整日志（可全选复制）：")
        .size((560, 22))
        .position((20, 10))
        .parent(&window)
        .build(&mut label)
        .expect("创建标签失败");

    // 多行文本框（只读，可复制）
    let mut text_box = Default::default();
    nwg::TextBox::builder()
        .text(log_text)
        .size((560, 300))
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

    // "复制日志" 按钮
    let mut copy_btn = Default::default();
    nwg::Button::builder()
        .text("复制日志")
        .size((100, 32))
        .position((360, 350))
        .parent(&window)
        .build(&mut copy_btn)
        .expect("创建按钮失败");

    // "关闭" 按钮
    let mut close_btn = Default::default();
    nwg::Button::builder()
        .text("关闭")
        .size((100, 32))
        .position((480, 350))
        .parent(&window)
        .build(&mut close_btn)
        .expect("创建按钮失败");

    // 保存日志文本用于复制
    let log_for_copy = log_text.to_string();

    // 事件处理
    let window_handle_clone = window.handle;
    let copy_btn_handle = copy_btn.handle;
    let close_btn_handle = close_btn.handle;

    let handler = nwg::full_bind_event_handler(&window_handle_clone, move |evt, _evt_data, handle| {
        match evt {
            nwg::Event::OnButtonClick => {
                if handle == copy_btn_handle {
                    // 复制到剪贴板
                    nwg::Clipboard::set_data_text(window_handle_clone, &log_for_copy);
                    let _ = nwg::modal_info_message(window_handle_clone, "提示", "日志已复制到剪贴板");
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
        }
    });

    nwg::dispatch_thread_events();
    nwg::unbind_event_handler(&handler);
}
