# ============================================================
# publish.ps1 — 快速发布：提交变更并推送到 main
# ============================================================
# 使用方法：在仓库根目录执行
#   .\scripts\publish.ps1
#   .\scripts\publish.ps1 -Message "添加xxx模组"
#
# 流程：
#   1. 校验 packwiz/ 和 server.json 存在
#   2. 从 dev 合并到 main
#   3. 推送 main（触发 GitHub Actions 自动部署到 Pages）
#   4. 切回 dev 继续开发
#
# 前提：
#   - GitHub 仓库已启用 Pages (Source: Deploy from branch → gh-pages)
#   - .github/workflows/publish.yml 已配置
# ============================================================

param(
    # 合并提交信息（默认带时间戳）
    [string]$Message = ("release: " + (Get-Date -Format "yyyy-MM-dd HH:mm")),

    # 是否跳过 dev→main 合并，直接在当前分支推送
    [switch]$Direct
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path $PSScriptRoot -Parent

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  UPMC 发布" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# ── 检查前提条件 ──
$ServerJson = Join-Path $RepoRoot "server.json"
if (-not (Test-Path $ServerJson)) {
    Write-Host "[错误] server.json 不存在" -ForegroundColor Red
    exit 1
}

Push-Location $RepoRoot
$currentBranch = git branch --show-current
try {
    # 确保工作区干净
    $dirty = git status --porcelain
    if ($dirty) {
        Write-Host "[1/3] 提交当前变更..." -ForegroundColor Yellow
        git add -A
        git commit -m $Message
    } else {
        Write-Host "[1/3] 工作区干净，无需提交" -ForegroundColor Gray
    }

    if ($Direct) {
        # 直接推送当前分支
        Write-Host "[2/3] 推送当前分支..." -ForegroundColor Yellow
        git push
    } else {
        # dev → main 合并流程
        Write-Host "[2/3] 合并 $currentBranch → main..." -ForegroundColor Yellow
        git checkout main
        git merge $currentBranch -m "merge: $currentBranch → main ($Message)"
        git push origin main

        # 切回开发分支
        Write-Host "[3/3] 切回 $currentBranch..." -ForegroundColor Yellow
        git checkout $currentBranch
    }

    Write-Host ""
    Write-Host "[完成] 已推送到 main，GitHub Actions 将自动部署到 Pages。" -ForegroundColor Green
    Write-Host "部署状态: https://github.com/chenjicheng/upmc/actions" -ForegroundColor Gray
}
finally {
    # 确保切回开发分支（合并失败时可能停留在 main）
    if (-not $Direct -and $currentBranch -and (git branch --show-current) -ne $currentBranch) {
        Write-Host "正在切回 $currentBranch ..." -ForegroundColor Yellow
        git checkout $currentBranch 2>$null
    }
    Pop-Location
}
