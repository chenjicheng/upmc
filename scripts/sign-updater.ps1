# ============================================================
# sign-updater.ps1 — 可选 Authenticode 签名
# ============================================================
#
# GitHub Actions 中如配置以下 Secrets，则会自动签名 updater.exe：
#   WINDOWS_SIGN_CERT_BASE64  PFX 证书的 base64 内容
#   WINDOWS_SIGN_CERT_PASSWORD PFX 证书密码
#
# 未配置证书时脚本会跳过签名，不阻塞普通构建。
# ============================================================

param(
    [Parameter(Mandatory = $true)]
    [string]$FilePath
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $FilePath)) {
    throw "待签名文件不存在: $FilePath"
}

$certBase64 = $env:WINDOWS_SIGN_CERT_BASE64
$certPassword = $env:WINDOWS_SIGN_CERT_PASSWORD

if ([string]::IsNullOrWhiteSpace($certBase64) -or [string]::IsNullOrWhiteSpace($certPassword)) {
    Write-Host "未配置签名证书，跳过 Authenticode 签名。"
    exit 0
}

$tempDir = Join-Path $env:RUNNER_TEMP "upmc-signing"
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
$pfxPath = Join-Path $tempDir "codesign.pfx"

try {
    [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($certBase64))

    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe |
        Where-Object { $_.FullName -match "\\x64\\signtool\.exe$" } |
        Sort-Object FullName -Descending |
        Select-Object -First 1

    if (-not $signtool) {
        throw "找不到 signtool.exe"
    }

    & $signtool.FullName sign `
        /f $pfxPath `
        /p $certPassword `
        /fd SHA256 `
        /tr https://timestamp.digicert.com `
        /td SHA256 `
        $FilePath

    if ($LASTEXITCODE -ne 0) {
        throw "signtool 签名失败，退出码: $LASTEXITCODE"
    }

    $signature = Get-AuthenticodeSignature $FilePath
    if ($signature.Status -ne "Valid") {
        throw "签名状态无效: $($signature.Status)"
    }

    Write-Host "Authenticode 签名完成: $FilePath"
}
finally {
    Remove-Item $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
