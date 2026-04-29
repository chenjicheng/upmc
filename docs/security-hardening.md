# 安全加固说明

本文记录 UPMC 更新器为降低 Microsoft Defender 误报和供应链风险所做的约束。

## 自更新

自更新不再调用 PowerShell、cmd 或脚本解释器，也不再使用 `ExecutionPolicy Bypass`。主程序下载新版 `updater.exe` 到 `.exe.new` 后，会复制当前 exe 为唯一命名的 `upmc-update-helper-*.exe`（旧版兼容名为 `upmc-update-helper.exe`）。helper 进程等待主程序退出后覆盖原 exe 并启动新版。

这比隐藏 PowerShell 内联命令更不容易触发 Defender 的下载器/dropper 启发式规则。

## 下载来源白名单

远程 `server.json` 能控制首次安装下载项，因此下载 URL 必须满足：

1. 使用 HTTPS。
2. 主机名属于 `TRUSTED_DOWNLOAD_HOST_SUFFIXES` 中的可信域名后缀。

如需新增下载源，应先在 `upmc/src/config.rs` 中加入可信域名后缀，再发布更新器。

## Bootstrap 下载 SHA256

以下下载项会落地执行或解压，必须在 `server.json` 的 `downloads` 中提供对应 SHA256：

```json
{
  "downloads": {
    "pcl2_url": "https://.../PlainCraftLauncher2.exe",
    "pcl2_sha256": "64位小写十六进制SHA256",

    "packwiz_bootstrap_url": "https://.../packwiz-installer-bootstrap.jar",
    "packwiz_bootstrap_sha256": "64位小写十六进制SHA256",

    "fabric_installer_url": "https://.../fabric-installer.jar",
    "fabric_installer_sha256": "64位小写十六进制SHA256",

    "settings_url": "https://.../settings.zip",
    "settings_sha256": "64位小写十六进制SHA256"
  }
}
```

`settings_url` 可省略；如果配置了 `settings_url`，则必须同时配置 `settings_sha256`。

## ZIP 解压

ZIP 条目会被检查并拒绝以下路径：

- 包含父目录跳转的路径；
- 绝对路径；
- Windows 盘符或前缀路径；
- 根目录路径。

这可以避免下载的 ZIP 覆盖目标目录之外的文件。

## 可选 Authenticode 签名

GitHub Actions 支持可选签名。配置以下仓库 Secrets 后，发布流程会先签名 `updater.exe`，再计算 SHA256 并上传：

- `WINDOWS_SIGN_CERT_BASE64`：PFX 证书文件的 base64 内容；
- `WINDOWS_SIGN_CERT_PASSWORD`：PFX 密码。

未配置证书时会跳过签名，不阻塞构建。

在 PowerShell 中生成 base64 可使用：

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes("codesign.pfx")) | Set-Content cert-base64.txt
```
