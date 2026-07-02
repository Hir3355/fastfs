[CmdletBinding()]
param(
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$projectRoot = $PSScriptRoot
$sourceDirectory = Join-Path $projectRoot 'dist\FastFs'

if (-not $SkipBuild) {
    & (Join-Path $projectRoot 'build.ps1')
}

$manifestPath = Join-Path $sourceDirectory 'FastFs.psd1'
$manifest = Import-PowerShellDataFile -LiteralPath $manifestPath
$documents = [Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments)
if ([string]::IsNullOrWhiteSpace($documents)) {
    throw '現在のユーザーのドキュメントディレクトリを取得できませんでした'
}

$destination = Join-Path $documents "PowerShell\Modules\FastFs\$($manifest.ModuleVersion)"
[IO.Directory]::CreateDirectory($destination) | Out-Null
$destinationPrefix = $destination + [IO.Path]::DirectorySeparatorChar
$loadedFromDestination = Get-Module | Where-Object {
    $_.Path -and [IO.Path]::GetFullPath($_.Path).StartsWith(
        $destinationPrefix,
        [StringComparison]::OrdinalIgnoreCase)
}
if ($loadedFromDestination) {
    throw "更新対象の FastFs が現在のプロセスで使用中です。新しいプロセスから pwsh -NoProfile -File '$PSCommandPath' -SkipBuild を実行してください"
}
$obsoleteFormatFile = Join-Path $destination 'FastFs.Format.ps1xml'
if (Test-Path -LiteralPath $obsoleteFormatFile) {
    Remove-Item -LiteralPath $obsoleteFormatFile
}
foreach ($name in @(
    'FastFs.PowerShell.dll',
    'fastfs.dll',
    'FastFs.psd1',
    'LICENSE',
    'THIRD_PARTY_NOTICES.md'
)) {
    Copy-Item -LiteralPath (Join-Path $sourceDirectory $name) -Destination $destination
}

Write-Output "FastFs $($manifest.ModuleVersion) をインストールしました: $destination"
