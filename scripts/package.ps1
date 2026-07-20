$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$manifest = Get-Content (Join-Path $root "Cargo.toml") -Raw
$versionMatch = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
    throw "无法从 Cargo.toml 读取版本号"
}
$version = $versionMatch.Groups[1].Value
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$distRoot = Join-Path $root "dist"
$outDir = Join-Path $distRoot "Course-snatching-v$version-windows-x64-$stamp"
$zipPath = "$outDir.zip"

$skipTests = $env:COURSE_SNATCHING_SKIP_TESTS -eq "1"

New-Item -ItemType Directory -Force $distRoot | Out-Null
if ((Test-Path -LiteralPath $outDir) -or (Test-Path -LiteralPath $zipPath)) {
    throw "输出目录已存在，请稍后重试：$outDir"
}

# Warn about stale root binaries that confuse users
foreach ($name in @("Course-snatching.exe", "Course-snatching-new.exe")) {
    $stale = Join-Path $root $name
    if (Test-Path -LiteralPath $stale) {
        Write-Warning "根目录存在过期产物 $name ；发布包以 dist/ 为准。建议删除根目录 EXE。"
    }
}

Push-Location $root
try {
    if (-not $skipTests) {
        Write-Output "运行测试..."
        cargo test
        if ($LASTEXITCODE -ne 0) { throw "测试失败" }
    }

    Write-Output "Release 构建..."
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "Release 构建失败" }

    New-Item -ItemType Directory $outDir | Out-Null
    New-Item -ItemType Directory (Join-Path $outDir "runtime\debug") -Force | Out-Null
    $exeSrc = Join-Path $root "target\release\Course-snatching.exe"
    $exeName = "Course-snatching-v$version-windows-x64.exe"
    Copy-Item -LiteralPath $exeSrc -Destination (Join-Path $outDir $exeName)
    # Keep plain name for convenience
    Copy-Item -LiteralPath $exeSrc -Destination (Join-Path $outDir "Course-snatching.exe")
    Copy-Item -LiteralPath (Join-Path $root "README.md") -Destination $outDir
    Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination $outDir
    Copy-Item -LiteralPath (Join-Path $root "CHANGELOG.md") -Destination $outDir
    Copy-Item -LiteralPath (Join-Path $root "SECURITY.md") -Destination $outDir
    Copy-Item -LiteralPath (Join-Path $root "config.example.toml") -Destination $outDir
    New-Item -ItemType File (Join-Path $outDir "runtime\debug\.keep") | Out-Null

    $rustc = (rustc -V 2>$null)
    $git = ""
    try { $git = (git -C $root rev-parse --short HEAD 2>$null) } catch {}
    $buildInfo = @"
name=Course-snatching
version=$version
target=windows-x64
built_at=$stamp
rustc=$rustc
git=$git
"@
    Set-Content -LiteralPath (Join-Path $outDir "BUILD_INFO.txt") -Value $buildInfo -Encoding UTF8

    # SHA-256 for both exes
    $sums = @()
    Get-ChildItem -LiteralPath $outDir -Filter *.exe | ForEach-Object {
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $sums += "$hash  $($_.Name)"
    }
    
    # 简易 SBOM：记录直接依赖树（cargo tree）
    try {
        $tree = cargo tree --prefix none --edges normal 2>$null | Out-String
        Set-Content -LiteralPath (Join-Path $outDir "SBOM-cargo-tree.txt") -Value $tree -Encoding UTF8
    } catch {
        Write-Warning "SBOM 生成跳过：$($_.Exception.Message)"
    }

    Set-Content -LiteralPath (Join-Path $outDir "SHA256SUMS.txt") -Value ($sums -join "`n") -Encoding ASCII


    # 可选 Authenticode：设置 COURSE_SNATCHING_SIGN_THUMBPRINT 后对 dist 内 EXE 签名
    if ($env:COURSE_SNATCHING_SIGN_THUMBPRINT) {
        $thumb = $env:COURSE_SNATCHING_SIGN_THUMBPRINT
        Get-ChildItem -LiteralPath $outDir -Filter *.exe | ForEach-Object {
            Write-Output "Authenticode 签名 $($_.Name) ..."
            & signtool sign /sha1 $thumb /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 $_.FullName
            if ($LASTEXITCODE -ne 0) { throw "签名失败: $($_.Name)" }
        }
    }

    Compress-Archive -LiteralPath $outDir -DestinationPath $zipPath -CompressionLevel Optimal
    $zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath "$zipPath.sha256" -Value "$zipHash  $(Split-Path $zipPath -Leaf)" -Encoding ASCII
    Write-Output "发布包已生成：$zipPath"
    Write-Output "SHA256: $zipHash"
}
finally {
    Pop-Location
}
