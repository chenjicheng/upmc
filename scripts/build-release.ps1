# ============================================================
# build-release.ps1 — 构建首次分发包
# ============================================================
# 使用方法：
#   .\scripts\build-release.ps1
#
# 流程：
#   1. Release 编译 Rust 更新器
#   2. 组装分发目录结构
#   3. 打包成 .zip
#
# 输出：
#   ./dist/upmc-server-pack.zip
#
# 注意：以下文件需要你手动准备放入 launcher/ 目录：
#   - launcher/PCL/Plain Craft Launcher 2.exe  (PCL2 本体)
#   - launcher/jre/                             (Java 运行时)
#   - launcher/updater/packwiz-installer-bootstrap.jar
#   - launcher/updater/fabric-installer.jar
# ============================================================

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path $PSScriptRoot -Parent
$UpdaterDir = Join-Path $RepoRoot "updater-rs"
$LauncherDir = Join-Path $RepoRoot "launcher"
$DistDir = Join-Path $RepoRoot "dist"
$OutputZip = Join-Path $DistDir "upmc-server-pack.zip"
$TempDir = Join-Path $DistDir "_build_temp"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  UPMC 首次分发包构建" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# ── 步骤 1: 编译 Rust 更新器 (Release) ──
Write-Host "[1/4] 编译更新器 (Release)..." -ForegroundColor Yellow

Push-Location $UpdaterDir
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[错误] Rust 编译失败" -ForegroundColor Red
        exit 1
    }
}
finally {
    Pop-Location
}

$ExePath = Join-Path $UpdaterDir "target\release\upmc-updater.exe"
if (-not (Test-Path $ExePath)) {
    Write-Host "[错误] 找不到编译产物: $ExePath" -ForegroundColor Red
    exit 1
}

Write-Host "  编译产物: $ExePath"
Write-Host "  大小: $([math]::Round((Get-Item $ExePath).Length / 1MB, 2)) MB"

# ── 步骤 2: 清理临时目录 ──
Write-Host "[2/4] 准备分发目录..." -ForegroundColor Yellow

if (Test-Path $TempDir) {
    Remove-Item $TempDir -Recurse -Force
}
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null

# ── 步骤 3: 组装文件 ──
Write-Host "[3/4] 组装分发文件..." -ForegroundColor Yellow

# 复制更新器 exe（重命名为服务器名）
Copy-Item $ExePath (Join-Path $TempDir "我的服务器.exe")

# 复制 launcher/ 下的所有内容（PCL、JRE、updater jars）
if (Test-Path $LauncherDir) {
    Get-ChildItem -Path $LauncherDir | ForEach-Object {
        $dest = Join-Path $TempDir $_.Name
        if ($_.PSIsContainer) {
            Copy-Item $_.FullName $dest -Recurse -Force
        }
        else {
            Copy-Item $_.FullName $dest -Force
        }
    }
}

# 确保必要目录存在
$dirsToCreate = @(
    (Join-Path $TempDir "PCL"),
    (Join-Path $TempDir ".minecraft"),
    (Join-Path $TempDir ".minecraft\mods"),
    (Join-Path $TempDir ".minecraft\config"),
    (Join-Path $TempDir "updater"),
    (Join-Path $TempDir "jre")
)

foreach ($dir in $dirsToCreate) {
    if (-not (Test-Path $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }
}

# ── 步骤 4: 打包 ──
Write-Host "[4/4] 打包成 ZIP..." -ForegroundColor Yellow

if (Test-Path $OutputZip) {
    Remove-Item $OutputZip -Force
}

New-Item -ItemType Directory -Path $DistDir -Force | Out-Null
Compress-Archive -Path "$TempDir\*" -DestinationPath $OutputZip

# 清理临时目录
Remove-Item $TempDir -Recurse -Force

$zipSize = [math]::Round((Get-Item $OutputZip).Length / 1MB, 2)

Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host "  构建完成！" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
Write-Host ""
Write-Host "输出: $OutputZip"
Write-Host "大小: $zipSize MB"
Write-Host ""
Write-Host "发给玩家之前，请确保以下文件已放入 launcher/ 目录：" -ForegroundColor Yellow
Write-Host "  □ launcher/PCL/Plain Craft Launcher 2.exe"
Write-Host "  □ launcher/jre/bin/java.exe (Adoptium JRE)"
Write-Host "  □ launcher/updater/packwiz-installer-bootstrap.jar"
Write-Host "  □ launcher/updater/fabric-installer.jar"
