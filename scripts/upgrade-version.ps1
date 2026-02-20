# ============================================================
# upgrade-version.ps1 — MC/Fabric 版本升级助手
# ============================================================
# 使用方法：在仓库根目录执行
#   .\scripts\upgrade-version.ps1
#   .\scripts\upgrade-version.ps1 -McVersion 1.21.12 -FabricVersion 0.18.5
#
# 流程：
#   1. 修改 pack.toml 中的 MC/Fabric 版本
#   2. packwiz update --all 更新所有 mod
#   3. packwiz refresh 刷新索引哈希
#   4. 显示变更摘要，确认后提交
# ============================================================

param(
    [string]$McVersion
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path $PSScriptRoot -Parent
$PackToml = Join-Path $RepoRoot "packwiz\pack.toml"
$PackwizDir = Join-Path $RepoRoot "packwiz"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  MC/Fabric 版本升级助手" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# ── 检查 packwiz 是否可用 ──
if (-not (Get-Command packwiz -ErrorAction SilentlyContinue)) {
    Write-Host "[错误] 找不到 packwiz，请先安装：" -ForegroundColor Red
    Write-Host "  go install github.com/packwiz/packwiz@latest" -ForegroundColor Yellow
    exit 1
}

# ── 读取当前版本 ──
$content = Get-Content $PackToml -Raw
$currentMc = if ($content -match 'minecraft\s*=\s*"([^"]+)"') { $Matches[1] } else { "未知" }
$currentFabric = if ($content -match 'fabric\s*=\s*"([^"]+)"') { $Matches[1] } else { "未知" }

Write-Host "当前版本: MC $currentMc / Fabric $currentFabric" -ForegroundColor Gray
Write-Host ""

# ── 获取目标 MC 版本 ──
if (-not $McVersion) {
    $McVersion = Read-Host "新 MC 版本（留空保持 $currentMc）"
    if ([string]::IsNullOrWhiteSpace($McVersion)) { $McVersion = $currentMc }
}

# ── 自动获取最新 Fabric Loader 版本 ──
Write-Host "正在获取最新 Fabric Loader 版本..." -ForegroundColor Gray
try {
    $fabricMeta = Invoke-RestMethod -Uri "https://meta.fabricmc.net/v2/versions/loader" -TimeoutSec 15
    $FabricVersion = ($fabricMeta | Where-Object { $_.stable -eq $true } | Select-Object -First 1).version
    if (-not $FabricVersion) {
        # 没有 stable 标记就取第一个
        $FabricVersion = $fabricMeta[0].version
    }
    Write-Host "  最新稳定版: $FabricVersion" -ForegroundColor Green
} catch {
    Write-Host "[警告] 无法获取 Fabric 版本，保持当前: $currentFabric" -ForegroundColor Yellow
    $FabricVersion = $currentFabric
}

if ($McVersion -eq $currentMc -and $FabricVersion -eq $currentFabric) {
    Write-Host "版本未变更，仅刷新 mod 和索引。" -ForegroundColor Yellow
} else {
    Write-Host ""
    Write-Host "升级: MC $currentMc -> $McVersion / Fabric $currentFabric -> $FabricVersion" -ForegroundColor Green
}

# ── 步骤 1: 修改 pack.toml ──
Write-Host ""
Write-Host "[1/4] 更新 pack.toml ..." -ForegroundColor Yellow

$newContent = $content `
    -replace ('minecraft\s*=\s*"[^"]+"'), "minecraft = `"$McVersion`"" `
    -replace ('fabric\s*=\s*"[^"]+"'), "fabric = `"$FabricVersion`""

[System.IO.File]::WriteAllText($PackToml, $newContent, [System.Text.UTF8Encoding]::new($false))
Write-Host "  pack.toml 已更新" -ForegroundColor Green

# ── 步骤 2: packwiz update --all ──
Write-Host ""
Write-Host "[2/4] 更新所有 mod (packwiz update --all) ..." -ForegroundColor Yellow

Push-Location $PackwizDir
try {
    packwiz update --all
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[警告] 部分 mod 可能没有适配 MC $McVersion 的版本" -ForegroundColor Yellow
        Write-Host "  请手动检查失败的 mod，可能需要移除或替换" -ForegroundColor Yellow
    } else {
        Write-Host "  所有 mod 已更新" -ForegroundColor Green
    }
} finally {
    Pop-Location
}

# ── 步骤 3: packwiz refresh ──
Write-Host ""
Write-Host "[3/4] 刷新索引 (packwiz refresh) ..." -ForegroundColor Yellow

Push-Location $PackwizDir
try {
    packwiz refresh
    Write-Host "  索引已刷新" -ForegroundColor Green
} finally {
    Pop-Location
}

# ── 步骤 4: 显示变更摘要 ──
Write-Host ""
Write-Host "[4/4] 变更摘要:" -ForegroundColor Yellow

Push-Location $RepoRoot
try {
    git diff --stat
    Write-Host ""

    $confirm = Read-Host "是否提交并推送？(Y/n)"
    if ($confirm -eq "" -or $confirm -match "^[Yy]") {
        $commitMsg = "升级到 MC $McVersion / Fabric $FabricVersion"
        git add -A
        git commit -m $commitMsg
        Write-Host ""
        Write-Host "已提交: $commitMsg" -ForegroundColor Green

        $push = Read-Host "推送到远程？(Y/n)"
        if ($push -eq "" -or $push -match "^[Yy]") {
            git push
            Write-Host "已推送，等待 GitHub Pages 部署..." -ForegroundColor Green
        }
    } else {
        Write-Host "已跳过提交。变更保留在工作区中。" -ForegroundColor Yellow
    }
} finally {
    Pop-Location
}

Write-Host ""
Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  完成！" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
