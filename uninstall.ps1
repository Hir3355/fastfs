[CmdletBinding(SupportsShouldProcess, ConfirmImpact = 'Medium')]
param()

$ErrorActionPreference = 'Stop'
$documents = [Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments)
if ([string]::IsNullOrWhiteSpace($documents)) {
    throw '現在のユーザーのドキュメントディレクトリを取得できませんでした'
}

$moduleRoot = [IO.Path]::GetFullPath(
    (Join-Path $documents 'PowerShell\Modules\FastFs'))
if (-not (Test-Path -LiteralPath $moduleRoot -PathType Container)) {
    Write-Output "FastFs はインストールされていません: $moduleRoot"
    return
}

$modulePrefix = $moduleRoot.TrimEnd(
    [IO.Path]::DirectorySeparatorChar,
    [IO.Path]::AltDirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
$loadedFromModuleRoot = Get-Module -All | Where-Object {
    if (-not $_.Path) {
        return $false
    }

    $loadedPath = [IO.Path]::GetFullPath($_.Path)
    return $loadedPath.StartsWith(
        $modulePrefix,
        [StringComparison]::OrdinalIgnoreCase)
}
if ($loadedFromModuleRoot) {
    throw "削除対象の FastFs が現在のプロセスで使用中です。新しいプロセスから pwsh -NoProfile -File '$PSCommandPath' を実行してください"
}

if (-not $PSCmdlet.ShouldProcess(
    $moduleRoot,
    'FastFs の全バージョンを削除')) {
    return
}

Remove-Item -LiteralPath $moduleRoot -Recurse
Write-Output "FastFs の全バージョンを削除しました: $moduleRoot"
