# Wong Tools 一键打包：release 构建 → 便携 ZIP → Inno 安装程序。
# 用法（仓库根）：powershell -ExecutionPolicy Bypass -File packaging\build.ps1
# 要求：VS2022 生成工具（link.exe）、Inno Setup 6（ISCC.exe）。
# 版本号只认 Cargo.toml，传给 ISCC 做安装包版本，产物顺手打 SHA-256。
# PowerShell 5.1 能跑，别用 7 专有语法。
# 文件带 UTF-8 BOM：5.1 靠它认中文，存盘时别去掉。

$ErrorActionPreference = 'Stop'

# 脚本在 packaging\ 下，上一级就是仓库根；后面全用绝对路径，不 cd。
$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'Cargo.toml'
$exe = Join-Path $root 'target\release\wong-tools.exe'
$stage = Join-Path $root 'dist\windows'
$zip = Join-Path $root 'dist\wong-tools-windows-x64.zip'
$iss = Join-Path $root 'packaging\wong-tools.iss'

# 版本号只认 [package] 段的 version（依赖里的 version 行一律无视）。
$version = $null
$inPackage = $false
foreach ($line in (Get-Content $manifest)) {
    if ($line -match '^\[(.+)\]$') {
        $inPackage = $Matches[1] -eq 'package'
    } elseif ($inPackage -and ($line -match '^version = "(.+)"$')) {
        $version = $Matches[1]
        break
    }
}
if ([string]::IsNullOrEmpty($version)) {
    throw 'Cargo.toml 的 [package] 里找不到 version，版本号没法定'
}
Write-Host "打包版本：$version"

# release 构建：--locked 保证和 Cargo.lock 一致，产物可复现。
& cargo build --release --locked --manifest-path $manifest
if ($LASTEXITCODE -ne 0) {
    throw "cargo build 失败，退出码 $LASTEXITCODE"
}
if (-not (Test-Path -LiteralPath $exe)) {
    throw "构建完找不到 $exe"
}

# 摆盘：单 exe 程序，字体/主题/文案全在二进制里，只拷它一个。
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item -LiteralPath $exe -Destination (Join-Path $stage 'wong-tools.exe') -Force

# 便携 ZIP：解压即用，不写注册表不建快捷方式。
if (Test-Path -LiteralPath $zip) {
    Remove-Item -LiteralPath $zip -Force
}
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
Write-Host "便携包：$zip"

# Inno 编译器定位：PATH 里有就用，没有看两个默认安装路径（按机 / 按用户），
# 都没有就明说。
$iscc = $null
$found = Get-Command ISCC.exe -ErrorAction SilentlyContinue
if ($null -ne $found) {
    $iscc = $found.Source
} else {
    foreach ($candidate in @(
        'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
    )) {
        if (Test-Path -LiteralPath $candidate) {
            $iscc = $candidate
            break
        }
    }
}
if ($null -eq $iscc) {
    throw '找不到 ISCC.exe，请先装 Inno Setup 6（https://jrsoftware.org/isdl.php），装完重跑'
}

# 在仓库根调 ISCC：exe、输出目录、中文语言包都传绝对路径，iss 里不猜相对基准。
$stageExe = Join-Path $stage 'wong-tools.exe'
$distDir = Join-Path $root 'dist'
$langIsl = Join-Path $root 'packaging\ChineseSimplified.isl'
Push-Location $root
try {
    & $iscc "/DMyAppVersion=$version" "/DExePath=$stageExe" "/DOutDir=$distDir" "/DLangIsl=$langIsl" $iss
    if ($LASTEXITCODE -ne 0) {
        throw "ISCC 失败，退出码 $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
$setup = Join-Path $root "dist\WongTools-Setup-$version.exe"
Write-Host "安装包：$setup"

# 校验和：发布时贴出去，用户下载完对一遍。
foreach ($artifact in @($zip, $setup)) {
    $hash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash.ToLower()
    Write-Host "$artifact"
    Write-Host "  SHA256: $hash"
}
