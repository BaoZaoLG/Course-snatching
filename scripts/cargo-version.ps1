# 从 cargo metadata 读取包版本。
#
# package.ps1 与 release-preflight.ps1 原先各自用
# '(?m)^version\s*=\s*"([^"]+)"' 匹配 Cargo.toml 的第一个顶格 version——将来
# 引入 [workspace.package] 就会静默取到错误的那一个，而这两个脚本的全部价值
# 恰恰就在于版本一致性。改用 cargo 自己的元数据，两处共用这一个函数。
function Get-CargoPackageVersion {
    param([Parameter(Mandatory = $true)][string]$Root)

    $json = cargo metadata --format-version 1 --no-deps --manifest-path (Join-Path $Root "Cargo.toml")
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata 执行失败，无法读取版本号"
    }
    $meta = $json | ConvertFrom-Json
    $package = $meta.packages | Where-Object { $_.name -eq "course-snatching" } | Select-Object -First 1
    if (-not $package) {
        $package = $meta.packages | Select-Object -First 1
    }
    if (-not $package -or -not $package.version) {
        throw "无法从 cargo metadata 读取版本号"
    }
    return $package.version
}
