# FastFs

FastFs は、Rust のファイル操作を PowerShell プロセス内で実行する Windows 向けモジュールです。`fastfs` で Linux 風のファイル操作と高速なテキスト検索を使えるようになります。
通常のコマンドとしてだけでなく、Claude Code や Codex などのコーディングエージェント向けツールとしても使えます。

## ビルドと導入

PowerShell 7.6以降が必要です。
ソースからビルドする場合は、Rustと.NET 10 SDKも必要です。

```powershell
pwsh -NoProfile -File .\install.ps1
```

グローバル導入したFastFsの全バージョンは、次のコマンドで削除できます。

```powershell
pwsh -NoProfile -File .\uninstall.ps1
```

## 使用例

```powershell
fastfs ls . -R
fastfs touch .\memo.txt
fastfs find . -name '*.txt'
fastfs find . -name '*.txt' | fastfs cat
fastfs sed -n '10,20p' .\memo.txt

fastfs rg -n 'pattern' .
fastfs rg -n -C 3 -g '*.rs' 'pattern' .\crates
fastfs rg -n 'pattern' . | head -c 8000
```

`ls`、`touch`、`find` はPowerShellオブジェクトを返します。`cat`、`sed`、`rg` は文字列を返します。

`sed` は読み取り専用で、`10p`、`10,20p`、`10,$p` の行範囲表示に対応します。`-n` は推奨表記ですが省略しても同じです。

`rg`の主なオプションは`-n`、`-i`、`-S`、`-F`、`-w`、`-x`、`-C`、`-B`、`-AfterContext`、`-g`、`-m`、`-Hidden`、`-NoIgnore`、`-Follow`、`-Text`、`-FilesWithMatches`、`-Count`です。

`head -c` は Unicode 文字数単位で切り詰め、FastFs の上流処理も協調停止します。PowerShell では、後方文脈・件数・ファイル名のみにはそれぞれ `-AfterContext`、`-Count`、`-FilesWithMatches` を使います。

## コーディングエージェントで使う場合

FastFs の利用規則は、必要なときだけ呼び出すスキルではなく、リポジトリで常時参照される `AGENTS.md` または `CLAUDE.md` に書くことを推奨します。
Codex などでは `AGENTS.md`、Claude Code では `CLAUDE.md` に、次のような指示を追加します。

```md
## FastFs

- Assume PowerShell is the shell for this workspace.
- Treat FastFs as a globally installed, in-process PowerShell module.
- Prefer FastFs over external `ls`, `find`, `cat`, `sed`, `rg`, and `head` utilities or PowerShell alternatives whenever FastFs supports the required operation.
- Use `fastfs ls` for directory listings.
- Use `fastfs find ROOT -name 'PATTERN'` for recursive filename searches.
- Use `fastfs cat FILE` to read an entire text file.
- Use `fastfs sed -n 'START,ENDp' FILE` to read a line range.
- Use `fastfs rg -n 'PATTERN' PATH...` for content searches.
- Use `fastfs touch FILE...` to create empty files or update modification times.
- To limit output by character count, use `COMMAND 2>&1 | Out-String -Stream -Width 200 | head -c CHARACTERS`.
- Treat results from `fastfs ls`, `fastfs find`, and `fastfs touch` as objects and use properties such as `Path`, `Name`, and `Length` instead of parsing formatted text.
- Use PowerShell commands or editing tools for unsupported operations such as deletion, copying, moving, and general text editing.
```

ワイルドカードと検索パターンは、PowerShell に解釈されないよう単一引用符で囲みます。
FastFs は PowerShell の構文を置き換えないため、パイプライン、`2>&1`、変数、スクリプトブロックは通常どおり利用できます。

## 速度比較

Intel Core i7-14700KF、PowerShell 7.6.3、.NET 10.0.9で、モジュール読込を除外し、ウォームアップ後の実行時間の中央値を比較しています。
値は実行環境やファイル構成によって変動します。

| 処理 | FastFs | 比較対象 | 速い方 |
| --- | ---: | ---: | --- |
| 2,000件の一覧を文字列化 | 28.04 ms | PowerShell：131.50 ms | FastFs（4.69倍） |
| 4,000ファイルを走査して2,000件を出力 | 31.52 ms | PowerShell：153.57 ms | FastFs（4.87倍） |
| 約5 MiB、20,000行の全体読取 | 113.98 ms | PowerShell：191.44 ms | FastFs（1.68倍） |
| 500ファイルの更新日時を変更 | 67.72 ms | PowerShell：125.38 ms | FastFs（1.85倍） |
| 約5 MiBの読取を`head -c 4000`で制限 | 0.82 ms | PowerShell：52.66 ms | FastFs（64.22倍） |
| Cargoレジストリを正規表現で検索 | 1,621.50 ms | ripgrep 15.1.0：2,420.67 ms | FastFs（1.49倍） |
| 同じ検索を`head -c 8000`で制限 | 530.19 ms | ripgrep 15.1.0：2,441.79 ms | FastFs（4.61倍） |

FastFsはPowerShell内で動く一方、ripgrepはコマンドごとに外部プロセスを起動するため、実際の利用と同じく`rg.exe`の起動時間を含めています。
