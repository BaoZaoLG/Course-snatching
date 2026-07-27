$ErrorActionPreference = "Stop"

# cargo-deny 取代 cargo-audit：后者读 Cargo.lock 且没有 target 过滤能力，
# 只能全局 --ignore。真正的风险不是那两条公告本身（SECURITY.md 的分析是对的：
# quick-xml 经 wayland-scanner 进入 lock 文件，Windows 目标不编译它），而是
# 长期挂着的 ignore 列表会变成习惯——将来同类的「仅 Linux 依赖」公告会被顺手
# 加进去，真正该管的那条也就一起放过了。按 target 裁剪后（见 deny.toml），
# ignore 列表是空的。

$deny = Get-Command cargo-deny -ErrorAction SilentlyContinue
if (-not $deny) {
    $deny = Get-Command cargo-deny.exe -ErrorAction SilentlyContinue
}
if (-not $deny) {
    throw "未找到 cargo-deny。请先运行：cargo install cargo-deny --locked"
}

# 兜底断言：即便 deny.toml 的 target 裁剪将来被改坏，这一条仍会拦住
# quick-xml 真的进入 Windows 构建树的情况。
$windowsTree = cargo tree --target x86_64-pc-windows-msvc -i quick-xml 2>&1 | Out-String
if ($windowsTree -match '(?m)^quick-xml v') {
    throw "quick-xml 已进入 Windows 构建树，请修复依赖：`n$windowsTree"
}

cargo deny check
if ($LASTEXITCODE -ne 0) {
    throw "cargo-deny 检查失败（公告 / 许可证 / 依赖来源 / 重复依赖）"
}

Write-Output "依赖检查通过（按 Windows 目标裁剪，无需 ignore 任何公告）"
