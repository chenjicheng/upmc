// ============================================================
// build.rs — 构建脚本
// ============================================================
// 在编译时运行，用于：
//   1. 嵌入应用图标 (.ico) 到 exe 文件
//   2. 设置 exe 的版本信息（右键属性可见）
//   3. 传递 UPMC_BUILD_ID 环境变量供 option_env!() 使用
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

    // 内嵌的 DLL 变更时触发重新编译（include_bytes! 引用的文件）
    println!("cargo:rerun-if-changed=../target/release/dwrite.dll");
    println!("cargo:rerun-if-changed=../target/release/force_proxy.dll");

    // 将环境变量传递给 rustc，供 option_env!() / env!() 使用
    // CI 中设置，本地开发时不设置则使用默认值

    // UPMC_BUILD_ID: commit SHA，用于 dev 通道版本对比
    println!("cargo:rerun-if-env-changed=UPMC_BUILD_ID");
    if let Ok(build_id) = std::env::var("UPMC_BUILD_ID") {
        println!("cargo:rustc-env=UPMC_BUILD_ID={build_id}");
    }

    // UPMC_CHANNEL: 构建通道（stable/dev），决定默认更新通道
    // 未设置时默认 stable
    println!("cargo:rerun-if-env-changed=UPMC_CHANNEL");
    let channel = std::env::var("UPMC_CHANNEL")
        .unwrap_or_else(|_| "stable".to_string())
        .to_lowercase();
    println!("cargo:rustc-env=UPMC_CHANNEL={channel}");

    // UPMC_SUB_URL: 代理订阅地址，CI 从 GitHub Secrets 注入
    // 未设置时 option_env!() 返回 None，回退为空字符串
    println!("cargo:rerun-if-env-changed=UPMC_SUB_URL");
}
