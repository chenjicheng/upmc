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
Write-Host "[1/3] 编译 Release..." -ForegroundColor Yellow

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
Write-Host "[2/3] 复制到 dist/..." -ForegroundColor Yellow

New-Item -ItemType Directory -Path $DistDir -Force | Out-Null
$OutputExe = Join-Path $DistDir "我的服务器.exe"
Copy-Item $ExePath $OutputExe -Force

$size = [math]::Round((Get-Item $OutputExe).Length / 1MB, 2)

# ── 计算 SHA256 并更新 server.json ──
Write-Host "[3/3] 更新 server.json 中的 updater_sha256..." -ForegroundColor Yellow

$hash = (Get-FileHash $OutputExe -Algorithm SHA256).Hash.ToLower()

$ServerJson = Join-Path $RepoRoot "server.json"
if (Test-Path $ServerJson) {
    # 使用正则替换，避免 ConvertFrom-Json/ConvertTo-Json 破坏格式和中文
    $content = [System.IO.File]::ReadAllText($ServerJson, [System.Text.Encoding]::UTF8)

    if ($content -match '"updater_sha256"\s*:\s*"[^"]*"') {
        $content = $content -replace '"updater_sha256"\s*:\s*"[^"]*"', "`"updater_sha256`": `"$hash`""
    } else {
        Write-Host "  [警告] server.json 中找不到 updater_sha256 字段，请手动添加" -ForegroundColor Yellow
    }

    # 写入时不添加 BOM
    [System.IO.File]::WriteAllText($ServerJson, $content, (New-Object System.Text.UTF8Encoding $false))
    Write-Host "  SHA256: $hash" -ForegroundColor Gray
    Write-Host "  server.json 已更新" -ForegroundColor Green
} else {
    Write-Host "  [警告] server.json 不存在，请手动添加 updater_sha256: $hash" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host "  编译完成！" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
Write-Host ""
Write-Host "输出: $OutputExe"
Write-Host "大小: $size MB"
Write-Host "SHA256: $hash"
Write-Host ""
Write-Host "将此文件发给玩家即可。" -ForegroundColor Green
Write-Host "玩家双击后会自动下载 Java、PCL2、模组等所有组件。" -ForegroundColor Green
