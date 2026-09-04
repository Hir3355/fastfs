[CmdletBinding()]
param(
    [string]$DestinationRoot
)

$ErrorActionPreference = 'Stop'
$minimumPowerShellVersion = [version]'7.6'
if ($PSVersionTable.PSVersion -lt $minimumPowerShellVersion) {
    throw "FastFsにはPowerShell $minimumPowerShellVersion 以降が必要です"
}
if (-not $IsWindows -or [Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne 'X64') {
    throw 'FastFsの配布版はWindows x64に対応しています'
}

$headers = @{
    Accept = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
    'User-Agent' = 'FastFs-installer'
}
$release = Invoke-RestMethod `
    -Uri 'https://api.github.com/repos/Hir3355/fastfs/releases/latest' `
    -Headers $headers
$archiveAssets = @($release.assets | Where-Object {
    $_.name -match '^FastFs-[0-9]+\.[0-9]+\.[0-9]+-win-x64\.zip$'
})
if ($archiveAssets.Count -ne 1) {
    throw '最新リリースのFastFs配布ZIPを特定できませんでした'
}
$archiveAsset = $archiveAssets[0]
$digestMatch = [regex]::Match(
    [string]$archiveAsset.digest,
    '\Asha256:([0-9a-fA-F]{64})\z')
if (-not $digestMatch.Success) {
    throw '最新リリースからSHA-256値を取得できませんでした'
}

$temporaryDirectory = Join-Path `
    ([IO.Path]::GetTempPath()) `
    "FastFs-install-$([guid]::NewGuid().ToString('N'))"
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
try {
    $archivePath = Join-Path $temporaryDirectory $archiveAsset.name
    Invoke-WebRequest `
        -Uri $archiveAsset.browser_download_url `
        -OutFile $archivePath `
        -Headers @{ 'User-Agent' = 'FastFs-installer' }
    $expectedHash = $digestMatch.Groups[1].Value
    $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
    if (-not $actualHash.Equals($expectedHash, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'ダウンロードしたFastFs配布ZIPのSHA-256値が一致しません'
    }

    $sourceDirectory = Join-Path $temporaryDirectory 'FastFs'
    [IO.Directory]::CreateDirectory($sourceDirectory) | Out-Null
    Expand-Archive -LiteralPath $archivePath -DestinationPath $sourceDirectory
    $manifestPath = Join-Path $sourceDirectory 'FastFs.psd1'
    $manifest = Import-PowerShellDataFile -LiteralPath $manifestPath

    if ([string]::IsNullOrWhiteSpace($DestinationRoot)) {
        $documents = [Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments)
        if ([string]::IsNullOrWhiteSpace($documents)) {
            throw '現在のユーザーのドキュメントディレクトリを取得できませんでした'
        }
        $moduleRoot = Join-Path $documents 'PowerShell\Modules\FastFs'
    } else {
        $moduleRoot = [IO.Path]::GetFullPath($DestinationRoot)
    }
    $destination = Join-Path $moduleRoot $manifest.ModuleVersion
    $requiredFiles = @(
        'FastFs.PowerShell.dll',
        'fastfs.dll',
        'FastFs.psd1',
        'LICENSE',
        'THIRD_PARTY_NOTICES.md',
        'RUST-STDLIB-COPYRIGHT.html'
    )
    foreach ($name in $requiredFiles) {
        $sourcePath = Join-Path $sourceDirectory $name
        if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
            throw "配布物に必要なファイルがありません: $name"
        }
    }

    $alreadyInstalled = Test-Path -LiteralPath $destination -PathType Container
    if ($alreadyInstalled) {
        foreach ($name in $requiredFiles) {
            $sourcePath = Join-Path $sourceDirectory $name
            $destinationPath = Join-Path $destination $name
            if (-not (Test-Path -LiteralPath $destinationPath -PathType Leaf) -or
                (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash -ne
                (Get-FileHash -LiteralPath $destinationPath -Algorithm SHA256).Hash) {
                $alreadyInstalled = $false
                break
            }
        }
    }
    if ($alreadyInstalled) {
        Write-Output "FastFs $($manifest.ModuleVersion) はインストール済みです"
        return
    }

    $loadedModule = Get-Module -Name FastFs
    if ($loadedModule) {
        throw 'FastFsが現在のPowerShellで使用中です。新しいPowerShellを開いてインストールを再実行してください'
    }
    [IO.Directory]::CreateDirectory($destination) | Out-Null

    $obsoleteFormatFile = Join-Path $destination 'FastFs.Format.ps1xml'
    if (Test-Path -LiteralPath $obsoleteFormatFile) {
        Remove-Item -LiteralPath $obsoleteFormatFile
    }
    foreach ($name in $requiredFiles) {
        $sourcePath = Join-Path $sourceDirectory $name
        try {
            Copy-Item -LiteralPath $sourcePath -Destination $destination
        } catch [IO.IOException] {
            throw 'FastFsのファイルが使用中です。FastFsを使用しているPowerShellをすべて閉じてから再実行してください'
        }
    }

    Import-Module (Join-Path $destination 'FastFs.psd1') -Force
    Write-Output "FastFs $($manifest.ModuleVersion) をインストールしました"
} finally {
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse
    }
}
