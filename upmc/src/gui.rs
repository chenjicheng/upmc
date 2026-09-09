use crate::{
    config::{self, ChannelConfig, UpdateChannel},
    discord_proxy,
    gui_state::{Job, Outcome, UiState},
    gui_switches::{Control, SwitchQueue},
    update::{self, Progress, UpdateResult},
    version,
};
use anyhow::{Context, Result};
use slint::ComponentHandle;
use std::os::windows::process::CommandExt;
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{self, SyncSender},
    time::Duration,
};
slint::include_modules!();
pub struct UpdaterApp;
impl UpdaterApp {
    pub fn run(base_dir: PathBuf, channel: ChannelConfig) {
        if let Err(error) = run(base_dir, channel) {
            crate::observability::event(
                "startup.fatal",
                "Slint UI failed",
                format!("{error:#}"),
                "GUI",
                "exit with failure",
                std::process::id(),
            );
            eprintln!("界面启动失败：{error:#}");
            std::process::exit(1);
        }
    }
}

fn render(ui: &App, state: &UiState) {
    render_at(
        ui,
        state,
        state
            .started
            .map_or(Duration::ZERO, |start| start.elapsed()),
    );
}
fn render_at(ui: &App, state: &UiState, elapsed: Duration) {
    let busy = state.busy.is_some();
    ui.set_busy(busy);
    let main_job = matches!(state.busy, Some(Job::Update | Job::Launch));
    ui.set_switches_enabled(!main_job && !state.exit);
    let main_busy = main_job && elapsed >= Duration::from_millis(300);
    ui.set_main_busy(main_busy);
    ui.set_updating(main_busy && state.busy == Some(Job::Update));
    ui.set_launchable(state.ready && !busy);
    ui.set_proxy_on(state.proxy);
    ui.set_proxy_text(
        if state.proxy {
            "已启用"
        } else {
            "未启用"
        }
        .into(),
    );
    ui.set_status(if main_job && !main_busy {
        if state.previous_status.is_empty() {
            "准备整合包".into()
        } else {
            state.previous_status.clone().into()
        }
    } else {
        state.status.clone().into()
    });
    let main_error = matches!(state.error_job(), Some(Job::Update | Job::Launch));
    ui.set_proxy_error(matches!(
        state.error_job(),
        Some(Job::ProxyStart | Job::ProxyStop)
    ));
    ui.set_settings_error(
        if matches!(
            state.error_job(),
            Some(Job::UdpSettings | Job::ChannelSettings | Job::WindowSettings)
        ) {
            state.error.clone().into()
        } else {
            "".into()
        },
    );
    ui.set_scenario(if main_error { 5 } else { 0 });
    ui.set_detail(
        if main_busy {
            state.progress_detail.clone()
        } else {
            String::new()
        }
        .into(),
    );
    ui.set_has_error(main_error);
    ui.set_progress(state.percent.min(100) as i32);
    ui.set_action_text(
        if main_busy {
            "请稍候…"
        } else if state.ready
            || (main_job && (state.previous_ready || state.previous_status.is_empty()))
        {
            "启动 PCL"
        } else {
            "重试更新"
        }
        .into(),
    );
}

fn save_udp(base: &Path, enabled: bool) -> Result<()> {
    let mut settings = config::load_user_settings(base);
    settings.proxy_udp = enabled;
    config::save_user_settings(base, &settings).context("保存 UDP 设置失败")
}

enum Request {
    Update,
    Proxy(bool),
    Udp(bool),
    Channel(UpdateChannel),
    HideAfterLaunch(bool),
    Launch,
}
impl Request {
    fn job(&self) -> Job {
        match self {
            Self::Update => Job::Update,
            Self::Proxy(true) => Job::ProxyStart,
            Self::Proxy(false) => Job::ProxyStop,
            Self::Udp(_) => Job::UdpSettings,
            Self::Channel(_) => Job::ChannelSettings,
            Self::HideAfterLaunch(_) => Job::WindowSettings,
            Self::Launch => Job::Launch,
        }
    }
}
enum Event {
    Progress(Progress),
    Finished(Outcome),
}
type Executor = fn(&Path, &ChannelConfig, Request, &dyn Fn(Progress)) -> Result<Outcome>;
struct Controller {
    ui: slint::Weak<App>,
    base: PathBuf,
    channel: RefCell<ChannelConfig>,
    udp: Cell<bool>,
    hide_after_launch: Cell<bool>,
    hide_window: Box<dyn Fn(&App) -> Result<()>>,
    state: RefCell<UiState>,
    log: RefCell<Vec<String>>,
    switches: RefCell<SwitchQueue>,
    errors: RefCell<Vec<String>>,
    error_window: RefCell<Option<slint::Weak<LogWindow>>>,
    sender: SyncSender<Event>,
    executor: Executor,
}
const BUSY_CLOSE_FEEDBACK: &str = "任务进行中，请等待完成后关闭。";

impl Controller {
    fn submit_switch(&self, control: Control, enabled: bool) {
        if matches!(self.state.borrow().busy, Some(Job::Update | Job::Launch))
            || self.state.borrow().exit
        {
            return;
        }
        self.switches.borrow_mut().request(control, enabled);
        self.repaint();
        self.start_queued();
    }
    fn start_queued(&self) {
        if self.state.borrow().busy.is_some() || self.state.borrow().exit {
            return;
        }
        let next = self.switches.borrow_mut().take_next();
        if let Some(change) = next {
            self.start(match change.control {
                Control::Proxy => Request::Proxy(change.enabled),
                Control::Udp => Request::Udp(change.enabled),
                Control::HideAfterLaunch => Request::HideAfterLaunch(change.enabled),
                Control::Channel => Request::Channel(if change.enabled {
                    UpdateChannel::Dev
                } else {
                    UpdateChannel::Stable
                }),
            });
        }
    }
    fn paint_switches(&self, ui: &App) {
        let switches = self.switches.borrow();
        let proxy = switches.value(Control::Proxy, self.state.borrow().proxy);
        ui.set_proxy_on(proxy);
        ui.set_proxy_text(if proxy { "已启用" } else { "未启用" }.into());
        ui.set_udp_enabled(switches.value(Control::Udp, self.udp.get()));
        ui.set_hide_after_launch(
            switches.value(Control::HideAfterLaunch, self.hide_after_launch.get()),
        );
        ui.set_dev_channel(switches.value(
            Control::Channel,
            self.channel.borrow().channel == UpdateChannel::Dev,
        ));
    }
    fn record_error(&self, error: String) {
        let recent_steps = self.log.borrow().join("\n");
        let context = match self.state.borrow().busy {
            Some(Job::Update) => "更新整合包",
            Some(Job::ProxyStart) => "启用 Discord 代理",
            Some(Job::ProxyStop) => "停用 Discord 代理",
            Some(Job::UdpSettings) => "保存 UDP 设置",
            Some(Job::ChannelSettings) => "保存更新通道",
            Some(Job::WindowSettings) => "保存窗口行为",
            Some(Job::Launch) => "启动 PCL",
            None => "界面操作",
        };
        let text = {
            let mut errors = self.errors.borrow_mut();
            let number = errors.len() + 1;
            let mut record = format!("[{number}] {context}\n{error}");
            if !recent_steps.is_empty() {
                record.push_str(&format!("\n\n执行记录：\n{recent_steps}"));
            }
            errors.push(record);
            errors.join("\n\n────────────────────────\n\n")
        };
        if let Some(window) = self
            .error_window
            .borrow()
            .as_ref()
            .and_then(slint::Weak::upgrade)
            && window.get_heading() == "错误详情"
        {
            window.set_log_text(text.into());
        }
    }
    fn repaint(&self) {
        if let Some(ui) = self.ui.upgrade() {
            render(&ui, &self.state.borrow());
            self.paint_switches(&ui);
        }
    }
    fn refresh(&self) {
        if let Some(ui) = self.ui.upgrade() {
            if self.state.borrow().busy.is_none() && ui.get_feedback() == BUSY_CLOSE_FEEDBACK {
                ui.set_feedback("".into());
            }
            render(&ui, &self.state.borrow());
            let local = version::read_local_version(&self.base);
            ui.set_pack_metadata(if local.mc_version.is_empty() {
                "尚未安装整合包".into()
            } else {
                format!(
                    "Minecraft {} · Fabric {}",
                    local.mc_version, local.fabric_version
                )
                .into()
            });
            ui.set_udp_enabled(self.udp.get());
            ui.set_dev_channel(self.channel.borrow().channel == UpdateChannel::Dev);
            ui.set_window_title(config::window_title(self.channel.borrow().channel).into());
            self.paint_switches(&ui);
        }
    }
    fn start(&self, request: Request) {
        if !self.state.borrow_mut().begin(request.job()) {
            return;
        }
        self.log.borrow_mut().clear();
        self.repaint();
        let base = self.base.clone();
        let channel = self.channel.borrow().clone();
        let sender = self.sender.clone();
        let executor = self.executor;
        let spawn = std::thread::Builder::new()
            .name("upmc-worker".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    executor(&base, &channel, request, &|p| {
                        let _ = sender.send(Event::Progress(p));
                    })
                }));
                let outcome = match result {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => Outcome::Failed(format!("{error:#}")),
                    Err(_) => Outcome::Failed("后台任务异常退出，请查看日志后重试".into()),
                };
                let _ = sender.send(Event::Finished(outcome));
            });
        if let Err(error) = spawn {
            self.record_error(format!("无法启动后台任务：{error}"));
            self.state
                .borrow_mut()
                .fail_before_start(format!("无法启动后台任务：{error}"));
            self.switches.borrow_mut().complete(false);
            self.start_queued();
            self.refresh();
        }
    }
    fn event(&self, event: Event) {
        match event {
            Event::Progress(p) => {
                let mut state = self.state.borrow_mut();
                state.percent = p.percent.min(100);
                self.append_log(format!("[{}%] {}", state.percent, p.message));
                state.progress_detail = p.message;
                drop(state);
                self.repaint();
            }
            Event::Finished(outcome) => {
                let launched = matches!(outcome, Outcome::Launched);
                if matches!(outcome, Outcome::Saved)
                    && let Some(change) = self.switches.borrow().active()
                {
                    match change.control {
                        Control::Channel => {
                            self.channel.borrow_mut().channel = if change.enabled {
                                UpdateChannel::Dev
                            } else {
                                UpdateChannel::Stable
                            }
                        }
                        Control::Udp => self.udp.set(change.enabled),
                        Control::HideAfterLaunch => self.hide_after_launch.set(change.enabled),
                        Control::Proxy => {}
                    }
                }
                if let Outcome::Failed(error) = &outcome {
                    self.record_error(error.clone());
                }
                let success = !matches!(outcome, Outcome::Failed(_));
                self.state.borrow_mut().finish(outcome);
                self.switches.borrow_mut().complete(success);
                self.start_queued();
                self.refresh();
                if launched && self.hide_after_launch.get() {
                    if let Some(ui) = self.ui.upgrade() {
                        if let Err(error) = (self.hide_window)(&ui) {
                            let message = format!("隐藏窗口失败：{error:#}");
                            self.record_error(message.clone());
                            self.state.borrow_mut().launch_window_error(message);
                            self.refresh();
                        }
                    }
                }
                if self.state.borrow().exit {
                    let _ = slint::quit_event_loop();
                }
            }
        }
    }
    fn append_log(&self, line: String) {
        let mut log = self.log.borrow_mut();
        if log.len() >= 500 {
            log.remove(0);
        }
        log.push(line);
    }
    fn request_close(&self) -> bool {
        if self.state.borrow().busy.is_some() {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_feedback(BUSY_CLOSE_FEEDBACK.into());
            }
            false
        } else {
            true
        }
    }
    fn restore_window(&self, show: impl FnOnce(&App) -> Result<()>) {
        use slint::winit_030::WinitWindowAccessor;
        if let Some(ui) = self.ui.upgrade() {
            if let Err(error) = show(&ui) {
                let message = format!("恢复窗口失败：{error:#}");
                self.record_error(message.clone());
                self.state.borrow_mut().launch_window_error(message);
                self.refresh();
                return;
            }
            ui.window().set_minimized(false);
            ui.window()
                .with_winit_window(|window| window.focus_window());
        }
    }
}
fn native_close(controller: &Controller, quit: impl FnOnce()) -> slint::CloseRequestResponse {
    if controller.request_close() {
        quit();
        slint::CloseRequestResponse::HideWindow
    } else {
        slint::CloseRequestResponse::KeepWindowShown
    }
}
fn execute(
    base: &Path,
    channel: &ChannelConfig,
    request: Request,
    progress: &dyn Fn(Progress),
) -> Result<Outcome> {
    Ok(match request {
        Request::Update => match update::run_update(base, channel, progress)? {
            UpdateResult::Success { proxy_running } => Outcome::Updated(proxy_running),
            UpdateResult::Offline => Outcome::Offline,
            UpdateResult::SelfUpdateRestarting => Outcome::Restarting,
        },
        Request::Proxy(true) => {
            discord_proxy::setup(base, progress)?;
            Outcome::ProxyStarted
        }
        Request::Proxy(false) => {
            discord_proxy::stop(base)?;
            Outcome::ProxyStopped
        }
        Request::Udp(value) => {
            save_udp(base, value)?;
            Outcome::Saved
        }
        Request::Channel(value) => {
            config::save_channel_config(base, &ChannelConfig { channel: value })?;
            Outcome::Saved
        }
        Request::HideAfterLaunch(value) => {
            let mut settings = config::load_user_settings(base);
            settings.hide_after_launch = value;
            config::save_user_settings(base, &settings).context("保存窗口行为失败")?;
            Outcome::Saved
        }
        Request::Launch => {
            let launcher = base.join(config::PCL2_EXE);
            anyhow::ensure!(launcher.is_file(), "找不到启动器：{}", launcher.display());
            std::process::Command::new(launcher)
                .current_dir(base)
                .creation_flags(config::CREATE_NO_WINDOW)
                .spawn()
                .context("启动 PCL 失败")?;
            Outcome::Launched
        }
    })
}
fn run(base: PathBuf, channel: ChannelConfig) -> Result<()> {
    run_with_executor(base, channel, execute)
}
fn run_with_executor(base: PathBuf, channel: ChannelConfig, executor: Executor) -> Result<()> {
    use slint::winit_030::{
        WinitWindowAccessor,
        winit::platform::windows::{CornerPreference, WindowAttributesExtWindows},
    };
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(|a| a.with_corner_preference(CornerPreference::Round))
        .select()?;
    let ui = App::new()?;
    ui.set_window_title(config::window_title(channel.channel).into());
    ui.set_pack_name(config::INSTALL_DIR_NAME.into());
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    let (sender, receiver) = mpsc::sync_channel(64);
    let initial_settings = config::load_user_settings(&base);
    let tray = Rc::new(RefCell::new(None));
    let hide_tray = tray.clone();
    let controller = Rc::new(Controller {
        ui: ui.as_weak(),
        base,
        channel: RefCell::new(channel),
        udp: Cell::new(initial_settings.proxy_udp),
        hide_after_launch: Cell::new(initial_settings.hide_after_launch),
        hide_window: Box::new(move |ui| {
            if hide_tray.borrow().is_none() {
                let icon = tray_icon::Icon::from_resource(1, Some((32, 32)))?;
                *hide_tray.borrow_mut() = Some(
                    tray_icon::TrayIconBuilder::new()
                        .with_icon(icon)
                        .with_tooltip("UPMC · 点击显示窗口")
                        .build()?,
                );
            }
            ui.hide()?;
            Ok(())
        }),
        state: RefCell::new(UiState::default()),
        log: RefCell::new(Vec::new()),
        switches: RefCell::new(SwitchQueue::default()),
        errors: RefCell::new(Vec::new()),
        error_window: RefCell::new(None),
        sender,
        executor,
    });
    controller.refresh();
    let c = controller.clone();
    ui.on_primary_action(move || {
        let ready = c.state.borrow().ready;
        c.start(if ready {
            Request::Launch
        } else {
            Request::Update
        });
    });
    let c = controller.clone();
    ui.on_check_update(move || c.start(Request::Update));
    let c = controller.clone();
    ui.on_proxy_toggle(move || {
        let enable = !c
            .switches
            .borrow()
            .value(Control::Proxy, c.state.borrow().proxy);
        c.submit_switch(Control::Proxy, enable);
    });
    let c = controller.clone();
    ui.on_udp_change(move |value| c.submit_switch(Control::Udp, value));
    let c = controller.clone();
    ui.on_hide_after_launch_change(move |value| c.submit_switch(Control::HideAfterLaunch, value));
    let c = controller.clone();
    ui.on_channel_change(move |value| {
        c.submit_switch(Control::Channel, value);
    });
    let c = controller.clone();
    ui.on_close_window(move || {
        if c.request_close() {
            let _ = slint::quit_event_loop();
        }
    });
    let c = controller.clone();
    ui.window().on_close_requested(move || {
        native_close(&c, || {
            let _ = slint::quit_event_loop();
        })
    });
    let weak = ui.as_weak();
    ui.on_minimize_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });
    let c = controller.clone();
    ui.on_drag_window(move || {
        if let Some(ui) = c.ui.upgrade() {
            match ui.window().with_winit_window(|w| w.drag_window()) {
                Some(Ok(())) => {}
                other => {
                    c.record_error(format!("窗口拖动失败：{other:?}"));
                    ui.set_feedback("窗口操作失败".into());
                    ui.set_has_error(true);
                }
            }
        }
    });
    let logs = LogWindow::new()?;
    *controller.error_window.borrow_mut() = Some(logs.as_weak());
    let c = controller.clone();
    let weak_logs = logs.as_weak();
    ui.on_show_logs(move || {
        if let Some(logs) = weak_logs.upgrade() {
            logs.set_heading("错误详情".into());
            logs.set_log_text(
                c.errors
                    .borrow()
                    .join("\n\n────────────────────────\n\n")
                    .into(),
            );
            if let Err(error) = logs.show() {
                c.record_error(format!("打开错误详情失败：{error}"));
                if let Some(ui) = c.ui.upgrade() {
                    ui.set_feedback("无法打开错误详情".into());
                }
            }
        }
    });
    let timer = slint::Timer::default();
    let weak_logs = logs.as_weak();
    let c = controller.clone();
    ui.on_show_licenses(move || {
        if let Some(logs) = weak_logs.upgrade() {
            logs.set_heading("开源许可".into());
            logs.set_log_text(include_str!("../assets/third-party-notices.txt").into());
            if let Err(error) = logs.show() {
                c.record_error(format!("打开开源许可失败：{error}"));
                if let Some(ui) = c.ui.upgrade() {
                    ui.set_feedback("无法打开开源许可".into());
                    ui.set_has_error(true);
                }
            }
        }
    });
    let c = controller.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(60),
        move || {
            for event in tray_icon::TrayIconEvent::receiver().try_iter().take(16) {
                if matches!(
                    event,
                    tray_icon::TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    }
                ) {
                    c.restore_window(|ui| ui.show().map_err(Into::into));
                }
            }
            for event in receiver.try_iter().take(128) {
                c.event(event);
            }
            if c.state.borrow().busy.is_some() {
                c.repaint();
            }
        },
    );
    controller.start(Request::Update);
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_state::{Job, Outcome};
    fn launch_controller(ui: &App, base: &Path, sender: SyncSender<Event>) -> Controller {
        Controller {
            ui: ui.as_weak(),
            base: base.to_owned(),
            channel: RefCell::new(ChannelConfig::default()),
            udp: Cell::new(true),
            hide_after_launch: Cell::new(false),
            hide_window: Box::new(|ui| ui.hide().map_err(Into::into)),
            state: RefCell::new(UiState::default()),
            log: RefCell::new(Vec::new()),
            switches: RefCell::new(SwitchQueue::default()),
            errors: RefCell::new(Vec::new()),
            error_window: RefCell::new(None),
            sender,
            executor: execute,
        }
    }
    #[test]
    fn launch_visibility_and_hide_failure_keep_updater_usable() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (sender, _) = mpsc::sync_channel(64);
        let mut c = launch_controller(&ui, temp.path(), sender);
        let hides = Rc::new(Cell::new(0));
        let calls = hides.clone();
        c.hide_window = Box::new(move |_| {
            calls.set(calls.get() + 1);
            Ok(())
        });
        c.state.borrow_mut().ready = true;
        c.state.borrow_mut().begin(Job::Launch);
        c.event(Event::Finished(Outcome::Launched));
        assert_eq!(hides.get(), 0);
        assert!(!c.state.borrow().exit);
        c.hide_after_launch.set(true);
        c.state.borrow_mut().begin(Job::Launch);
        c.event(Event::Finished(Outcome::Failed("spawn denied".into())));
        assert_eq!(hides.get(), 0);
        c.state.borrow_mut().begin(Job::Launch);
        c.event(Event::Finished(Outcome::Launched));
        assert_eq!(hides.get(), 1);
        c.hide_window = Box::new(|_| anyhow::bail!("tray creation denied"));
        c.state.borrow_mut().begin(Job::Launch);
        c.event(Event::Finished(Outcome::Launched));
        assert!(c.state.borrow().ready);
        assert!(!c.state.borrow().exit);
        assert!(ui.get_has_error());
        assert!(
            c.errors
                .borrow()
                .last()
                .unwrap()
                .contains("tray creation denied")
        );
        assert!(c.request_close());
    }
    #[test]
    fn launch_setting_is_optimistic_serial_and_rolls_back_storage_failure() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::sync_channel(64);
        let c = launch_controller(&ui, temp.path(), sender);
        c.submit_switch(Control::HideAfterLaunch, true);
        assert!(ui.get_hide_after_launch());
        c.submit_switch(Control::HideAfterLaunch, false);
        assert!(!ui.get_hide_after_launch());
        c.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(!ui.get_hide_after_launch());
        c.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(!config::load_user_settings(temp.path()).hide_after_launch);
        std::fs::remove_file(temp.path().join(config::USER_SETTINGS_FILE)).unwrap();
        std::fs::create_dir(temp.path().join(config::USER_SETTINGS_FILE)).unwrap();
        c.submit_switch(Control::HideAfterLaunch, true);
        assert!(ui.get_hide_after_launch());
        c.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(!ui.get_hide_after_launch());
        assert!(
            c.errors
                .borrow()
                .last()
                .unwrap()
                .contains("保存窗口行为失败")
        );
    }
    #[test]
    fn native_close_quits_persistent_event_loop_only_when_idle() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (sender, _) = mpsc::sync_channel(64);
        let c = launch_controller(&ui, temp.path(), sender);
        let quits = Cell::new(0);
        native_close(&c, || quits.set(quits.get() + 1));
        assert_eq!(
            quits.get(),
            1,
            "native close must quit while hide-to-tray keeps the loop alive"
        );
        c.state.borrow_mut().begin(Job::Update);
        native_close(&c, || quits.set(quits.get() + 1));
        assert_eq!(quits.get(), 1, "busy close must leave the job running");
    }
    #[test]
    fn tray_restore_failure_is_visible_in_shared_error_surface_and_retry_works() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (sender, _) = mpsc::sync_channel(64);
        let c = launch_controller(&ui, temp.path(), sender);
        c.restore_window(|_| anyhow::bail!("window unavailable"));
        assert!(
            ui.get_has_error(),
            "restoration failure needs an error details affordance"
        );
        assert!(
            c.errors
                .borrow()
                .last()
                .unwrap()
                .contains("window unavailable")
        );
        c.restore_window(|ui| ui.show().map_err(Into::into));
        assert!(ui.window().is_visible());
        assert!(!c.state.borrow().exit);
    }
    #[test]
    fn another_setting_does_not_hide_a_failed_setting() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let mut state = UiState::default();
        state.begin(Request::Udp(true).job());
        state.finish(Outcome::Failed("UDP save denied".into()));
        state.begin(Request::Channel(UpdateChannel::Dev).job());
        render(&ui, &state);
        assert_eq!(ui.get_settings_error(), "UDP save denied");
        state.finish(Outcome::Saved);
        render(&ui, &state);
        assert_eq!(ui.get_settings_error(), "UDP save denied");
        state.begin(Request::Udp(true).job());
        render(&ui, &state);
        assert!(ui.get_settings_error().is_empty());
    }
    #[test]
    fn close_warning_ends_with_job_but_unrelated_feedback_remains() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (sender, _) = mpsc::sync_channel(64);
        let c = Controller {
            ui: ui.as_weak(),
            base: temp.path().to_owned(),
            channel: RefCell::new(ChannelConfig::default()),
            udp: Cell::new(true),
            hide_after_launch: Cell::new(false),
            hide_window: Box::new(|ui| ui.hide().map_err(Into::into)),
            state: RefCell::new(UiState::default()),
            log: RefCell::new(Vec::new()),
            switches: RefCell::new(SwitchQueue::default()),
            errors: RefCell::new(Vec::new()),
            error_window: RefCell::new(None),
            sender,
            executor: |_, _, _, _| Ok(Outcome::Saved),
        };
        c.state.borrow_mut().begin(Job::UdpSettings);
        assert!(!c.request_close());
        assert!(!ui.get_feedback().is_empty());
        c.refresh();
        assert!(!ui.get_feedback().is_empty());
        c.event(Event::Finished(Outcome::Saved));
        assert!(ui.get_feedback().is_empty());
        assert!(c.request_close());
        ui.set_feedback("窗口操作失败".into());
        c.refresh();
        assert_eq!(ui.get_feedback(), "窗口操作失败");
    }
    #[test]
    fn acknowledged_channel_choice_is_applied_without_a_second_storage_read() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::sync_channel(64);
        let c = Controller {
            ui: ui.as_weak(),
            base: temp.path().to_owned(),
            channel: RefCell::new(ChannelConfig::default()),
            udp: Cell::new(false),
            hide_after_launch: Cell::new(false),
            hide_window: Box::new(|ui| ui.hide().map_err(Into::into)),
            state: RefCell::new(UiState::default()),
            log: RefCell::new(Vec::new()),
            switches: RefCell::new(SwitchQueue::default()),
            errors: RefCell::new(Vec::new()),
            error_window: RefCell::new(None),
            sender,
            executor: |_, _, _, _| Ok(Outcome::Saved),
        };
        c.submit_switch(Control::Channel, true);
        assert!(ui.get_dev_channel());
        c.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(ui.get_dev_channel());
        assert_eq!(c.channel.borrow().channel, UpdateChannel::Dev);
        assert_eq!(
            ui.get_window_title(),
            config::window_title(UpdateChannel::Dev)
        );
        config::save_channel_config(
            temp.path(),
            &ChannelConfig {
                channel: UpdateChannel::Stable,
            },
        )
        .unwrap();
        c.submit_switch(Control::Udp, true);
        c.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert_eq!(
            c.channel.borrow().channel,
            UpdateChannel::Dev,
            "UDP acknowledgement cannot reload a stale channel file"
        );
        assert!(ui.get_dev_channel());
    }
    #[test]
    fn repeated_switch_clicks_apply_latest_intent_without_duplicate_backend_work() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::sync_channel(64);
        fn fixture(
            base: &Path,
            _: &ChannelConfig,
            request: Request,
            _: &dyn Fn(Progress),
        ) -> Result<Outcome> {
            use std::io::Write;
            let Request::Proxy(value) = request else {
                anyhow::bail!("unexpected request")
            };
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(base.join("calls"))?;
            write!(file, "{value};")?;
            Ok(if value {
                Outcome::ProxyStarted
            } else {
                Outcome::ProxyStopped
            })
        }
        let c = Controller {
            ui: ui.as_weak(),
            base: temp.path().to_owned(),
            channel: RefCell::new(ChannelConfig::default()),
            udp: Cell::new(true),
            hide_after_launch: Cell::new(false),
            hide_window: Box::new(|ui| ui.hide().map_err(Into::into)),
            state: RefCell::new(UiState::default()),
            log: RefCell::new(Vec::new()),
            switches: RefCell::new(SwitchQueue::default()),
            errors: RefCell::new(Vec::new()),
            error_window: RefCell::new(None),
            sender,
            executor: fixture,
        };
        c.submit_switch(Control::Proxy, true);
        assert!(ui.get_proxy_on());
        c.submit_switch(Control::Proxy, false);
        assert!(!ui.get_proxy_on());
        c.submit_switch(Control::Proxy, true);
        assert!(ui.get_proxy_on());
        c.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("calls")).unwrap(),
            "true;"
        );
        c.submit_switch(Control::Proxy, false);
        c.submit_switch(Control::Proxy, true);
        c.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(
            ui.get_proxy_on(),
            "old stop completion must not overwrite newer on intent"
        );
        c.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(ui.get_proxy_on());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("calls")).unwrap(),
            "true;false;true;"
        );
    }
    #[test]
    fn short_busy_hints_are_not_shown_but_long_updates_are_visible() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        let mut state = UiState::default();
        state.begin(Job::Update);
        state.finish(Outcome::Updated(false));
        state.begin(Job::Update);
        render_at(&ui, &state, Duration::from_millis(299));
        assert!(!ui.get_main_busy());
        assert_eq!(ui.get_status(), "一切就绪");
        assert_eq!(ui.get_action_text(), "启动 PCL");
        render_at(&ui, &state, Duration::from_millis(300));
        assert!(ui.get_main_busy() && ui.get_updating());
        assert_eq!(ui.get_action_text(), "请稍候…");
        state.finish(Outcome::Failed("explicit failure".into()));
        render_at(&ui, &state, Duration::ZERO);
        assert!(ui.get_has_error());
    }
    fn smoke_launch(base: &Path) -> Result<Outcome> {
        let attempted = base.join("launch-attempted");
        if !attempted.exists() {
            std::fs::write(attempted, "fixture")?;
            anyhow::bail!("隔离测试：首次启动失败；再次点击模拟成功，不运行真实 PCL")
        }
        Ok(Outcome::Launched)
    }
    #[test]
    fn desktop_launch_fixture_fails_once_then_simulates_success_without_pcl() {
        let temp = tempfile::tempdir().unwrap();
        assert!(smoke_launch(temp.path()).is_err());
        assert_eq!(smoke_launch(temp.path()).unwrap(), Outcome::Launched);
        assert_eq!(smoke_launch(temp.path()).unwrap(), Outcome::Launched);
        assert!(!temp.path().join(config::PCL2_EXE).exists());
    }
    #[test]
    #[ignore = "interactive Windows smoke; backend effects isolated to temporary fixture"]
    fn desktop_window_smoke() {
        let temp = tempfile::tempdir().unwrap();
        version::save_local_version(
            temp.path(),
            &version::LocalVersion {
                mc_version: "1.21.11".into(),
                fabric_version: "0.18.4".into(),
                version_tag: "smoke".into(),
            },
        )
        .unwrap();
        fn fixture(
            base: &Path,
            channel: &ChannelConfig,
            request: Request,
            progress: &dyn Fn(Progress),
        ) -> Result<Outcome> {
            match request {
                Request::Update => {
                    progress(Progress::new(50, "隔离测试：验证后台进度"));
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(Outcome::Updated(false))
                }
                Request::Proxy(value) => {
                    std::thread::sleep(Duration::from_millis(800));
                    let attempted = base.join("proxy-attempted");
                    if value && !attempted.exists() {
                        std::fs::write(attempted, "fixture")?;
                        anyhow::bail!(
                            "隔离测试：代理启动失败；订阅服务器返回 HTTP 503。可重试，未修改真实 Discord。"
                        );
                    }
                    Ok(if value {
                        Outcome::ProxyStarted
                    } else {
                        Outcome::ProxyStopped
                    })
                }
                Request::Launch => smoke_launch(base),
                other => execute(base, channel, other, progress),
            }
        }
        run_with_executor(temp.path().to_owned(), ChannelConfig::default(), fixture).unwrap();
    }
    #[test]
    fn persisted_udp_preserves_proxy_opt_out_and_reports_write_failure() {
        let temp = tempfile::tempdir().unwrap();
        config::save_user_settings(
            temp.path(),
            &config::UserSettings {
                proxy_udp: true,
                proxy_enabled: false,
                ..config::UserSettings::default()
            },
        )
        .unwrap();
        save_udp(temp.path(), false).unwrap();
        let settings = config::load_user_settings(temp.path());
        assert!(!settings.proxy_udp && !settings.proxy_enabled);
        let blocked = temp.path().join("blocked");
        std::fs::write(&blocked, "file").unwrap();
        assert!(save_udp(&blocked, true).is_err());
    }
    #[test]
    fn native_view_reflects_real_update_failure_and_ready_states() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = App::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(400.0, 420.0));
        let mut state = UiState::default();
        state.begin(Job::Update);
        render_at(&ui, &state, Duration::from_millis(300));
        assert!(!ui.get_launchable());
        let clicks = Rc::new(std::cell::Cell::new(0));
        let count = clicks.clone();
        ui.on_primary_action(move || count.set(count.get() + 1));
        let find = |label: &str| {
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap()
        };
        find("请稍候…").invoke_accessible_default_action();
        assert_eq!(
            clicks.get(),
            0,
            "busy actions must ignore accessibility activation"
        );
        let update_position = find("检查更新").absolute_position();
        state.finish(Outcome::Failed("SHA256 mismatch".into()));
        render(&ui, &state);
        assert_eq!(
            find("检查更新").absolute_position(),
            update_position,
            "failure details must not move the existing action"
        );
        assert_eq!(ui.get_action_text(), "重试更新");
        assert!(
            ui.get_detail().is_empty(),
            "failure reasons belong only in the shared error window"
        );
        state.begin(Job::Update);
        state.finish(Outcome::Updated(true));
        render(&ui, &state);
        assert!(ui.get_launchable() && ui.get_proxy_on());
        assert_eq!(ui.get_action_text(), "启动 PCL");
        let launch_position = find("启动 PCL").absolute_position();
        let proxy_position = find("Discord 代理开关").absolute_position();
        state.begin(Job::ProxyStart);
        render(&ui, &state);
        assert_eq!(
            ui.get_action_text(),
            "启动 PCL",
            "proxy activity must not flash the primary action label"
        );
        assert!(!ui.get_updating());
        assert_eq!(
            ui.get_proxy_text(),
            "已启用",
            "switch displays intent without a connecting intermediate state"
        );
        assert_eq!(find("启动 PCL").absolute_position(), launch_position);
        assert_eq!(find("Discord 代理开关").absolute_position(), proxy_position);
        state.finish(Outcome::ProxyStarted);
        render(&ui, &state);
        let check_position = find("检查更新").absolute_position();
        state.begin(Job::ProxyStop);
        state.finish(Outcome::ProxyStopped);
        state.begin(Job::ProxyStart);
        state.finish(Outcome::Failed("HTTP 503: subscription unavailable".into()));
        render(&ui, &state);
        assert!(ui.get_proxy_error());
        assert!(
            !ui.get_has_error(),
            "proxy errors must not insert a primary-area error action"
        );
        assert!(
            ui.get_detail().is_empty(),
            "proxy errors stay beside the proxy switch"
        );
        assert_eq!(find("检查更新").absolute_position(), check_position);
        find("代理错误详情");
        state.begin(Job::Update);
        state.finish(Outcome::Updated(true));
        render(&ui, &state);
        find("启动 PCL").invoke_accessible_default_action();
        assert_eq!(clicks.get(), 1);
        find("设置").invoke_accessible_default_action();
        let udp_changes = Rc::new(std::cell::Cell::new(0));
        let count = udp_changes.clone();
        ui.on_udp_change(move |_| count.set(count.get() + 1));
        find("UDP 流量开关").invoke_accessible_default_action();
        assert_eq!(udp_changes.get(), 1);
        ui.set_switches_enabled(false);
        find("UDP 流量开关").invoke_accessible_default_action();
        assert_eq!(udp_changes.get(), 1);
        find("关于").invoke_accessible_default_action();
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "#MadeWithSlint")
                .next()
                .is_some()
        );
        find("开源许可");
        let temp = tempfile::tempdir().unwrap();
        config::save_user_settings(
            temp.path(),
            &config::UserSettings {
                proxy_udp: true,
                proxy_enabled: false,
                ..config::UserSettings::default()
            },
        )
        .unwrap();
        let (sender, receiver) = mpsc::sync_channel(64);
        let controller = Controller {
            ui: ui.as_weak(),
            base: temp.path().to_owned(),
            channel: RefCell::new(ChannelConfig::default()),
            udp: Cell::new(true),
            hide_after_launch: Cell::new(false),
            hide_window: Box::new(|ui| ui.hide().map_err(Into::into)),
            state: RefCell::new(UiState::default()),
            log: RefCell::new(Vec::new()),
            switches: RefCell::new(SwitchQueue::default()),
            errors: RefCell::new(Vec::new()),
            error_window: RefCell::new(None),
            sender,
            executor: |_, _, _, _| anyhow::bail!("fixture: settings write rejected"),
        };
        ui.set_udp_enabled(true);
        controller.submit_switch(Control::Udp, false);
        assert!(
            !ui.get_udp_enabled(),
            "settings switch must change before persistence completes"
        );
        controller.event(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(
            ui.get_udp_enabled(),
            "failed save restores the persisted/default value"
        );
        assert!(ui.get_settings_error().contains("settings write rejected"));
        assert!(
            controller
                .errors
                .borrow()
                .join("\n")
                .contains("settings write rejected")
        );
        let dialog = LogWindow::new().unwrap();
        *controller.error_window.borrow_mut() = Some(dialog.as_weak());
        for index in 0..600 {
            controller.append_log(format!("progress {index}"));
        }
        controller.state.borrow_mut().begin(Job::Update);
        controller.event(Event::Finished(Outcome::Failed(
            "second full error cause".into(),
        )));
        assert!(dialog.get_log_text().contains("settings write rejected"));
        assert!(dialog.get_log_text().contains("second full error cause"));
        assert_eq!(controller.errors.borrow().len(), 2);
        assert!(
            execute(
                tempfile::tempdir().unwrap().path(),
                &ChannelConfig::default(),
                Request::Launch,
                &|_| {}
            )
            .is_err()
        );
    }
}
