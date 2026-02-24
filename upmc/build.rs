// ============================================================
// build.rs — 构建脚本
// ============================================================
// 在编译时运行，用于：
//   1. 嵌入应用图标 (.ico) 到 exe 文件
//   2. 设置 exe 的版本信息（右键属性可见）
//
// 依赖 winresource crate。
// ============================================================

fn main() {
    // 只在 Windows 上执行
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winresource::WindowsResource::new();

        // 设置 exe 图标（如果 assets/icon.ico 存在）
        // 如果没有图标文件，注释掉这行即可正常编译
        if std::path::Path::new("assets/icon.ico").exists() {
            res.set_icon("assets/icon.ico");
        }

        // 嵌入应用程序清单（启用 Common Controls v6，
        // 解决 GetWindowSubclass 入口点找不到的问题）
        if std::path::Path::new("assets/app.manifest").exists() {
            res.set_manifest_file("assets/app.manifest");
        }

        // 设置 exe 版本信息（右键 → 属性 → 详细信息）
        res.set("ProductName", "UPMC 服务器更新器");
        res.set("FileDescription", "Minecraft 服务器整合包自动更新工具");
        res.set("LegalCopyright", "MIT License");

        // 资源文件变更时触发重新编译
        println!("cargo:rerun-if-changed=assets/icon.ico");
        println!("cargo:rerun-if-changed=assets/app.manifest");

        // 编译资源
        res.compile().expect("编译 Windows 资源失败");
    }
}
