# ============================================================
# build-release.ps1 — 编译更新器 exe
# ============================================================
# 使用方法：
#   .\scripts\build-release.ps1
#
# 输出：
#   ./dist/我的服务器.exe
#
# 这就是需要分发给玩家的唯一文件。
# 玩家双击后，exe 会自动下载所有依赖组件。
# ============================================================

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path $PSScriptRoot -Parent
$UpdaterDir = Join-Path $RepoRoot "updater-rs"
$DistDir = Join-Path $RepoRoot "dist"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  UPMC 更新器编译" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# ── 编译 Release ──
Write-Host "[1/2] 编译 Release..." -ForegroundColor Yellow

Push-Location $UpdaterDir
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[错误] 编译失败" -ForegroundColor Red
        exit 1
    }
}
finally {
    Pop-Location
}

$ExePath = Join-Path $UpdaterDir "target\release\upmc-updater.exe"
if (-not (Test-Path $ExePath)) {
    Write-Host "[错误] 找不到编译产物" -ForegroundColor Red
    exit 1
}

# ── 复制到 dist/ ──
Write-Host "[2/2] 复制到 dist/..." -ForegroundColor Yellow

New-Item -ItemType Directory -Path $DistDir -Force | Out-Null
$OutputExe = Join-Path $DistDir "我的服务器.exe"
Copy-Item $ExePath $OutputExe -Force

$size = [math]::Round((Get-Item $OutputExe).Length / 1MB, 2)

Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host "  编译完成！" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
Write-Host ""
Write-Host "输出: $OutputExe"
Write-Host "大小: $size MB"
Write-Host ""
Write-Host "将此文件发给玩家即可。" -ForegroundColor Green
Write-Host "玩家双击后会自动下载 Java、PCL2、模组等所有组件。" -ForegroundColor Green
