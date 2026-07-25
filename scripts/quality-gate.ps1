$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

function Invoke-CargoStep {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    Write-Output $Label
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Label 失败"
    }
}

Push-Location $root
try {
    Invoke-CargoStep "检查格式..." @("fmt", "--all", "--", "--check")
    Invoke-CargoStep "运行 Clippy..." @("clippy", "--all-targets", "--all-features", "--", "-D", "warnings")
    Invoke-CargoStep "运行全部目标测试..." @("test", "--all-targets")
    Invoke-CargoStep "执行 Release 构建..." @("build", "--release")

    Write-Output "执行依赖安全审计..."
    & (Join-Path $PSScriptRoot "audit.ps1")
    if ($LASTEXITCODE -ne 0) {
        throw "依赖安全审计失败"
    }

    Write-Output "检查 Cargo 包内容..."
    $files = @(& cargo package --list --allow-dirty)
    if ($LASTEXITCODE -ne 0) {
        throw "读取 Cargo 包内容失败"
    }
    $normalized = $files | ForEach-Object { $_ -replace '\\', '/' }
    $forbidden = @(
        'config.toml',
        'Course-snatching.exe',
        'Course-snatching-new.exe',
        'src.rar',
        'runtime/debug'
    )
    foreach ($item in $forbidden) {
        if ($normalized | Select-String -SimpleMatch $item) {
            throw "Cargo 包含禁止内容：$item"
        }
    }

    Write-Output "完整质量门禁通过"
}
finally {
    Pop-Location
}
