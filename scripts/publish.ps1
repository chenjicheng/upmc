# ============================================================
# publish.ps1 — 一键发布到分发仓库
# ============================================================
# 使用方法：在管理端仓库根目录执行
#   .\scripts\publish.ps1
#
# 流程：
#   1. 从 packwiz/ 目录收集索引文件
#   2. 复制 server.json
#   3. 推送到分发仓库 (upmc-dist)
#
# 前提：
#   - 分发仓库已 clone 到与管理端同级目录（../upmc-dist）
#   - 分发仓库已配置 GitHub Pages
# ============================================================

param(
    # 分发仓库的本地路径（默认在管理端旁边）
    [string]$DistRepo = (Join-Path (Split-Path $PSScriptRoot -Parent) "..\upmc-dist"),

    # 提交信息（默认带时间戳）
    [string]$Message = "update: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path $PSScriptRoot -Parent
$PackwizDir = Join-Path $RepoRoot "packwiz"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  UPMC 发布脚本" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# ── 检查前提条件 ──
if (-not (Test-Path $PackwizDir)) {
    Write-Host "[错误] packwiz 目录不存在: $PackwizDir" -ForegroundColor Red
    exit 1
}

if (-not (Test-Path $DistRepo)) {
    Write-Host "[错误] 分发仓库不存在: $DistRepo" -ForegroundColor Red
    Write-Host "请先 clone 分发仓库："
    Write-Host "  git clone https://github.com/YOUR_USERNAME/upmc-dist.git $DistRepo"
    exit 1
}

$ServerJson = Join-Path $RepoRoot "server.json"
if (-not (Test-Path $ServerJson)) {
    Write-Host "[错误] server.json 不存在" -ForegroundColor Red
    exit 1
}

# ── 步骤 1: 清理分发仓库中的旧文件（保留 .git） ──
Write-Host "[1/4] 清理分发仓库旧文件..." -ForegroundColor Yellow

Get-ChildItem -Path $DistRepo -Exclude ".git" | Remove-Item -Recurse -Force

# ── 步骤 2: 复制 packwiz 索引文件 ──
Write-Host "[2/4] 复制 packwiz 索引文件..." -ForegroundColor Yellow

# 复制 packwiz 目录下的所有文件（保持目录结构）
Get-ChildItem -Path $PackwizDir -Recurse | ForEach-Object {
    $relativePath = $_.FullName.Substring($PackwizDir.Length + 1)
    $destPath = Join-Path $DistRepo $relativePath

    if ($_.PSIsContainer) {
        New-Item -ItemType Directory -Path $destPath -Force | Out-Null
    }
    else {
        $destDir = Split-Path $destPath -Parent
        if (-not (Test-Path $destDir)) {
            New-Item -ItemType Directory -Path $destDir -Force | Out-Null
        }
        Copy-Item $_.FullName $destPath -Force
    }
}

# ── 步骤 3: 复制 server.json ──
Write-Host "[3/4] 复制 server.json..." -ForegroundColor Yellow

Copy-Item $ServerJson (Join-Path $DistRepo "server.json") -Force

# ── 步骤 4: 提交并推送 ──
Write-Host "[4/4] 提交并推送到远程..." -ForegroundColor Yellow

Push-Location $DistRepo
try {
    git add -A
    $changes = git status --porcelain
    if ($changes) {
        git commit -m $Message
        git push
        Write-Host ""
        Write-Host "[完成] 发布成功！" -ForegroundColor Green
        Write-Host "GitHub Pages 将在几分钟内自动部署。" -ForegroundColor Green
    }
    else {
        Write-Host ""
        Write-Host "[跳过] 没有变更需要发布。" -ForegroundColor Yellow
    }
}
finally {
    Pop-Location
}
