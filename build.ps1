[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release'
)

$ErrorActionPreference = 'Stop'
$projectRoot = $PSScriptRoot
$rustProfile = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
$cargoArguments = @('build', '--package', 'fastfs')
if ($Configuration -eq 'Release') {
    $cargoArguments += '--release'
}

& cargo @cargoArguments
if ($LASTEXITCODE -ne 0) {
    throw "cargo build が終了コード $LASTEXITCODE で失敗しました"
}

$powerShellHome = Split-Path -Parent (Get-Process -Id $PID).Path
$powerShellProject = Join-Path $projectRoot 'powershell\FastFs.PowerShell\FastFs.PowerShell.csproj'
& dotnet build $powerShellProject '--configuration' $Configuration "-p:PowerShellHome=$powerShellHome"
if ($LASTEXITCODE -ne 0) {
    throw "dotnet build が終了コード $LASTEXITCODE で失敗しました"
}

$moduleDirectory = Join-Path $projectRoot 'dist\FastFs'
New-Item -ItemType Directory -Path $moduleDirectory -Force | Out-Null
$obsoleteFormatFile = Join-Path $moduleDirectory 'FastFs.Format.ps1xml'
if (Test-Path -LiteralPath $obsoleteFormatFile) {
    Remove-Item -LiteralPath $obsoleteFormatFile
}
$managedOutput = Join-Path $projectRoot "powershell\FastFs.PowerShell\bin\$Configuration\net10.0"
$rustLibrary = Join-Path $projectRoot "target\$rustProfile\fastfs.dll"
$rustSysroot = (& rustc --print sysroot).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($rustSysroot)) {
    throw 'Rustツールチェーンのsysrootを取得できませんでした'
}
$rustStdCopyright = Join-Path $rustSysroot 'share\doc\rust\COPYRIGHT-library.html'
if (-not (Test-Path -LiteralPath $rustStdCopyright -PathType Leaf)) {
    throw "Rust標準ライブラリの著作権通知が見つかりません: $rustStdCopyright。Rustのドキュメントコンポーネントをインストールしてください"
}

Copy-Item -LiteralPath $rustLibrary -Destination $moduleDirectory
Copy-Item -LiteralPath (Join-Path $managedOutput 'FastFs.PowerShell.dll') -Destination $moduleDirectory
Copy-Item -LiteralPath (Join-Path $projectRoot 'powershell\FastFs.PowerShell\FastFs.psd1') -Destination $moduleDirectory
Copy-Item -LiteralPath (Join-Path $projectRoot 'LICENSE') -Destination $moduleDirectory
Copy-Item -LiteralPath (Join-Path $projectRoot 'THIRD_PARTY_NOTICES.md') -Destination $moduleDirectory
Copy-Item -LiteralPath $rustStdCopyright `
    -Destination (Join-Path $moduleDirectory 'RUST-STDLIB-COPYRIGHT.html')

Write-Output "FastFs モジュールを生成しました: $moduleDirectory"
