//! UI state contract shared by native callbacks and worker completion delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    Update,
    ProxyStart,
    ProxyStop,
    Settings,
    Launch,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Updated(bool),
    Offline,
    Restarting,
    ProxyStarted,
    ProxyStopped,
    Saved,
    Launched,
    Failed(String),
}
#[derive(Debug, Default)]
pub struct UiState {
    pub started: Option<std::time::Instant>,
    pub previous_status: String,
    pub previous_ready: bool,
    pub progress_detail: String,
    pub busy: Option<Job>,
    pub ready: bool,
    pub proxy: bool,
    pub exit: bool,
    pub status: String,
    pub error: String,
    pub percent: u32,
}
impl UiState {
    pub fn begin(&mut self, job: Job) -> bool {
        if self.busy.is_some() || self.exit || (job == Job::Launch && !self.ready) {
            return false;
        }
        self.previous_status = self.status.clone();
        self.previous_ready = self.ready;
        self.started = Some(std::time::Instant::now());
        self.progress_detail.clear();
        self.busy = Some(job);
        self.error.clear();
        self.percent = 0;
        let activity = match job {
            Job::Update => {
                self.ready = false;
                "正在检查更新"
            }
            Job::ProxyStart => {
                self.proxy = true;
                "已启用"
            }
            Job::ProxyStop => {
                self.proxy = false;
                "未启用"
            }
            Job::Settings => "正在保存设置",
            Job::Launch => "正在启动 PCL",
        };
        if matches!(job, Job::Update | Job::Launch) {
            self.status = activity.into();
        }
        true
    }
    pub fn finish(&mut self, outcome: Outcome) {
        let Some(job) = self.busy.take() else {
            return;
        };
        self.started = None;
        self.progress_detail.clear();
        let activity = match outcome {
            Outcome::Updated(proxy) => {
                self.ready = true;
                self.proxy = proxy;
                self.percent = 100;
                "一切就绪"
            }
            Outcome::Offline => {
                self.ready = true;
                self.proxy = false;
                "离线模式"
            }
            Outcome::Restarting => {
                self.ready = false;
                self.exit = true;
                "正在重启更新器"
            }
            Outcome::ProxyStarted => {
                self.proxy = true;
                "代理已启用"
            }
            Outcome::ProxyStopped => {
                self.proxy = false;
                "代理已停止"
            }
            Outcome::Saved => "设置已保存",
            Outcome::Launched => {
                self.exit = true;
                "PCL 已启动"
            }
            Outcome::Failed(error) => {
                self.error = error;
                match job {
                    Job::Update => "更新失败",
                    Job::ProxyStart => {
                        self.proxy = false;
                        "代理启用失败"
                    }
                    Job::ProxyStop => {
                        self.proxy = false;
                        "代理停止不完整"
                    }
                    Job::Settings => "设置保存失败",
                    Job::Launch => "启动失败",
                }
            }
        };
        if matches!(job, Job::Update | Job::Launch) {
            self.status = activity.into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_switch_is_optimistic_and_failure_restores_off_with_full_error() {
        let mut s = UiState::default();
        s.begin(Job::Update);
        s.finish(Outcome::Updated(false));
        assert!(s.begin(Job::ProxyStart));
        assert!(s.proxy, "clicking start must light the switch immediately");
        s.finish(Outcome::Failed(
            "subscription failed: HTTP 503; upstream unavailable".into(),
        ));
        assert!(!s.proxy);
        assert_eq!(
            s.error,
            "subscription failed: HTTP 503; upstream unavailable"
        );
        assert!(s.ready);
        s.begin(Job::ProxyStart);
        s.finish(Outcome::ProxyStarted);
        assert!(s.proxy);
        s.begin(Job::ProxyStop);
        assert!(
            !s.proxy,
            "stop also applies the requested switch position immediately"
        );
    }
    #[test]
    fn proxy_start_and_stop_leave_game_status_unchanged() {
        let mut s = UiState::default();
        s.begin(Job::Update);
        s.finish(Outcome::Updated(false));
        let status = s.status.clone();
        assert!(s.begin(Job::ProxyStart));
        assert_eq!(
            s.status, status,
            "starting proxy must not replace the game status"
        );
        s.finish(Outcome::ProxyStarted);
        assert_eq!(s.status, status);
        assert!(s.begin(Job::ProxyStop));
        assert_eq!(s.status, status);
        s.finish(Outcome::ProxyStopped);
        assert_eq!(s.status, status);
    }
    #[test]
    fn update_gates_launch_and_serializes_all_operations() {
        let mut s = UiState::default();
        assert!(!s.begin(Job::Launch));
        assert!(s.begin(Job::Update));
        for job in [Job::Update, Job::Launch, Job::ProxyStart, Job::Settings] {
            assert!(!s.begin(job));
        }
        s.finish(Outcome::Updated(true));
        assert!(s.ready && s.proxy && !s.exit);
        assert!(s.begin(Job::Launch));
        s.finish(Outcome::Failed("launcher missing".into()));
        assert!(s.ready && !s.exit);
        assert_eq!(s.error, "launcher missing");
        assert!(s.begin(Job::Launch));
        s.finish(Outcome::Launched);
        assert!(s.exit);
    }
    #[test]
    fn failed_recheck_revokes_old_readiness_and_can_retry() {
        let mut s = UiState::default();
        assert!(s.begin(Job::Update));
        s.finish(Outcome::Updated(false));
        assert!(s.begin(Job::Update));
        assert!(!s.ready);
        s.finish(Outcome::Failed("hash mismatch".into()));
        assert!(!s.begin(Job::Launch));
        assert!(s.begin(Job::Update));
        s.finish(Outcome::Offline);
        assert!(s.ready);
        assert!(s.status.contains("离线"));
    }
    #[test]
    fn proxy_failures_do_not_clear_game_readiness_and_stop_failure_is_visible() {
        let mut s = UiState::default();
        s.begin(Job::Update);
        s.finish(Outcome::Updated(true));
        assert!(s.begin(Job::ProxyStop));
        s.finish(Outcome::Failed("cleanup failed".into()));
        assert!(s.ready);
        assert!(!s.proxy);
        assert_eq!(s.error, "cleanup failed");
        assert!(s.begin(Job::ProxyStart));
        s.finish(Outcome::ProxyStarted);
        assert!(s.proxy);
        assert!(s.ready);
        assert!(s.begin(Job::ProxyStop));
        s.finish(Outcome::ProxyStopped);
        assert!(!s.proxy);
        assert!(s.ready);
    }
    #[test]
    fn restart_never_enables_launch_and_settings_do_not_authorize_it() {
        let mut s = UiState::default();
        s.begin(Job::Settings);
        s.finish(Outcome::Saved);
        assert!(!s.ready && !s.exit);
        s.begin(Job::Update);
        s.finish(Outcome::Restarting);
        assert!(s.exit && !s.ready);
        assert!(!s.begin(Job::Update));
    }
}
