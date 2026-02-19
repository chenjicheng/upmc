// ============================================================
// config.rs — 配置常量
// ============================================================
// 集中管理所有可配置的路径和 URL。
// 修改这里的常量即可适配不同服务器。
// ============================================================

/// 远程 server.json 的 URL（GitHub Pages 托管）
/// 更新器启动时会从这个地址拉取最新的版本信息。
/// 格式见项目根目录的 server.json 模板。
pub const REMOTE_SERVER_JSON_URL: &str =
    "https://YOUR_GITHUB_USERNAME.github.io/upmc-dist/server.json";

/// 本地版本记录文件名（相对于更新器 exe 所在目录）
/// 更新器会在这个文件里记录当前已安装的 MC 版本和 Fabric 版本，
/// 用于和远程 server.json 对比，判断是否需要升级。
pub const LOCAL_VERSION_FILE: &str = "updater/local.json";

/// packwiz-installer-bootstrap.jar 的路径（相对于 exe 所在目录）
pub const PACKWIZ_BOOTSTRAP_JAR: &str = "updater/packwiz-installer-bootstrap.jar";

/// fabric-installer.jar 的路径（相对于 exe 所在目录）
pub const FABRIC_INSTALLER_JAR: &str = "updater/fabric-installer.jar";

/// 内置 JRE 的 java.exe 路径（相对于 exe 所在目录）
pub const JAVA_EXE: &str = "jre/bin/java.exe";

/// .minecraft 目录路径（相对于 exe 所在目录）
pub const MINECRAFT_DIR: &str = ".minecraft";

/// PCL2 启动器路径（相对于 exe 所在目录）
pub const PCL2_EXE: &str = "PCL/Plain Craft Launcher 2.exe";

/// 窗口标题
pub const WINDOW_TITLE: &str = "我的服务器 - 更新器";

/// 窗口宽度（像素）
pub const WINDOW_WIDTH: i32 = 420;

/// 窗口高度（像素）
pub const WINDOW_HEIGHT: i32 = 200;

/// HTTP 请求超时时间（秒）
pub const HTTP_TIMEOUT_SECS: u64 = 30;
