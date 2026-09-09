//! Coalesce switch intent while backend jobs remain serialized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Proxy,
    Udp,
    Channel,
    HideAfterLaunch,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub control: Control,
    pub enabled: bool,
}
#[derive(Default)]
pub struct SwitchQueue {
    pending: std::collections::VecDeque<Change>,
    active: Option<Change>,
    desired: [Option<bool>; 4],
}
impl SwitchQueue {
    pub fn active(&self) -> Option<Change> {
        self.active
    }
    pub fn request(&mut self, control: Control, enabled: bool) {
        self.desired[control as usize] = Some(enabled);
        self.pending.retain(|change| change.control != control);
        self.pending.push_back(Change { control, enabled });
    }
    pub fn take_next(&mut self) -> Option<Change> {
        if self.active.is_some() {
            return None;
        }
        self.active = self.pending.pop_front();
        self.active
    }
    pub fn complete(&mut self, success: bool) {
        let Some(active) = self.active.take() else {
            return;
        };
        // A different setting may change the meaning of a repeated proxy start.
        // Coalesce only an uninterrupted sequence for the same control.
        if success
            && self
                .pending
                .iter()
                .all(|change| change.control == active.control)
        {
            self.pending.retain(|change| *change != active);
        }
        if !self
            .pending
            .iter()
            .any(|change| change.control == active.control)
        {
            self.desired[active.control as usize] = None;
        }
    }
    pub fn value(&self, control: Control, actual: bool) -> bool {
        self.desired[control as usize].unwrap_or(actual)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_settings_change_keeps_a_later_proxy_reapply() {
        let mut q = SwitchQueue::default();
        q.request(Control::Proxy, true);
        q.take_next();
        q.request(Control::Udp, false);
        q.request(Control::Proxy, false);
        q.request(Control::Proxy, true);
        q.complete(true);
        assert_eq!(
            q.take_next(),
            Some(Change {
                control: Control::Udp,
                enabled: false
            })
        );
        q.complete(true);
        assert_eq!(
            q.take_next(),
            Some(Change {
                control: Control::Proxy,
                enabled: true
            })
        );
    }
    #[test]
    fn latest_click_is_immediate_and_survives_an_older_completion() {
        let mut q = SwitchQueue::default();
        q.request(Control::Proxy, true);
        assert!(q.value(Control::Proxy, false));
        assert_eq!(
            q.take_next(),
            Some(Change {
                control: Control::Proxy,
                enabled: true
            })
        );
        q.request(Control::Proxy, false);
        assert!(!q.value(Control::Proxy, true));
        assert_eq!(
            q.take_next(),
            None,
            "never run two switch operations concurrently"
        );
        q.complete(true);
        assert!(
            !q.value(Control::Proxy, true),
            "old start success must not light a newer stop request"
        );
        assert_eq!(
            q.take_next(),
            Some(Change {
                control: Control::Proxy,
                enabled: false
            })
        );
        q.complete(true);
        assert!(!q.value(Control::Proxy, false));
        assert_eq!(q.take_next(), None);
    }
    #[test]
    fn matching_success_coalesces_rapid_reversals_without_restarting_twice() {
        let mut q = SwitchQueue::default();
        q.request(Control::Proxy, true);
        q.take_next();
        q.request(Control::Proxy, false);
        q.request(Control::Proxy, true);
        q.complete(true);
        assert_eq!(q.take_next(), None);
        assert!(q.value(Control::Proxy, true));
    }
    #[test]
    fn failed_current_intent_rolls_back_but_a_newer_request_is_retained() {
        let mut q = SwitchQueue::default();
        q.request(Control::Udp, false);
        assert!(!q.value(Control::Udp, true));
        q.take_next();
        q.complete(false);
        assert!(q.value(Control::Udp, true));
        q.request(Control::Proxy, true);
        q.take_next();
        q.request(Control::Proxy, false);
        q.complete(false);
        assert_eq!(
            q.take_next(),
            Some(Change {
                control: Control::Proxy,
                enabled: false
            })
        );
    }
    #[test]
    fn each_control_keeps_its_own_latest_choice() {
        let mut q = SwitchQueue::default();
        q.request(Control::Proxy, true);
        q.take_next();
        q.request(Control::Udp, false);
        q.request(Control::Channel, true);
        q.request(Control::Udp, true);
        assert!(q.value(Control::Udp, false));
        assert!(q.value(Control::Channel, false));
        q.complete(true);
        assert_eq!(
            q.take_next(),
            Some(Change {
                control: Control::Channel,
                enabled: true
            })
        );
        q.complete(true);
        assert_eq!(
            q.take_next(),
            Some(Change {
                control: Control::Udp,
                enabled: true
            })
        );
        q.complete(true);
        assert_eq!(q.take_next(), None);
    }
}
