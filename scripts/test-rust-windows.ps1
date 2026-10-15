# 仅供 Windows CI：Rust 测试/示例没有主程序的资源清单，Tauri 原生控件需要显式启用 v6。
# 不处理发布目录中的程序，只修改本次 Cargo 返回、位于仓库 target 内的测试产物。
param([switch]$Smoke, [switch]$LayoutSmoke)
$ErrorActionPreference = 'Stop'
$root = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$target = [System.IO.Path]::GetFullPath((Join-Path $root 'src-tauri\target')) + [System.IO.Path]::DirectorySeparatorChar
$manifest = Join-Path $PSScriptRoot 'windows-tests.manifest'
$kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
$mt = Get-ChildItem -LiteralPath $kits -Recurse -File -Filter 'mt.exe' |
    Where-Object { $_.FullName -match '\\x64\\mt\.exe$' } |
    Sort-Object FullName | Select-Object -Last 1
if (-not $mt) { throw '找不到 Windows SDK 的 mt.exe' }

Push-Location $root
try {
    if ($Smoke -or $LayoutSmoke) {
        $example = if ($LayoutSmoke) { 'reg_layout_smoke' } else { 'reg_driver_smoke' }
        $output = & cargo build --locked --manifest-path src-tauri/Cargo.toml --example $example --message-format=json
    } else {
        $output = & cargo test --locked --manifest-path src-tauri/Cargo.toml --no-run --message-format=json
    }
    $compileCode = $LASTEXITCODE
    $records = @($output | ForEach-Object {
        try { $v = $_ | ConvertFrom-Json } catch { return }
        if ($v.reason -eq 'compiler-message' -and $v.message.rendered) {
            Write-Host $v.message.rendered
        }
        $v
    })
    if ($compileCode -ne 0) { exit $compileCode }
    $artifacts = @($records | ForEach-Object {
        $v = $_
        if ($v.reason -ne 'compiler-artifact' -or -not $v.executable) { return }
        if ((($Smoke -or $LayoutSmoke) -and $v.target.name -eq $example) -or (-not ($Smoke -or $LayoutSmoke) -and $v.profile.test)) {
            $v.executable
        }
    } | Sort-Object -Unique)
    if ($artifacts.Count -eq 0) { throw 'Cargo 没有返回预期的测试程序' }
    foreach ($exe in $artifacts) {
        $path = [System.IO.Path]::GetFullPath($exe)
        if (-not $path.StartsWith($target, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "测试产物不在仓库 target 目录内：$path"
        }
        Write-Host "嵌入测试清单：$path"
        & $mt.FullName -nologo -manifest $manifest "-outputresource:$path;#1"
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        # 直接执行刚刚嵌入清单的产物。再次 cargo test 可能重新链接并覆盖清单。
        & $path
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    if (-not ($Smoke -or $LayoutSmoke)) {
        & cargo test --locked --manifest-path src-tauri/Cargo.toml --doc
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    exit 0
} finally {
    Pop-Location
}
