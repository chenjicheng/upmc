use crate::{
    config::{self, ChannelConfig, UpdateChannel},
    discord_proxy,
    gui_state::{Job, Outcome, UiState},
    update::{self, Progress, UpdateResult},
    version,
};
use anyhow::{Context, Result};
use slint::ComponentHandle;
use std::os::windows::process::CommandExt;
use std::{
    cell::RefCell,
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
    ui.set_settings_error(if state.error_job() == Some(Job::Settings) {
        state.error.clone().into()
    } else {
        "".into()
    });
    ui.set_scenario(if main_error { 5 } else { 0 });
    ui.set_detail(
        if main_error {
            state.error.chars().take(100).collect::<String>()
        } else if main_busy {
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
    Launch,
}
impl Request {
    fn job(&self) -> Job {
        match self {
            Self::Update => Job::Update,
            Self::Proxy(true) => Job::ProxyStart,
            Self::Proxy(false) => Job::ProxyStop,
            Self::Udp(_) | Self::Channel(_) => Job::Settings,
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
    state: RefCell<UiState>,
    log: RefCell<Vec<String>>,
    sender: SyncSender<Event>,
    executor: Executor,
}
impl Controller {
    fn repaint(&self) {
        if let Some(ui) = self.ui.upgrade() {
            render(&ui, &self.state.borrow());
        }
    }
    fn refresh(&self) {
        if let Some(ui) = self.ui.upgrade() {
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
            ui.set_udp_enabled(config::load_user_settings(&self.base).proxy_udp);
            ui.set_dev_channel(self.channel.borrow().channel == UpdateChannel::Dev);
        }
    }
    fn start(&self, request: Request) {
        if !self.state.borrow_mut().begin(request.job()) {
            return;
        }
        self.refresh();
        if let Some(ui) = self.ui.upgrade() {
            match &request {
                Request::Udp(value) => ui.set_udp_enabled(*value),
                Request::Channel(value) => ui.set_dev_channel(*value == UpdateChannel::Dev),
                _ => {}
            }
        }
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
            self.state
                .borrow_mut()
                .finish(Outcome::Failed(format!("无法启动后台任务：{error}")));
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
                if matches!(outcome, Outcome::Saved) {
                    // Channel file may have been updated by this serialized worker.
                    match std::fs::read(self.base.join(config::CHANNEL_CONFIG_FILE))
                        .ok()
                        .and_then(|v| serde_json::from_slice::<ChannelConfig>(&v).ok())
                    {
                        Some(channel) => *self.channel.borrow_mut() = channel,
                        None => {} // UDP saves do not require a channel file.
                    }
                }
                if let Outcome::Failed(error) = &outcome {
                    self.append_log(error.clone());
                }
                self.state.borrow_mut().finish(outcome);
                self.refresh();
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
                ui.set_feedback("任务进行中，请等待完成后关闭。".into());
            }
            false
        } else {
            true
        }
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
    let controller = Rc::new(Controller {
        ui: ui.as_weak(),
        base,
        channel: RefCell::new(channel),
        state: RefCell::new(UiState::default()),
        log: RefCell::new(Vec::new()),
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
        let enable = !c.state.borrow().proxy;
        c.start(Request::Proxy(enable));
    });
    let c = controller.clone();
    ui.on_udp_change(move |value| c.start(Request::Udp(value)));
    let c = controller.clone();
    ui.on_channel_change(move |value| {
        c.start(Request::Channel(if value {
            UpdateChannel::Dev
        } else {
            UpdateChannel::Stable
        }))
    });
    let c = controller.clone();
    ui.on_close_window(move || {
        if c.request_close() {
            let _ = slint::quit_event_loop();
        }
    });
    let c = controller.clone();
    ui.window().on_close_requested(move || {
        if c.request_close() {
            slint::CloseRequestResponse::HideWindow
        } else {
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let weak = ui.as_weak();
    ui.on_minimize_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });
    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            match ui.window().with_winit_window(|w| w.drag_window()) {
                Some(Ok(())) => {}
                other => ui.set_feedback(format!("窗口拖动失败：{other:?}").into()),
            }
        }
    });
    let logs = LogWindow::new()?;
    let c = controller.clone();
    let weak_logs = logs.as_weak();
    ui.on_show_logs(move || {
        if let Some(logs) = weak_logs.upgrade() {
            logs.set_heading("错误详情".into());
            logs.set_log_text(c.log.borrow().join("\n").into());
            if let Err(error) = logs.show() {
                if let Some(ui) = c.ui.upgrade() {
                    ui.set_feedback(format!("无法打开日志：{error}").into());
                }
            }
        }
    });
    let timer = slint::Timer::default();
    let weak_logs = logs.as_weak();
    let weak_ui = ui.as_weak();
    ui.on_show_licenses(move || {
        if let Some(logs) = weak_logs.upgrade() {
            logs.set_heading("开源许可".into());
            logs.set_log_text(include_str!("../assets/third-party-notices.txt").into());
            if let Err(error) = logs.show()
                && let Some(ui) = weak_ui.upgrade()
            {
                ui.set_feedback(format!("无法打开开源许可：{error}").into());
            }
        }
    });
    let c = controller.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(60),
        move || {
            for event in receiver.try_iter().take(128) {
                c.event(event);
            }
            if c.state.borrow().busy.is_some() {
                c.repaint();
            }
        },
    );
    controller.start(Request::Update);
    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_state::{Job, Outcome};
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
                Request::Launch => anyhow::bail!("隔离测试：启动失败时保留窗口和错误详情"),
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
        assert!(ui.get_detail().contains("SHA256 mismatch"));
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
        ui.set_busy(true);
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
            },
        )
        .unwrap();
        let (sender, receiver) = mpsc::sync_channel(64);
        let controller = Controller {
            ui: ui.as_weak(),
            base: temp.path().to_owned(),
            channel: RefCell::new(ChannelConfig::default()),
            state: RefCell::new(UiState::default()),
            log: RefCell::new(Vec::new()),
            sender,
            executor: |_, _, _, _| anyhow::bail!("fixture: settings write rejected"),
        };
        ui.set_udp_enabled(true);
        controller.start(Request::Udp(false));
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
