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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::config;
use crate::update::{self, Progress, UpdateResult};

/// 共享的进度状态，后台线程写入，GUI 线程读取。
/// 用 Arc<Mutex<>> 实现线程安全。
#[derive(Debug, Clone, Default)]
struct SharedState {
    progress: Progress,
    finished: bool,
    error: Option<String>,
}

/// GUI 窗口定义
/// 使用 NwgUi 派生宏自动生成窗口绑定代码。
#[derive(Default, NwgUi)]
pub struct UpdaterApp {
    // ── 窗口 ──
    #[nwg_control(
        title: "",       // 运行时设置
        size: (420, 200),
        position: (300, 300),
        flags: "WINDOW|VISIBLE",
        // 居中显示
        center: true
    )]
    #[nwg_events(OnWindowClose: [UpdaterApp::on_close])]
    window: nwg::Window,

    // ── 标题标签 ──
    #[nwg_control(
        text: "",
        size: (380, 30),
        position: (20, 15),
        flags: "VISIBLE"
    )]
    #[nwg_layout_item(layout: layout, col: 0, row: 0)]
    title_label: nwg::Label,

    // ── 状态文本 ──
    #[nwg_control(
        text: "正在初始化...",
        size: (380, 25),
        position: (20, 60),
        flags: "VISIBLE"
    )]
    status_label: nwg::Label,

    // ── 进度条 ──
    #[nwg_control(
        size: (380, 25),
        position: (20, 95),
        range: 0..100,
        pos: 0
    )]
    progress_bar: nwg::ProgressBar,

    // ── 底部提示 ──
    #[nwg_control(
        text: "",
        size: (380, 20),
        position: (20, 135),
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
                finished: false,
                error: None,
            })),
            base_dir: RefCell::new(base_dir),
            ..Default::default()
        };

        // 构建 UI
        let app = UpdaterApp::build_ui(app).expect("构建 UI 失败");

        // 设置窗口标题
        app.window.set_text(config::WINDOW_TITLE);
        app.title_label.set_text(config::WINDOW_TITLE);
        app.hint_label.set_text("请勿关闭此窗口...");

        // 启动后台更新线程
        let state = Arc::clone(&app.shared_state);
        let notice_sender = app.progress_notice.sender();
        let base_dir = app.base_dir.borrow().clone();

        thread::spawn(move || {
            // 执行更新，通过回调报告进度
            let result = update::run_update(&base_dir, &|progress: Progress| {
                let mut s = state.lock().unwrap();
                s.progress = progress;
                // 通知 GUI 线程刷新
                notice_sender.notice();
            });

            // 更新完成，标记状态
            let mut s = state.lock().unwrap();
            match result {
                Ok(UpdateResult::Success) | Ok(UpdateResult::Offline) => {
                    s.finished = true;
                }
                Err(e) => {
                    s.error = Some(format!("{:#}", e));
                    s.finished = true;
                }
            }
            notice_sender.notice();
        });

        // 运行 GUI 事件循环（阻塞直到窗口关闭）
        nwg::dispatch_thread_events();
    }

    /// 后台线程发来进度通知时调用
    fn on_progress_update(&self) {
        let state = self.shared_state.lock().unwrap();

        // 更新进度条和状态文本
        self.progress_bar.set_pos(state.progress.percent);
        self.status_label.set_text(&state.progress.message);

        if state.finished {
            if let Some(ref error) = state.error {
                // 出错了：显示错误信息
                self.status_label
                    .set_text(&format!("更新失败: {}", error));
                self.hint_label.set_text("请截图联系管理员");
                self.progress_bar.set_pos(0);
            } else {
                // 成功：启动延迟定时器，1.5秒后打开 PCL2
                self.hint_label.set_text("即将启动游戏...");
                self.launch_timer.start();
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
            let _ = std::process::Command::new(&pcl2_path)
                .current_dir(pcl2_path.parent().unwrap_or(&base_dir))
                .spawn();
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
