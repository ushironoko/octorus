# octorus

<p align="center">
  <img src="assets/banner.png" alt="octorus banner" width="600">
</p>

[![Crates.io](https://img.shields.io/crates/v/octorus.svg)](https://crates.io/crates/octorus)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

[English](./README.md)

GitHub PRやローカルdiffを、6,000ファイル・300,000行超の規模でもターミナル上で表示・レビューできます。2つのAIエージェントが自動でレビューと修正を繰り返し、approveまで導きます。

## Features

### Performance
- キャッシュによる高速起動・表示
- 高いメモリ効率と安全性
- 6,000ファイル以上・300,000行以上のdiffで動作

### AI Rally
AIエージェントによるPRの自動レビュー＆修正サイクル。reviewerエージェントがdiffを分析しコメントを投稿、revieweeエージェントが問題を修正してコミット——承認されるまでループします。

### Local Diff Mode
`git diff HEAD` のローカルdiffをfile watcherによりリアルタイムでプレビュー——PRは不要です。`L`キーでPRモードとLocalモードを即座に切り替えられます。

### PR Review
- ファイルリストとdiffプレビューのsplit view
- tree-sitterによるsyntax highlighting
- 特定の行へのinline commentとcode suggestionの追加
- review commentの表示・ナビゲーションとjump-to-line
- レビューの送信（Approve / Request Changes / Comment）
- Vimライクなsymbol search（`gd`）、その場でのファイル表示・編集（`gf`）
- PRコミットをdiffプレビュー付きで閲覧できるgit log view
- ワークフロー詳細付きのCIチェックステータス表示

### Customization
- すべてのkeybindingsとeditorを自由に設定可能
- AI Rallyのprompt templateをカスタマイズ可能
- 任意のsyntax highlightingテーマを追加可能

## 必要要件

- [GitHub CLI (gh)](https://cli.github.com/) - インストール・認証済みであること
- Rust 1.70+（ソースからビルドする場合）
- **AI Rally 機能を使用する場合**（オプション、いずれか）:
  - [Claude Code](https://claude.ai/code) - Anthropic の CLI ツール
  - [OpenAI Codex CLI](https://github.com/openai/codex) - OpenAI の CLI ツール

## インストール

```bash
cargo install octorus
```

または [mise](https://mise.jdx.dev/) 経由でインストール:

```bash
mise use -g github:ushironoko/octorus
```

または、ソースからビルド:

```bash
git clone https://github.com/ushironoko/octorus.git
cd octorus
cargo build --release
cp target/release/or ~/.local/bin/
```

## 使い方

```bash
# 1. 設定ファイルを初期化（AI Rally を使う場合は推奨）
or init

# 2. 現在のリポジトリの PR 一覧を開く（git remote から自動検出）
or

# 3. 特定の PR を開く
or --repo owner/repo --pr 123

# 4. AI Rally を開始（PR 選択後に自動開始）
or --ai-rally

# 5. ヘッドレスモードで AI Rally を実行（TUI なし、CI/CD 対応）
or --repo owner/repo --pr 123 --ai-rally

# 6. ローカル diff に対してヘッドレス AI Rally を実行
or --local --ai-rally

# 7. ローカルの変更をリアルタイムで確認
or --local
```

### オプション

| オプション | 説明 |
|--------|-------------|
| `-r, --repo <REPO>` | リポジトリ名（例: "owner/repo"） |
| `-p, --pr <PR>` | プルリクエスト番号 |
| `--ai-rally` | AI Rally モードを直接開始（`--pr` または `--local` と組み合わせるとヘッドレスモード） |
| `--working-dir <DIR>` | AI エージェントの作業ディレクトリ（デフォルト: カレントディレクトリ） |
| `--local` | GitHub 取得をせず、`HEAD` との差分を表示 |
| `--auto-focus` | ローカルモード時に差分更新があったファイルへ自動フォーカス |
| `--accept-local-overrides` | ヘッドレスモードでローカル `.octorus/` の AI 設定上書きを許可 |

### サブコマンド

| サブコマンド | 説明 |
|------------|-------------|
| `or init` | グローバル設定ファイル、プロンプトテンプレート、エージェント SKILL.md を初期化 |
| `or init --local` | プロジェクトローカルの `.octorus/` 設定とプロンプトを初期化 |
| `or init --force` | 既存の設定ファイルを上書き |
| `or clean` | AI Rally セッションデータを削除 |

`or init` はグローバル設定を作成:
- `~/.config/octorus/config.toml` - メイン設定ファイル
- `~/.config/octorus/prompts/` - プロンプトテンプレートディレクトリ
- `~/.claude/skills/octorus/SKILL.md` - エージェントスキルドキュメント（`~/.claude/` が存在する場合）

`or init --local` はプロジェクトローカル設定を作成:
- `.octorus/config.toml` - プロジェクトローカル設定（グローバルを上書き）
- `.octorus/prompts/` - プロジェクトローカルプロンプトテンプレート

## AI Rally

AI Rally は2つの AI エージェントによる自動 PR レビュー＆修正サイクルです:

- **Reviewer**: PR の diff を分析してレビューフィードバックを提供
- **Reviewee**: レビューフィードバックに基づいて問題を修正し、変更をコミット

### 動作フロー

```
┌─────────────────┐
│  Rally 開始     │  ファイル一覧画面で 'A' を押下
└────────┬────────┘
         ▼
┌─────────────────┐
│    Reviewer     │  AI が PR の diff をレビュー
│ (Claude/Codex)  │  → PR にレビューコメントを投稿
└────────┬────────┘
         │
    ┌────┴────┐
    │ Approve?│
    └────┬────┘
     No  │  Yes ──→ 完了 ✓
         ▼
┌─────────────────┐
│    Reviewee     │  AI が問題を修正
│ (Claude/Codex)  │  → ローカルにコミット（デフォルトで push なし）
└────────┬────────┘
         │
    ┌────┴──────────────┐
    │                   │
    ▼                   ▼
 完了          NeedsClarification /
    │           NeedsPermission
    │                   │
    │          ユーザーが応答 (y/n)
    │                   │
    └─────────┬─────────┘
              ▼
┌───────────────────────┐
│  再レビュー (Reviewer) │  更新された diff:
│                       │  git diff（ローカル優先）or
│                       │  gh pr diff（push 済みの場合）
└───────────┬───────────┘
            │
       ┌────┴────┐
       │ Approve?│  ... Approve または最大
       └─────────┘      イテレーションまで繰り返し
```

### ヘッドレスモード（CI/CD）

`--ai-rally` を `--pr` または `--local` と組み合わせると、AI Rally は**ヘッドレスモード**で実行されます — TUI は起動せず、すべての出力は stderr に出力され、CI/CD パイプラインに適した終了コードでプロセスが終了します。

```bash
# 特定の PR に対してヘッドレスラリーを実行
or --repo owner/repo --pr 123 --ai-rally

# ローカル diff に対してヘッドレスラリーを実行
or --local --ai-rally

# カスタム作業ディレクトリを指定
or --repo owner/repo --pr 123 --ai-rally --working-dir /path/to/repo
```

**JSON 出力**（stdout）:

ヘッドレスモードは完了時に JSON オブジェクトを stdout に出力します:

```json
{
  "result": "Approved",
  "iterations": 2,
  "summary": "All issues resolved",
  "last_review": { ... },
  "last_fix": { ... }
}
```

| フィールド | 型 | 説明 |
|-----------|------|-------------|
| `result` | `"Approved"` / `"NotApproved"` / `"Error"` | 最終結果 |
| `iterations` | number | 実行されたレビュー＆修正イテレーション数 |
| `summary` | string | 結果の要約 |
| `last_review` | object \| null | 最後のレビュワー出力（ある場合） |
| `last_fix` | object \| null | 最後のレビュイー出力（ある場合） |

**終了コード:**

| コード | 意味 |
|--------|------|
| `0` | Reviewer が Approve |
| `1` | 未承認（Request Changes、エラー、中断） |

**ヘッドレスポリシー**（人間の操作なし）:

| 状況 | 動作 |
|------|------|
| 確認が必要（Clarification） | 自動スキップ（エージェントが最善の判断で続行） |
| 許可が必要（Permission） | 自動拒否（動的なツール拡張を防止） |
| 投稿確認（Post Confirmation） | 自動承認（レビュー/修正を PR に投稿） |
| エージェントのテキスト/思考 | 非表示（stdout への JSON 漏洩を防止） |

**CI/CD 例（GitHub Actions）:**

```yaml
- name: AI Rally Review
  run: |
    or --repo ${{ github.repository }} --pr ${{ github.event.pull_request.number }} --ai-rally
```

### 特徴

- **PR 統合**: レビューコメントは自動的に PR に投稿
- **外部 Bot サポート**: Copilot、CodeRabbit 等の Bot からのフィードバックを収集
- **安全な操作**: 危険な git 操作（`--force`、`reset --hard`）は禁止
- **セッション永続化**: Rally の状態はローカルに保存され、再開可能
- **インタラクティブフロー**: AI エージェントが確認や許可を求める際、対話的に応答可能
- **ローカル Diff サポート**: 再レビュー時はローカルの `git diff` を優先して未プッシュの変更を検出。push 済みの場合は `gh pr diff` にフォールバック
- **一時停止/再開**: `p` を押すと次のチェックポイント（イテレーション間）で Rally を一時停止。再度 `p` を押すと再開。ヘッダーに待機中は `(Pausing...)`、停止中は `(PAUSED)` と表示
- **バックグラウンド実行**: `b` を押すと Rally をバックグラウンドで実行しながらファイル閲覧を継続可能
- **自動投稿**: `[ai]` 設定で `auto_post = true` にすると、確認プロンプトをスキップしてレビュー/修正コメントを PR に自動投稿

### 推奨構成

Codex はサンドボックスモードを使用し、細粒度のツール権限制御ができません。
最大限のセキュリティのため、以下の構成を推奨:

| 役割 | 推奨 | 理由 |
|------|-------------|--------|
| Reviewer | Codex または Claude | 読み取り専用操作のため、どちらも安全 |
| Reviewee | **Claude** | allowedTools による細粒度のツール制御が可能 |

安全な構成の例:

```toml
[ai]
reviewer = "codex"   # 安全: 読み取り専用サンドボックス
reviewee = "claude"  # 推奨: 細粒度のツール制御
reviewee_additional_tools = ["Skill"]  # 必要なものだけ追加
```

**注意**: Codex を reviewee として使用する場合、`--full-auto` モードで実行され、
ワークスペースへの書き込みアクセスとツール制限なしで動作します。

### ツール権限

#### デフォルトで許可されるツール

**Reviewer**（読み取り専用操作）:

| ツール | 説明 |
|------|-------------|
| Read, Glob, Grep | ファイル読み取りと検索 |
| `gh pr view/diff/checks` | PR 情報の表示 |
| `gh api --method GET` | GitHub API（GET のみ） |

**Reviewee**（コード修正）:

| カテゴリ | コマンド |
|----------|----------|
| ファイル | Read, Edit, Write, Glob, Grep |
| Git | status, diff, add, commit, log, show, branch, switch, stash |
| GitHub CLI | pr view, pr diff, pr checks, api GET |
| Cargo | build, test, check, clippy, fmt, run |
| npm/pnpm/bun | install, test, run |

#### 追加ツール（Claude only）

追加ツールは設定で有効化できます。Claude Code の `--allowedTools` 形式を使用:

| 例 | 説明 |
|---------|-------------|
| `"Skill"` | Claude Code スキルの実行 |
| `"WebFetch"` | URL コンテンツの取得 |
| `"WebSearch"` | Web 検索 |
| `"Bash(git push:*)"` | リモートへの git push |
| `"Bash(gh api --method POST:*)"` | GitHub API POST リクエスト |

```toml
[ai]
reviewee_additional_tools = ["Skill", "Bash(git push:*)"]
```

**Breaking Change (v0.2.0)**: `git push` はデフォルトで無効になりました。
有効にするには `"Bash(git push:*)"` を `reviewee_additional_tools` に追加してください。

## Local Diff Mode

Local Diff Mode は、プルリクエストなしでローカルの未コミット変更（`git diff HEAD`）を TUI 上で直接プレビューする機能です。ファイルウォッチャーがリアルタイムで変更を検知し、diff を自動更新します。

### 起動方法

```bash
# Local Diff Mode で起動
or --local

# Auto-focus 付き: 更新のたびに変更ファイルへ自動ジャンプ
or --local --auto-focus
```

### リアルタイムファイル監視

Local Mode では、作業ディレクトリのファイル変更を監視します（`.git/` 内部やアクセスのみのイベントは無視）。ファイルを保存すると、diff 画面が自動的に更新されます。

### Auto-focus

`--auto-focus` を有効にする（または `F` キーでトグルする）と、最も直近に変更されたファイルを自動的に選択・フォーカスします。ファイル一覧にいる場合は、自動的に Split View の diff 画面に遷移します。選択アルゴリズムは、現在のカーソル位置から最も近い変更ファイルを選択します。

ヘッダーには Local Mode 時は `[LOCAL]`、Auto-focus 有効時は `[LOCAL AF]` と表示されます。

### PR モードとの切替

`L` キーでいつでも PR モードと Local モードを切り替えられます:

```
PR mode ──[L]──► Local mode
  │                │
  │  UI 状態を      │  ファイルウォッチャー起動
  │  保存/復元      │  git diff HEAD を表示
  │                │
Local mode ──[L]──► PR mode
```

モード切替時に UI 状態（選択ファイル、スクロール位置）は保持されます。PR から切り替えた場合、Local Mode で `L` を押すとキャッシュされた PR データと共にその PR に復帰します。

### PR モードとの違い

Local Mode では PR が存在しないため、以下の機能は**無効**になります:

| 機能 | 利用可否 |
|------|---------|
| 変更ファイル一覧の閲覧 | ✅ |
| シンタックスハイライト付き diff | ✅ |
| Split View | ✅ |
| 定義へジャンプ (`gd`) | ✅ |
| エディタでファイルを開く (`gf`) | ✅ |
| インラインコメントの追加 | ❌ |
| サジェスチョンの追加 | ❌ |
| レビュー送信 | ❌ |
| コメント一覧の表示 | ❌ |
| CI チェックの表示 (`S`) | ❌ |
| PR をブラウザで開く (`O`) | ❌ |

## 設定

### グローバル設定

`or init` を実行してデフォルト設定ファイルを作成するか、手動で `~/.config/octorus/config.toml` を作成:

```toml
# レビュー本文入力に使用するエディタ
# 解決順序: この設定値 → $VISUAL → $EDITOR → vi
# 引数付きも可: editor = "code --wait"
# editor = "vim"

[diff]
# diff 画面のシンタックスハイライトテーマ
# 利用可能なテーマについては下記「テーマ」セクションを参照
theme = "base16-ocean.dark"
# diff 画面でのタブ文字のスペース数（最小値: 1）
tab_width = 4
# 追加/削除行の背景色を表示（デフォルト: true）
# bg_color = false

[keybindings]
# 設定可能なすべてのキーについては「設定可能なキーバインド」セクションを参照
approve = "a"
request_changes = "r"
comment = "c"
suggestion = "s"

[git_log]
# コミット diff のキャッシュ最大数（デフォルト: 20）
max_diff_cache = 20

[ai]
# レビュワー/レビュイーに使用する AI エージェント
# サポート: "claude" (Claude Code), "codex" (OpenAI Codex CLI)
reviewer = "claude"
reviewee = "claude"

# 最大イテレーション回数
max_iterations = 10

# エージェント実行のタイムアウト（秒）
timeout_secs = 600

# カスタムプロンプトディレクトリ（デフォルト: ~/.config/octorus/prompts/）
# prompt_dir = "/custom/path/to/prompts"

# reviewer 用の追加ツール（Claude only）
# Claude Code の --allowedTools 形式で指定
# reviewer_additional_tools = []

# reviewee 用の追加ツール（Claude only）
# 例: "Skill", "WebFetch", "WebSearch", "Bash(git push:*)"
# reviewee_additional_tools = ["Skill", "Bash(git push:*)"]

# レビュー/修正コメントを確認なしで PR に自動投稿
# デフォルトは false（投稿前に確認プロンプトを表示）
# auto_post = true
```

### プロジェクトローカル設定

リポジトリルートの `.octorus/` ディレクトリにプロジェクト固有の設定を作成できます。バージョン管理に含めることでチーム内で設定を共有できます。

```bash
or init --local
```

以下のファイルが生成されます:

```
.octorus/
├── config.toml        # プロジェクトローカル設定（グローバルを上書き）
└── prompts/
    ├── reviewer.md    # プロジェクト固有のレビュワープロンプト
    ├── reviewee.md    # プロジェクト固有のレビュイープロンプト
    └── rereview.md    # プロジェクト固有の再レビュープロンプト
```

**上書き動作**: ローカル設定はグローバル設定に対してディープマージされます。上書きしたいキーのみ指定すれば、未指定のキーはグローバル設定から継承されます。

```toml
# .octorus/config.toml — 上書きしたいものだけ指定
[ai]
max_iterations = 5
timeout_secs = 300
```

**プロンプト解決順序**（優先度の高い順）:
1. `.octorus/prompts/`（プロジェクトローカル）
2. `ai.prompt_dir`（設定ファイルで指定されたカスタムディレクトリ）
3. `~/.config/octorus/prompts/`（グローバル）
4. 組み込みデフォルト

> **注意**: `.octorus/` を含むリポジトリをクローンまたはフォークする際は、その設定がリポジトリのオーナーによって作成されたものであることに留意してください。octorus はユーザーを保護するため、以下の制限を設けています:
>
> - **`editor` はローカル設定では常に無視されます。** プロジェクト単位での設定はできません。
> - **AI 関連の設定**（`ai.reviewer`, `ai.reviewee`, `ai.*_additional_tools`, `ai.auto_post`）や**ローカルプロンプトファイル**は、AI Rally 開始前に確認ダイアログが表示されます。ヘッドレスモードでは `--accept-local-overrides` フラグを明示的に指定する必要があります。
> - **`ai.prompt_dir`** はローカル設定では絶対パスや `..` を使用できません。
> - `.octorus/prompts/` 内のシンボリックリンクは追跡されません。

### プロンプトテンプレートのカスタマイズ

AI Rally はカスタマイズ可能なプロンプトテンプレートを使用します。`or init` を実行してデフォルトテンプレートを生成し、必要に応じて編集してください:

```
~/.config/octorus/prompts/
├── reviewer.md    # レビュワーエージェント用プロンプト
├── reviewee.md    # レビュイーエージェント用プロンプト
└── rereview.md    # 再レビュー用プロンプト
```

テンプレートは `{{variable}}` 構文で変数置換をサポートしています:

| 変数 | 説明 | 使用可能なテンプレート |
|----------|-------------|--------------|
| `{{repo}}` | リポジトリ名（例: "owner/repo"） | すべて |
| `{{pr_number}}` | プルリクエスト番号 | すべて |
| `{{pr_title}}` | プルリクエストタイトル | すべて |
| `{{pr_body}}` | プルリクエスト本文 | reviewer |
| `{{diff}}` | PR の diff 内容 | reviewer |
| `{{iteration}}` | 現在のイテレーション番号 | すべて |
| `{{review_summary}}` | レビュワーからのサマリー | reviewee |
| `{{review_action}}` | レビューアクション（Approve/RequestChanges/Comment） | reviewee |
| `{{review_comments}}` | レビューコメント一覧 | reviewee |
| `{{blocking_issues}}` | ブロッキングイシュー一覧 | reviewee |
| `{{external_comments}}` | 外部ツールからのコメント | reviewee |
| `{{changes_summary}}` | 変更内容のサマリー | rereview |
| `{{updated_diff}}` | 修正後の diff | rereview |

### テーマ

`[diff]` セクションの `theme` オプションで、diff 画面のシンタックスハイライトの配色を設定できます。

#### 組み込みテーマ

| テーマ | 説明 |
|-------|-------------|
| `base16-ocean.dark` | Base16 Ocean ベースのダークテーマ（デフォルト） |
| `base16-ocean.light` | Base16 Ocean ベースのライトテーマ |
| `base16-eighties.dark` | Base16 Eighties ベースのダークテーマ |
| `base16-mocha.dark` | Base16 Mocha ベースのダークテーマ |
| `Dracula` | Dracula カラースキーム |
| `InspiredGitHub` | GitHub 風のライトテーマ |
| `Solarized (dark)` | Solarized ダーク |
| `Solarized (light)` | Solarized ライト |

```toml
[diff]
theme = "Dracula"
```

テーマ名は**大文字・小文字を区別しません**（`dracula`、`Dracula`、`DRACULA` のいずれでも動作します）。

指定したテーマが見つからない場合は `base16-ocean.dark` にフォールバックします。

#### カスタムテーマ

`~/.config/octorus/themes/` に `.tmTheme`（TextMate テーマ）ファイルを配置することで、カスタムテーマを追加できます:

```
~/.config/octorus/themes/
├── MyCustomTheme.tmTheme
└── nord.tmTheme
```

ファイル名（`.tmTheme` 拡張子を除いた部分）がテーマ名になります:

```toml
[diff]
theme = "MyCustomTheme"
```

組み込みテーマと同名のカスタムテーマは、組み込みテーマを上書きします。

## キーバインド

### PR 一覧画面

| キー | 操作 |
|-----|--------|
| `j` / `↓` | 下に移動 |
| `k` / `↑` | 上に移動 |
| `Shift+j` | ページダウン |
| `Shift+k` | ページアップ |
| `gg` | 先頭にジャンプ |
| `G` | 末尾にジャンプ |
| `Enter` | PR を選択 |
| `o` | フィルタ: Open PR のみ |
| `c` | フィルタ: Closed PR のみ |
| `a` | フィルタ: すべての PR |
| `O` | PR をブラウザで開く |
| `S` | CI チェックステータスを表示 |
| `Space /` | キーワードフィルタ |
| `R` | PR 一覧をリフレッシュ |
| `L` | Local Diff Mode の切替 |
| `?` | ヘルプを表示 |
| `q` | 終了 |

PR は無限スクロールで読み込まれ、下にスクロールすると追加の PR が自動的に取得されます。ヘッダーに現在の状態フィルタ（open/closed/all）が表示されます。

### ファイル一覧画面

| キー | 操作 |
|-----|--------|
| `j` / `↓` | 下に移動 |
| `k` / `↑` | 上に移動 |
| `Shift+j` | ページダウン |
| `Shift+k` | ページアップ |
| `Enter` / `→` / `l` | Split View を開く |
| `v` | ファイルを viewed/unviewed にマーク |
| `V` | ディレクトリを viewed にマーク |
| `a` | PR を Approve |
| `r` | Request changes |
| `c` | Comment only |
| `C` | レビューコメント一覧を表示 |
| `R` | 強制リフレッシュ（キャッシュ破棄） |
| `d` | PR の説明文を表示 |
| `A` | AI Rally を開始 |
| `S` | CI チェックステータスを表示 |
| `gl` | Git log 画面を開く |
| `I` | Issue 一覧を開く |
| `Space /` | キーワードフィルタ |
| `L` | Local Diff Mode の切替 |
| `F` | Auto-focus の切替（Local Mode 時） |
| `?` | ヘルプを表示/非表示 |
| `q` | 終了 |

### Split View

Split View はファイル一覧（左 35%）と diff プレビュー（右 65%）を横並びで表示します。フォーカスされているペインは黄色のボーダーでハイライトされます。

**ファイル一覧フォーカス時:**

| キー | 操作 |
|-----|--------|
| `j` / `↓` | ファイル選択を移動（diff が追従） |
| `k` / `↑` | ファイル選択を移動（diff が追従） |
| `Enter` / `→` / `l` | diff ペインにフォーカス |
| `←` / `h` / `q` | ファイル一覧に戻る |

**diff フォーカス時:**

| キー | 操作 |
|-----|--------|
| `j` / `↓` | diff をスクロール |
| `k` / `↑` | diff をスクロール |
| `gd` | 定義へジャンプ |
| `gf` | $EDITOR でファイルを開く |
| `gg` / `G` | 先頭/末尾にジャンプ |
| `Ctrl-o` | 前の位置に戻る |
| `Ctrl-d` | ページダウン |
| `Ctrl-u` | ページアップ |
| `n` | 次のコメントにジャンプ |
| `N` | 前のコメントにジャンプ |
| `c` | 行にコメントを追加 |
| `s` | 行にサジェスチョンを追加 |
| `Shift+Enter` | マルチライン選択モードに入る |
| `Enter` | コメントパネルを開く |
| `Tab` / `→` / `l` | フルスクリーン diff 画面を開く |
| `←` / `h` | ファイル一覧にフォーカス |
| `q` | ファイル一覧に戻る |

### Diff 画面

| キー | 操作 |
|-----|--------|
| `j` / `↓` | 下に移動 |
| `k` / `↑` | 上に移動 |
| `gd` | 定義へジャンプ |
| `gf` | $EDITOR でファイルを開く |
| `gg` / `G` | 先頭/末尾にジャンプ |
| `Ctrl-o` | 前の位置に戻る |
| `n` | 次のコメントにジャンプ |
| `N` | 前のコメントにジャンプ |
| `Ctrl-d` | ページダウン |
| `Ctrl-u` | ページアップ |
| `c` | 行にコメントを追加 |
| `s` | 行にサジェスチョンを追加 |
| `Shift+Enter` / `V` | マルチライン選択モードに入る |
| `M` | Markdown リッチ表示の切替 |
| `Enter` | コメントパネルを開く |
| `←` / `h` / `q` / `Esc` | 前の画面に戻る |

**定義へジャンプ (`gd`)**: 複数のシンボル候補が見つかった場合、選択ポップアップが表示されます。`j`/`k` で移動、`Enter` でジャンプ、`Esc` でキャンセル。ジャンプスタック（`Ctrl-o` で戻る）は最大100件まで保持されます。

**Note**: 既存のコメントがある行は `●` マーカーで表示されます。コメントのある行を選択すると、diff の下にコメント内容が表示されます。

**マルチライン選択モード:**

`Shift+Enter` でマルチライン選択モードに入ります。行範囲を選択して、選択範囲全体にコメントやサジェスチョンを作成できます。

| キー | 操作 |
|-----|--------|
| `j` / `↓` | 選択範囲を下に拡張 |
| `k` / `↑` | 選択範囲を上に拡張 |
| `Enter` / `c` | 選択範囲にコメント |
| `s` | 選択範囲にサジェスチョン |
| `Esc` | 選択をキャンセル |

**コメントパネル（フォーカス時）:**

| キー | 操作 |
|-----|--------|
| `j` / `k` | パネルをスクロール |
| `c` | コメントを追加 |
| `s` | サジェスチョンを追加 |
| `r` | コメントに返信 |
| `Tab` / `Shift-Tab` | 返信対象を選択 |
| `n` / `N` | 次/前のコメントにジャンプ |
| `Esc` / `q` | パネルを閉じる |

### 入力モード（コメント/サジェスチョン/リプライ）

コメント、サジェスチョン、リプライを追加する際は、組み込みテキスト入力モードに入ります:

| キー | 操作 |
|-----|--------|
| `Ctrl+S` | 送信 |
| `Esc` | キャンセル |

複数行の入力が可能です。`Enter` で改行を挿入できます。

### Git Log 画面

PR のコミット履歴をシンタックスハイライト付き diff プレビューで閲覧できます。

**コミット一覧フォーカス時（Split View）:**

| キー | 操作 |
|-----|--------|
| `j` / `↓` | コミット一覧を下に移動 |
| `k` / `↑` | コミット一覧を上に移動 |
| `Shift+j` | ページダウン |
| `Shift+k` | ページアップ |
| `g` | 先頭のコミットにジャンプ |
| `G` | 末尾のコミットにジャンプ |
| `Enter` / `Tab` / `→` / `l` | diff ペインにフォーカス |
| `r` | リトライ（エラー時） |
| `q` / `Esc` / `←` / `h` | ファイル一覧に戻る |

**diff フォーカス時（Split View）:**

| キー | 操作 |
|-----|--------|
| `j` / `↓` | diff をスクロール |
| `k` / `↑` | diff をスクロール |
| `gg` / `G` | 先頭/末尾にジャンプ |
| `Ctrl-d` | ページダウン |
| `Ctrl-u` | ページアップ |
| `Tab` / `→` / `l` | フルスクリーン diff 画面を開く |
| `←` / `h` | コミット一覧にフォーカス |
| `q` | ファイル一覧に戻る |

コミットは無限スクロールで読み込まれ、下にスクロールすると追加のコミットが自動的に取得されます。diff はバックグラウンドでプリフェッチされ、高速なナビゲーションを実現します。

### CI チェック画面

| キー | 操作 |
|-----|--------|
| `j` / `↓` | 下に移動 |
| `k` / `↑` | 上に移動 |
| `Enter` | チェックをブラウザで開く |
| `R` | チェック一覧をリフレッシュ |
| `O` | PR をブラウザで開く |
| `?` | ヘルプを表示 |
| `q` / `Esc` | 前の画面に戻る |

ステータスアイコン: `✓`（成功）、`✕`（失敗）、`○`（実行中）、`-`（スキップ/キャンセル）。各チェックの名前、ワークフロー、所要時間が表示されます。

### コメント一覧画面

| キー | 操作 |
|-----|--------|
| `j` / `↓` | 下に移動 |
| `k` / `↑` | 上に移動 |
| `Enter` | ファイル/行にジャンプ |
| `q` / `Esc` | ファイル一覧に戻る |

### AI Rally 画面

| キー | 操作 |
|-----|--------|
| `j` / `↓` | ログ内を下に移動 |
| `k` / `↑` | ログ内を上に移動 |
| `Enter` | ログ詳細を表示 |
| `g` | 先頭にジャンプ |
| `G` | 末尾にジャンプ |
| `b` | バックグラウンド実行（ファイル一覧に戻る） |
| `y` | 許可を付与 / 回答を入力 |
| `n` | 許可を拒否 / スキップ |
| `p` | Rally の一時停止 / 再開 |
| `r` | リトライ（エラー時） |
| `q` / `Esc` | Rally を中止して終了 |

### 設定可能なキーバインド

すべてのキーバインドは `[keybindings]` セクションでカスタマイズできます。3つのフォーマットをサポート:

```toml
[keybindings]
# 単一キー
move_down = "j"

# 修飾子付きキー
page_down = { key = "d", ctrl = true }

# 2キーシーケンス
go_to_definition = ["g", "d"]
```

#### 設定可能なキー一覧

| キー | デフォルト | 説明 |
|-----|---------|-------------|
| **ナビゲーション** |||
| `move_down` | `j` | 下に移動 |
| `move_up` | `k` | 上に移動 |
| `move_left` | `h` | 左に移動 / 戻る |
| `move_right` | `l` | 右に移動 / 選択 |
| `page_down` | `Ctrl+d` | ページダウン |
| `page_up` | `Ctrl+u` | ページアップ |
| `jump_to_first` | `gg` | 先頭にジャンプ |
| `jump_to_last` | `G` | 末尾にジャンプ |
| `jump_back` | `Ctrl+o` | 前の位置に戻る |
| `next_comment` | `n` | 次のコメントにジャンプ |
| `prev_comment` | `N` | 前のコメントにジャンプ |
| **アクション** |||
| `approve` | `a` | PR を Approve |
| `request_changes` | `r` | Request changes |
| `comment` | `c` | コメント追加 |
| `suggestion` | `s` | サジェスチョン追加 |
| `reply` | `r` | コメントに返信 |
| `refresh` | `R` | 強制リフレッシュ |
| `submit` | `Ctrl+s` | 入力を送信 |
| **モード切替** |||
| `quit` | `q` | 終了 / 戻る |
| `help` | `?` | ヘルプを表示 |
| `comment_list` | `C` | コメント一覧を開く |
| `ai_rally` | `A` | AI Rally を開始 |
| `open_panel` | `Enter` | パネルを開く / 選択 |
| `open_in_browser` | `O` | PR をブラウザで開く |
| `ci_checks` | `S` | CI チェックステータスを表示 |
| `git_log` | `gl` | Git log 画面を開く |
| `issue_list` | `I` | Issue 一覧を開く |
| `toggle_local_mode` | `L` | Local Diff Mode の切替 |
| `toggle_auto_focus` | `F` | Auto-focus の切替（Local Mode 時） |
| `toggle_markdown_rich` | `M` | Markdown リッチ表示の切替 |
| `pr_description` | `d` | PR の説明文を表示 |
| **Diff 操作** |||
| `go_to_definition` | `gd` | 定義へジャンプ |
| `go_to_file` | `gf` | $EDITOR でファイルを開く |
| `multiline_select` | `V` | マルチライン選択モードに入る |
| **リスト操作** |||
| `filter` | `Space /` | キーワードフィルタ（PR 一覧 / ファイル一覧） |

### キーワードフィルタ

PR 一覧やファイル一覧で `Space /` を押すと、キーワードフィルタが起動します。入力した文字列で項目を絞り込めます。

| キー | 操作 |
|-----|--------|
| 文字入力 | キーワードで絞り込み |
| `Backspace` | 文字を削除 |
| `Ctrl+u` | フィルタテキストをクリア |
| `↑` / `↓` | フィルタ結果内を移動 |
| `Enter` | 選択を確定 |
| `Esc` | フィルタを解除 |

**Note**: 矢印キー（`↑/↓/←/→`）は常に Vim スタイルキーの代替として動作し、リマップできません。

## ライセンス

MIT
