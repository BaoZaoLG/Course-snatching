$ErrorActionPreference = "Stop"

$audit = Get-Command cargo-audit -ErrorAction SilentlyContinue
if (-not $audit) {
    $audit = Get-Command cargo-audit.exe -ErrorAction SilentlyContinue
}
if (-not $audit) {
    throw "未找到 cargo-audit。请先运行：cargo install cargo-audit --locked"
}

# 任何版本的 quick-xml 进入 Windows 依赖树都不应被忽略。
$windowsTree = cargo tree --target x86_64-pc-windows-msvc -i quick-xml 2>&1 | Out-String
if ($windowsTree -match '(?m)^quick-xml v') {
    throw "quick-xml 已进入 Windows 构建树，请修复依赖后再忽略相关公告：`n$windowsTree"
}

cargo audit `
    --ignore RUSTSEC-2026-0194 `
    --ignore RUSTSEC-2026-0195
if ($LASTEXITCODE -ne 0) {
    throw "依赖安全审计失败"
}

Write-Output "安全审计通过（Windows 树中无 quick-xml）"
