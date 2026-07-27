param(
    [string]$Tag = ""
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$manifestPath = Join-Path $root "Cargo.toml"
$mainPath = Join-Path $root "src\main.rs"

$manifest = Get-Content -LiteralPath $manifestPath -Raw
. (Join-Path $PSScriptRoot "cargo-version.ps1")
$version = Get-CargoPackageVersion -Root $root

if ([string]::IsNullOrWhiteSpace($Tag)) {
    $Tag = (& git -C $root describe --tags --exact-match HEAD 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($Tag)) {
        throw "当前提交没有精确 Git tag；请传入 -Tag v$version 或在已打 tag 的提交上运行"
    }
}

$expectedTag = "v$version"
if ($Tag -ne $expectedTag) {
    throw "发布版本不一致：Git tag=$Tag，Cargo.toml=$version（期望 $expectedTag）"
}

$main = Get-Content -LiteralPath $mainPath -Raw
$dynamicTitle = 'const APP_TITLE: &str = concat!("Course-snatching v", env!("CARGO_PKG_VERSION"));'
if (-not $main.Contains($dynamicTitle)) {
    $hardcodedTitle = [regex]::Match($main, 'Course-snatching v(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)')
    if (-not $hardcodedTitle.Success) {
        throw "无法验证程序标题版本；APP_TITLE 必须直接使用 CARGO_PKG_VERSION 或包含明确版本号"
    }
    if ($hardcodedTitle.Groups[1].Value -ne $version) {
        throw "程序标题版本不一致：标题=$($hardcodedTitle.Groups[1].Value)，Cargo.toml=$version"
    }
}

Write-Output "发布预检通过：tag=$Tag version=$version title=Course-snatching v$version"
