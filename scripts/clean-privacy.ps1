$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$debug = Join-Path $root "runtime\debug"
if (Test-Path $debug) {
    Get-ChildItem -LiteralPath $debug -File | Where-Object { $_.Name -ne ".keep" } | Remove-Item -Force
    Write-Output "已清理 runtime/debug 调试残留"
} else {
    Write-Output "无需清理"
}
# Remove accidental root exes
foreach ($name in @("Course-snatching.exe", "Course-snatching-new.exe")) {
    $p = Join-Path $root $name
    if (Test-Path -LiteralPath $p) {
        Remove-Item -LiteralPath $p -Force
        Write-Output "已删除根目录 $name"
    }
}
