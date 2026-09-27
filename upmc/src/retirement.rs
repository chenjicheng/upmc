slint::include_modules!();

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = RetirementNotice::new()?;
    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retirement_screen_tells_players_to_delete_the_launcher() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = RetirementNotice::new().unwrap();
        assert_eq!(ui.get_heading(), "UPMC 启动器已停止服务");
        assert_eq!(ui.get_instruction(), "请删除本软件，不再使用。");
        for old_action in ["启动 PCL", "检查更新", "设置", "Discord 代理开关"] {
            assert!(
                i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, old_action)
                    .next()
                    .is_none(),
                "retirement screen must not expose {old_action}"
            );
        }
    }
}
