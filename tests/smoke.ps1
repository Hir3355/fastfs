[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$moduleManifest = Join-Path $projectRoot 'dist\FastFs\FastFs.psd1'
$thirdPartyNotices = Join-Path $projectRoot 'dist\FastFs\THIRD_PARTY_NOTICES.md'
$rustStdCopyright = Join-Path $projectRoot 'dist\FastFs\RUST-STDLIB-COPYRIGHT.html'
$targetRoot = [IO.Path]::GetFullPath((Join-Path $projectRoot 'target'))
$fixtureRoot = [IO.Path]::GetFullPath((Join-Path $targetRoot 'smoke-fixture'))

foreach ($noticePath in @($thirdPartyNotices, $rustStdCopyright)) {
    if (-not (Test-Path -LiteralPath $noticePath -PathType Leaf)) {
        throw "配布用のライセンス通知が見つかりません: $noticePath"
    }
}
$thirdPartyText = [IO.File]::ReadAllText($thirdPartyNotices)
if (-not $thirdPartyText.Contains('UNICODE, INC. LICENSE AGREEMENT - DATA FILES AND SOFTWARE')) {
    throw 'Unicodeライセンスが第三者通知に含まれていません'
}

if (-not $fixtureRoot.StartsWith(
    $targetRoot + [IO.Path]::DirectorySeparatorChar,
    [StringComparison]::OrdinalIgnoreCase)) {
    throw "テスト用ディレクトリが target の外側です: $fixtureRoot"
}
if (Test-Path -LiteralPath $fixtureRoot) {
    Remove-Item -LiteralPath $fixtureRoot -Recurse
}

New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
Set-Content -LiteralPath (Join-Path $fixtureRoot 'one.txt') -Value @('alpha', 'beta') -Encoding utf8NoBOM
Set-Content -LiteralPath (Join-Path $fixtureRoot 'two.log') -Value 'log' -Encoding utf8NoBOM

Import-Module $moduleManifest -Force

$listing = @(fastfs ls $fixtureRoot)
if ($listing.Count -ne 2 -or $listing[0].GetType().FullName -ne 'FastFs.PowerShell.FastFsEntry') {
    throw 'ls の結果が不正です'
}

$found = @(fastfs find $fixtureRoot -name '*.txt')
if ($found.Count -ne 1 -or $found[0].Name -ne 'one.txt') {
    throw 'find の結果が不正です'
}

$touchedPath = Join-Path $fixtureRoot 'new.txt'
$null = fastfs touch $touchedPath
if (-not (Test-Path -LiteralPath $touchedPath)) {
    throw 'touch がファイルを作成しませんでした'
}

$content = @(fastfs cat (Join-Path $fixtureRoot 'one.txt'))
if ($content.Count -ne 2 -or $content[0] -ne 'alpha' -or $content[1] -ne 'beta') {
    throw 'cat の結果が不正です'
}

$rangePath = Join-Path $fixtureRoot 'range.txt'
$rangeLines = [string[]](1..30 | ForEach-Object { "line-$($_.ToString('D2'))" })
[IO.File]::WriteAllLines($rangePath, $rangeLines, [Text.UTF8Encoding]::new($false))
$expectedRange = [string[]]$rangeLines[9..19]
$rangeWithN = [string[]]@(fastfs sed -n '10,20p' $rangePath)
$rangeWithoutN = [string[]]@(fastfs sed '10,20p' $rangePath)
if (-not [Linq.Enumerable]::SequenceEqual($rangeWithN, $expectedRange) -or
    -not [Linq.Enumerable]::SequenceEqual($rangeWithoutN, $expectedRange)) {
    throw 'sed の範囲表示または -n の省略動作が不正です'
}

$singleLine = [string[]]@(fastfs sed -n '10p' $rangePath)
$openRange = [string[]]@($rangePath | fastfs sed -n '25,$p')
if ($singleLine.Count -ne 1 -or $singleLine[0] -ne 'line-10' -or
    $openRange.Count -ne 6 -or $openRange[0] -ne 'line-25' -or
    $openRange[-1] -ne 'line-30') {
    throw 'sed の単一行表示、終端省略、またはパイプライン入力が不正です'
}

$lfWithoutFinalNewlinePath = Join-Path $fixtureRoot 'lf-no-final-newline.txt'
[IO.File]::WriteAllText(
    $lfWithoutFinalNewlinePath,
    "先頭`n中央`n終端",
    [Text.UTF8Encoding]::new($false))
$lfRange = [string[]]@(fastfs sed -n '2,$p' $lfWithoutFinalNewlinePath)
if ($lfRange.Count -ne 2 -or $lfRange[0] -ne '中央' -or $lfRange[1] -ne '終端') {
    throw 'sed のLF、Unicode、または終端改行なしの処理が不正です'
}

$invalidSedErrors = @()
$invalidSedOutput = @(fastfs sed -n '20,10p' $rangePath `
    -ErrorAction SilentlyContinue -ErrorVariable +invalidSedErrors)
if ($invalidSedOutput.Count -ne 0 -or $invalidSedErrors.Count -ne 1 -or
    $invalidSedErrors[0].FullyQualifiedErrorId -notlike 'FastFs.InvalidScript*') {
    throw 'sed の不正な範囲が ErrorRecord へ正しく変換されませんでした'
}

$searchRoot = Join-Path $fixtureRoot 'search'
[IO.Directory]::CreateDirectory($searchRoot) | Out-Null
$searchPath = Join-Path $searchRoot 'sample.txt'
[IO.File]::WriteAllLines(
    $searchPath,
    [string[]]@('before', 'Needle one', 'between', 'Needle two', 'after'),
    [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText(
    (Join-Path $searchRoot 'other.log'),
    "Needle log`n",
    [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText(
    (Join-Path $searchRoot '.hidden.txt'),
    "Needle hidden`n",
    [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText(
    (Join-Path $searchRoot '.ignore'),
    "ignored.txt`n",
    [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText(
    (Join-Path $searchRoot 'ignored.txt'),
    "Needle ignored`n",
    [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText(
    (Join-Path $searchRoot '.rgignore'),
    "rgignored.txt`n",
    [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText(
    (Join-Path $searchRoot 'rgignored.txt'),
    "Needle rgignored`n",
    [Text.UTF8Encoding]::new($false))

$searchMatches = [string[]]@(fastfs rg -n -C 1 'Needle' $searchPath)
$expectedSearchMatches = [string[]]@(
    '1-before',
    '2:Needle one',
    '3-between',
    '4:Needle two',
    '5-after'
)
if (-not [Linq.Enumerable]::SequenceEqual($searchMatches, $expectedSearchMatches)) {
    throw 'rg の行番号または前後文脈が不正です'
}

$lineBoundaryMatches = [string[]]@(fastfs rg -n '^Needle one\r?$' $searchPath)
if ($lineBoundaryMatches.Count -ne 1 -or $lineBoundaryMatches[0] -ne '2:Needle one') {
    throw 'rg の行境界matcher設定が不正です'
}

$crlfExactPath = Join-Path $searchRoot 'crlf-exact.txt'
[IO.File]::WriteAllText(
    $crlfExactPath,
    "exact-line`r`nexact-line-suffix`r`n",
    [Text.UTF8Encoding]::new($false))
$crlfExactMatches = [string[]]@(fastfs rg -n -LineRegexp 'exact-line' $crlfExactPath)
if ($crlfExactMatches.Count -ne 1 -or $crlfExactMatches[0] -ne '1:exact-line') {
    throw 'rg のCRLFに対する行全体一致が不正です'
}

$literalRegexPath = Join-Path $fixtureRoot 'literal-regex.txt'
[IO.File]::WriteAllText(
    $literalRegexPath,
    "literal.value`nliteralXvalue`nliteralZvalue`n",
    [Text.UTF8Encoding]::new($false))
$regexMetacharMatches = [string[]]@(fastfs rg -n 'literal.value' $literalRegexPath)
$fixedLiteralMatches = [string[]]@(fastfs rg -n -FixedStrings 'literal.value' $literalRegexPath)
$automataRegexMatches = [string[]]@(fastfs rg -n '^literal(?:\.|[X])value$' $literalRegexPath)
if (-not [Linq.Enumerable]::SequenceEqual(
        $regexMetacharMatches,
        [string[]]@('1:literal.value', '2:literalXvalue', '3:literalZvalue')) -or
    -not [Linq.Enumerable]::SequenceEqual($fixedLiteralMatches, [string[]]@('1:literal.value')) -or
    -not [Linq.Enumerable]::SequenceEqual(
        $automataRegexMatches,
        [string[]]@('1:literal.value', '2:literalXvalue'))) {
    throw 'rg の固定文字列、正規表現、またはregex-automata経路が不正です'
}

$utf16Cases = @(
    [PSCustomObject]@{
        Label = 'UTF-16 LE'
        Path = Join-Path $fixtureRoot 'utf16-le.txt'
        Encoding = [Text.UnicodeEncoding]::new($false, $true)
    },
    [PSCustomObject]@{
        Label = 'UTF-16 BE'
        Path = Join-Path $fixtureRoot 'utf16-be.txt'
        Encoding = [Text.UnicodeEncoding]::new($true, $true)
    }
)
foreach ($utf16Case in $utf16Cases) {
    [IO.File]::WriteAllText($utf16Case.Path, "first`nneedle", $utf16Case.Encoding)
    $atStart = [string[]]@(fastfs rg -n '\Afirst' $utf16Case.Path)
    $notAtStart = [string[]]@(fastfs rg -n '\Aneedle' $utf16Case.Path)
    $atEnd = [string[]]@(fastfs rg -n 'needle\z' $utf16Case.Path)
    $notAtEnd = [string[]]@(fastfs rg -n 'first\z' $utf16Case.Path)
    if ($atStart.Count -ne 1 -or $atStart[0] -ne '1:first' -or
        $notAtStart.Count -ne 0 -or
        $atEnd.Count -ne 1 -or $atEnd[0] -ne '2:needle' -or
        $notAtEnd.Count -ne 0) {
        throw "rg の$($utf16Case.Label)に対する \A / \z アンカー処理が不正です"
    }
}

$globMatches = [string[]]@(fastfs rg -n -g '*.txt' 'Needle' $searchRoot)
if ($globMatches.Count -ne 5 -or
    @($globMatches | Where-Object { $_ -match 'sample\.txt:2:Needle one$' }).Count -ne 1 -or
    @($globMatches | Where-Object { $_ -match 'sample\.txt:4:Needle two$' }).Count -ne 1 -or
    @($globMatches | Where-Object { $_ -match '\.hidden\.txt:1:Needle hidden$' }).Count -ne 1 -or
    @($globMatches | Where-Object { $_ -match 'ignored\.txt:1:Needle ignored$' }).Count -ne 1 -or
    @($globMatches | Where-Object { $_ -match 'rgignored\.txt:1:Needle rgignored$' }).Count -ne 1) {
    throw 'rg の再帰走査またはglobが不正です'
}

[IO.File]::WriteAllText(
    (Join-Path $searchRoot 'brace.rs'),
    "Brace token rust`n",
    [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText(
    (Join-Path $searchRoot 'brace.ts'),
    "Brace token typescript`n",
    [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText(
    (Join-Path $searchRoot 'brace.js'),
    "Brace token javascript`n",
    [Text.UTF8Encoding]::new($false))
$braceGlobMatches = [string[]]@(fastfs rg -n -Glob '*.{rs,ts}' 'Brace token' $searchRoot)
if ($braceGlobMatches.Count -ne 2 -or
    @($braceGlobMatches | Where-Object { $_ -match 'brace\.rs:1:Brace token rust$' }).Count -ne 1 -or
    @($braceGlobMatches | Where-Object { $_ -match 'brace\.ts:1:Brace token typescript$' }).Count -ne 1) {
    throw 'rg のbrace glob展開または-Globパラメーターが不正です'
}

$defaultSearchMatches = [string[]]@(fastfs rg -n 'Needle' $searchRoot)
if ($defaultSearchMatches.Count -ne 3 -or
    @($defaultSearchMatches | Where-Object { $_ -match '\.hidden\.txt' }).Count -ne 0 -or
    @($defaultSearchMatches | Where-Object { $_ -match 'ignored\.txt' }).Count -ne 0 -or
    @($defaultSearchMatches | Where-Object { $_ -match 'rgignored\.txt' }).Count -ne 0) {
    throw 'rg の標準的な隠しファイル除外が不正です'
}

$noIgnoreMatches = [string[]]@(fastfs rg -n -NoIgnore 'Needle' $searchRoot)
if (@($noIgnoreMatches | Where-Object { $_ -match 'ignored\.txt:1:Needle ignored$' }).Count -ne 1 -or
    @($noIgnoreMatches | Where-Object { $_ -match 'rgignored\.txt:1:Needle rgignored$' }).Count -ne 1 -or
    @($noIgnoreMatches | Where-Object { $_ -match '\.hidden\.txt' }).Count -ne 0) {
    throw 'rg の.rgignoreまたは-NoIgnoreが不正です'
}

$pipelineMatches = [string[]]@($searchPath | fastfs rg -n -F 'Needle')
if ($pipelineMatches.Count -ne 2 -or $pipelineMatches[0] -ne '2:Needle one') {
    throw 'rg の固定文字列またはパイプライン入力が不正です'
}

$dashPatternPath = Join-Path $searchRoot 'dash.txt'
[IO.File]::WriteAllText(
    $dashPatternPath,
    "-leading-dash`n",
    [Text.UTF8Encoding]::new($false))
$dashPatternMatches = [string[]]@(fastfs rg -n -e '-leading-dash' $dashPatternPath)
$namedDashPatternMatches = [string[]]@(fastfs rg -n -Pattern '-leading-dash' $dashPatternPath)
if ($dashPatternMatches.Count -ne 1 -or $dashPatternMatches[0] -ne '1:-leading-dash' -or
    $namedDashPatternMatches.Count -ne 1 -or $namedDashPatternMatches[0] -ne '1:-leading-dash') {
    throw 'rg の-eまたは-Patternによるハイフン始まりパターンが不正です'
}

$cliCompatPath = Join-Path $fixtureRoot 'rg-cli-compat.txt'
[IO.File]::WriteAllText(
    $cliCompatPath,
    "Needle`nneedle`nneedles`n",
    [Text.UTF8Encoding]::new($false))
$smartCaseMatches = [string[]]@(fastfs rg -n -SmartCase -WordRegexp 'needle' $cliCompatPath)
$smartCaseUpperMatches = [string[]]@(fastfs rg -n -SmartCase -WordRegexp 'Needle' $cliCompatPath)
$maxCountMatches = [string[]]@(fastfs rg -n -MaxCount 1 'needle' $cliCompatPath)
$filesWithMatches = [string[]]@(fastfs rg -FilesWithMatches -WordRegexp 'needle' $cliCompatPath)
$countMatches = [string[]]@(fastfs rg -Count -SmartCase -WordRegexp 'needle' $cliCompatPath)
if (-not [Linq.Enumerable]::SequenceEqual($smartCaseMatches, [string[]]@('1:Needle', '2:needle')) -or
    -not [Linq.Enumerable]::SequenceEqual($smartCaseUpperMatches, [string[]]@('1:Needle')) -or
    -not [Linq.Enumerable]::SequenceEqual($maxCountMatches, [string[]]@('2:needle')) -or
    -not [Linq.Enumerable]::SequenceEqual($filesWithMatches, [string[]]@($cliCompatPath)) -or
    -not [Linq.Enumerable]::SequenceEqual($countMatches, [string[]]@('2'))) {
    throw 'rg の既存CLIオプション互換性が不正です'
}

$searchErrors = @()
$invalidSearch = @(fastfs rg '(' $searchPath `
    -ErrorAction SilentlyContinue -ErrorVariable +searchErrors)
if ($invalidSearch.Count -ne 0 -or $searchErrors.Count -ne 1 -or
    $searchErrors[0].FullyQualifiedErrorId -notlike 'FastFs.InvalidPattern*') {
    throw 'rg の不正な正規表現が ErrorRecord へ正しく変換されませんでした'
}

$prefix = fastfs cat (Join-Path $fixtureRoot 'one.txt') | head -c 7
if ($prefix -ne "alpha$([Environment]::NewLine)b") {
    throw "head -c の結果が不正です: '$prefix'"
}

$unicodePrefix = '日😀本語' | head -c 3
if ($unicodePrefix -ne '日😀本') {
    throw "head -c の文字数処理が不正です: '$unicodePrefix'"
}

$crlfPrefix = "a`r`nb" | head -c 2
if ($crlfPrefix -ne "a`r`n") {
    throw "head -c が CRLF を途中で分割しました: '$crlfPrefix'"
}

$combiningInput = "e$([char]0x0301)x"
$combiningPrefix = $combiningInput | head -c 1
if ($combiningPrefix -ne $combiningInput.Substring(0, 2)) {
    throw "head -c が結合文字を途中で分割しました: '$combiningPrefix'"
}

$joinedEmojiInput = "👩‍💻x"
$joinedEmojiElements = [Globalization.StringInfo]::GetTextElementEnumerator($joinedEmojiInput)
$null = $joinedEmojiElements.MoveNext()
$joinedEmojiPrefix = $joinedEmojiInput | head -c 1
if ($joinedEmojiPrefix -ne $joinedEmojiElements.GetTextElement()) {
    throw "head -c が結合絵文字を途中で分割しました: '$joinedEmojiPrefix'"
}

$batchTextPath = Join-Path $fixtureRoot 'batch-lines.txt'
$batchLines = @(0..1024 | ForEach-Object { "line-$($_.ToString('D4'))" })
Set-Content -LiteralPath $batchTextPath -Value $batchLines -Encoding utf8NoBOM
$batchContent = @(fastfs cat $batchTextPath)
if ($batchContent.Count -ne 1025 -or
    $batchContent[0] -ne 'line-0000' -or
    $batchContent[1024] -ne 'line-1024') {
    throw 'cat のテキストバッチ境界処理が不正です'
}

$batchEntryRoot = Join-Path $fixtureRoot 'batch-entries'
[IO.Directory]::CreateDirectory($batchEntryRoot) | Out-Null
0..256 | ForEach-Object {
    [IO.File]::WriteAllText(
        (Join-Path $batchEntryRoot "entry-$($_.ToString('D4')).txt"),
        '')
}
$batchEntries = @(fastfs ls $batchEntryRoot)
if ($batchEntries.Count -ne 257 -or
    $batchEntries[0].Name -ne 'entry-0000.txt' -or
    $batchEntries[256].Name -ne 'entry-0256.txt') {
    throw 'ls のエントリバッチ境界処理が不正です'
}

$streamRoot = Join-Path $fixtureRoot 'stream-search'
[IO.Directory]::CreateDirectory($streamRoot) | Out-Null
$streamPath = Join-Path $streamRoot 'many.txt'
[IO.File]::WriteAllLines(
    $streamPath,
    [string[]]@(0..699 | ForEach-Object { "stream-match-$($_.ToString('D4'))" }),
    [Text.UTF8Encoding]::new($false))
0..64 | ForEach-Object {
    [IO.File]::WriteAllText((Join-Path $streamRoot "filler-$($_.ToString('D2')).txt"), 'none')
}
$streamPrefix = fastfs rg -n 'stream-match' $streamRoot | head -c 120
if ($streamPrefix.Length -lt 100 -or $streamPrefix -notmatch 'stream-match-0000') {
    throw 'rg のチャンク送信またはhead早期停止が不正です'
}

$streamFirstLines = [string[]]@(fastfs rg -n 'stream-match' $streamRoot | head -n 2)
$streamAfterHead = [string[]]@(fastfs rg -n -MaxCount 1 'stream-match' $streamPath)
if (-not [Linq.Enumerable]::SequenceEqual(
        $streamFirstLines,
        [string[]]@(
            "$streamPath`:1:stream-match-0000",
            "$streamPath`:2:stream-match-0001")) -or
    -not [Linq.Enumerable]::SequenceEqual($streamAfterHead, [string[]]@('1:stream-match-0000'))) {
    throw 'rg のhead停止後の出力または停止状態の復帰が不正です'
}

$cacheRoot = Join-Path $fixtureRoot 'cache-search'
[IO.Directory]::CreateDirectory($cacheRoot) | Out-Null
0..1023 | ForEach-Object {
    [IO.File]::WriteAllText(
        (Join-Path $cacheRoot "cached-$($_.ToString('D4')).txt"),
        'stable')
}
$null = fastfs rg -Count 'cache-new-match' $cacheRoot
$newCachePath = Join-Path $cacheRoot 'new-match.txt'
[IO.File]::WriteAllText($newCachePath, 'cache-new-match')
$cacheMatches = @()
for ($attempt = 0; $attempt -lt 100; $attempt++) {
    $cacheMatches = [string[]]@(fastfs rg -n 'cache-new-match' $cacheRoot)
    if ($cacheMatches.Count -eq 1) {
        break
    }
    [Threading.Thread]::Sleep(20)
}
if ($cacheMatches.Count -ne 1 -or $cacheMatches[0] -notmatch 'new-match\.txt:1:cache-new-match$') {
    throw 'rg の変更監視付きファイル一覧キャッシュが失効しませんでした'
}

$innerCounts = [Collections.Generic.List[int]]::new()
$outerResult = @(1..2 | ForEach-Object {
    $innerContent = @(fastfs cat (Join-Path $fixtureRoot 'one.txt'))
    $innerCounts.Add($innerContent.Count)
    "outer-$_"
} | head -n 1)
if ($outerResult.Count -ne 1 -or
    $innerCounts.Count -ne 2 -or
    $innerCounts[0] -ne 2 -or
    $innerCounts[1] -ne 2) {
    throw '外側の head が内側の FastFs を誤って停止しました'
}

$missingPaths = [string[]](0..7 | ForEach-Object {
    Join-Path $fixtureRoot "missing-$_.txt"
})
$nativeErrors = @()
$mergedError = @(
    fastfs cat $missingPaths -ErrorAction Continue -ErrorVariable +nativeErrors 2>&1 |
        head -n 1
)
if ($mergedError.Count -ne 1 -or
    $mergedError[0] -isnot [Management.Automation.ErrorRecord] -or
    $nativeErrors.Count -ne 1) {
    throw 'ErrorRecord の変換または head によるエラー生成の早期停止が不正です'
}
$contentAfterError = @(fastfs cat (Join-Path $fixtureRoot 'one.txt'))
if ($contentAfterError.Count -ne 2) {
    throw 'head の停止状態が次の FastFs 呼び出しへ残留しました'
}

Write-Output 'FastFs のスモークテストに成功しました'
