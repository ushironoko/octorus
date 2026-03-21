use lasso::{Rodeo, Spur};
use ratatui::style::Style;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::ai::orchestrator::RallyEvent;
use crate::ai::RallyState;
use crate::diff::LineType;
use crate::loader::SingleFileDiffResult;
use crate::diff_store::{DiffCacheStore, DiffScrollState, ScrollMode, MAX_STORE_ENTRIES};
use crate::github::{
    ChangedFile, IssueComment, IssueDetail, IssueListPage, IssueStateFilter, IssueSummary,
    LinkedPr, PrCommit, PullRequest,
};

/// コメントのdiff内位置を表す構造体
#[derive(Debug, Clone)]
pub struct CommentPosition {
    pub diff_line_index: usize,
    pub comment_index: usize,
}

/// ジャンプ履歴の1エントリ（Go to Definition / Jump Back 用）
#[derive(Debug, Clone)]
pub struct JumpLocation {
    pub file_index: usize,
    pub line_index: usize,
    pub scroll_offset: usize,
}

/// シンボル選択ポップアップの状態
#[derive(Debug, Clone)]
pub struct SymbolPopupState {
    /// 候補シンボル一覧 (name, start, end)
    pub symbols: Vec<(String, usize, usize)>,
    /// 選択中のインデックス
    pub selected: usize,
}

/// インターン済みの Span（アロケーション削減）
///
/// 文字列をインターナーに格納し、4バイトの Spur で参照することで
/// 重複トークンのアロケーションを削減する。
#[derive(Clone)]
pub struct InternedSpan {
    /// インターン済み文字列への参照（4 bytes）
    pub content: Spur,
    /// スタイル情報（8 bytes）
    pub style: Style,
}

/// Diff行のキャッシュ（シンタックスハイライト済み）
#[derive(Clone)]
pub struct CachedDiffLine {
    /// 基本の Span（REVERSED なし）
    pub spans: Vec<InternedSpan>,
    /// 行の種類（背景色の決定に使用）
    pub line_type: LineType,
}

/// Diff表示のキャッシュ
pub struct DiffCache {
    /// キャッシュ対象のファイルインデックス
    pub file_index: usize,
    /// patch のハッシュ（変更検出用）
    pub patch_hash: u64,
    /// パース済みの行データ
    pub lines: Vec<CachedDiffLine>,
    /// 文字列インターナー（キャッシュ内で共有）
    pub interner: Rodeo,
    /// シンタックスハイライト済みかどうか（プレーンキャッシュは false）
    pub highlighted: bool,
    /// Markdown リッチ表示モードで構築されたかどうか
    pub markdown_rich: bool,
}

impl DiffCache {
    /// Spur を文字列参照に解決する
    ///
    /// ライフタイムは DiffCache に依存するため、ゼロコピーでレンダリング可能。
    pub fn resolve(&self, spur: Spur) -> &str {
        self.interner.resolve(&spur)
    }
}

/// 文字列のハッシュを計算
pub fn hash_string(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// 複数行選択の状態
#[derive(Debug, Clone)]
pub struct MultilineSelection {
    /// 選択開始行（diff内のインデックス）。Shift+Enter押下時の行。
    pub anchor_line: usize,
    /// 選択終了行（diff内のインデックス）。カーソル移動で更新。
    pub cursor_line: usize,
}

impl MultilineSelection {
    /// 選択範囲の先頭行（小さい方）
    pub fn start(&self) -> usize {
        self.anchor_line.min(self.cursor_line)
    }

    /// 選択範囲の末尾行（大きい方）
    pub fn end(&self) -> usize {
        self.anchor_line.max(self.cursor_line)
    }
}

/// 行ベース入力のコンテキスト（コメント/サジェスチョン共通）
#[derive(Debug, Clone)]
pub struct LineInputContext {
    pub file_index: usize,
    pub line_number: u32,
    /// patch 内の position（1始まり）。GitHub API の `position` パラメータに対応。
    pub diff_position: u32,
    /// 複数行選択時の開始行番号（new file の行番号）
    pub start_line_number: Option<u32>,
}

/// 統一入力モード
#[derive(Debug, Clone)]
pub enum InputMode {
    Comment(LineInputContext),
    Suggestion {
        context: LineInputContext,
        original_code: String,
    },
    Reply {
        comment_id: u64,
        reply_to_user: String,
        reply_to_body: String,
    },
    IssueComment {
        issue_number: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppState {
    PullRequestList,
    FileList,
    DiffView,
    TextInput,
    CommentList,
    Help,
    AiRally,
    SplitViewFileList,
    SplitViewDiff,
    PrDescription,
    ChecksList,
    GitLogSplitCommitList,
    GitLogSplitDiff,
    GitLogDiffView,
    IssueList,
    IssueDetail,
    IssueCommentList,
    GitOpsSplitTree,
    GitOpsSplitDiff,
}

impl AppState {
    /// PR データ（DataState）に依存しない画面かどうか
    pub fn is_data_state_independent(self) -> bool {
        matches!(
            self,
            Self::PullRequestList
                | Self::Help
                | Self::PrDescription
                | Self::ChecksList
                | Self::IssueList
                | Self::IssueDetail
                | Self::IssueCommentList
                | Self::TextInput
                | Self::GitOpsSplitTree
                | Self::GitOpsSplitDiff
        )
    }

    /// Issue 系の画面かどうか
    pub fn is_issue(self) -> bool {
        matches!(
            self,
            Self::IssueList | Self::IssueDetail | Self::IssueCommentList
        )
    }
}

/// Variant for diff view handling (fullscreen vs split pane)
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum DiffViewVariant {
    /// Fullscreen diff view
    Fullscreen,
    /// Split pane diff view (right pane)
    SplitPane,
}

/// Log event type for AI Rally
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogEventType {
    Info,
    Thinking,
    ToolUse,
    ToolResult,
    Text,
    Review,
    Fix,
    Error,
}

/// Structured log entry for AI Rally
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub event_type: LogEventType,
    pub message: String,
}

impl LogEntry {
    pub fn new(event_type: LogEventType, message: String) -> Self {
        let now = chrono::Local::now();
        Self {
            timestamp: now.format("%H:%M:%S").to_string(),
            event_type,
            message,
        }
    }
}

/// Permission request information for AI Rally
#[derive(Debug, Clone)]
pub struct PermissionInfo {
    pub action: String,
    pub reason: String,
}

/// Pause state for AI Rally (TUI-side tracking)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseState {
    /// Rally is running normally
    Running,
    /// Pause requested, waiting for checkpoint
    PauseRequested,
    /// Actually paused at checkpoint
    Paused,
}

/// State for AI Rally view
#[derive(Debug, Clone)]
pub struct AiRallyState {
    pub iteration: u32,
    pub max_iterations: u32,
    pub state: RallyState,
    pub history: Vec<RallyEvent>,
    pub logs: Vec<LogEntry>,
    pub log_scroll_offset: usize,
    /// Selected log index for detail view
    pub selected_log_index: Option<usize>,
    /// Whether the log detail modal is visible
    pub showing_log_detail: bool,
    /// Pending clarification question from reviewee
    pub pending_question: Option<String>,
    /// Pending permission request from reviewee
    pub pending_permission: Option<PermissionInfo>,
    /// Pending review post confirmation
    pub pending_review_post: Option<crate::ai::orchestrator::ReviewPostInfo>,
    /// Pending fix post confirmation
    pub pending_fix_post: Option<crate::ai::orchestrator::FixPostInfo>,
    /// Last rendered visible log height (updated by UI render)
    pub last_visible_log_height: usize,
    /// Pending local config security warning (key, value) pairs.
    /// When Some, the orchestrator has NOT been started yet — the user must
    /// approve ('y') or reject ('n'/'q') the overrides before proceeding.
    pub pending_config_warning: Option<Vec<(String, String)>>,
    /// Current pause state
    pub pause_state: PauseState,
}

impl AiRallyState {
    /// Push a new log entry, auto-following to the bottom if the selection is at the tail.
    /// This keeps auto-scroll active when the user is watching the latest logs.
    pub fn push_log(&mut self, entry: LogEntry) {
        let was_at_tail = self.is_selection_at_tail();
        self.logs.push(entry);

        if was_at_tail {
            // Keep selection at the new tail and maintain auto-scroll
            self.selected_log_index = Some(self.logs.len().saturating_sub(1));
            self.log_scroll_offset = 0; // 0 means auto-scroll to bottom
        }
    }

    /// Check if the current selection is at the tail (last log) or unset
    fn is_selection_at_tail(&self) -> bool {
        match self.selected_log_index {
            None => true, // No selection = follow tail
            Some(idx) => {
                // At tail if selected index is the last log (or beyond)
                idx >= self.logs.len().saturating_sub(1)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReviewAction {
    Approve,
    RequestChanges,
    Comment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingApproveChoice {
    Ignore,
    Submit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum HelpTab {
    #[default]
    Keybindings,
    Config,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CommentTab {
    #[default]
    Review,
    Discussion,
}

/// リトライリクエストの種類（統一リトライループで使用）
#[derive(Debug, Clone)]
pub enum RefreshRequest {
    PrRefresh { pr_number: u32 },
    LocalRefresh,
}

/// PRファイルの viewed 変更結果
#[derive(Debug, Clone)]
pub(super) enum MarkViewedResult {
    Completed {
        marked_paths: Vec<String>,
        total_targets: usize,
        error: Option<String>,
        set_viewed: bool,
    },
}

/// ファイルウォッチャーのハンドル
///
/// `active` フラグで callback の処理を制御する。
/// スレッド自体は `_thread` で保持され、プロセス終了まで生存する。
pub struct WatcherHandle {
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) _thread: std::thread::JoinHandle<()>,
}

/// PRデータの読み込み状態。
///
/// `Loaded` のフィールドは `Arc` ではなく `Box`/`Vec` で保持する。
/// `SessionCache` との間のデータ分配は `clone()` で行う（PR更新時のみ発生）。
/// シングルスレッドで完結する設計のため、`Arc` による共有所有権は不要。
#[derive(Debug, Clone)]
pub enum DataState {
    Loading,
    Loaded {
        pr: Box<PullRequest>,
        files: Vec<ChangedFile>,
    },
    Error(String),
}

/// Git Log 画面の全状態（receiver も含めて集約）
///
/// `App.git_log_state: Option<GitLogState>` として保持。
/// 画面クローズ時に `None` で全破棄。
pub struct GitLogState {
    pub commits: Vec<PrCommit>,
    pub selected_commit: usize,
    pub commit_list_scroll_offset: usize,
    /// 現在選択中コミットの raw unified diff
    pub commit_diff: Option<String>,
    /// Diff キャッシュストア（SHA キー）
    pub diff_store: DiffCacheStore<String>,
    /// Diff スクロール状態
    pub diff_scroll: DiffScrollState,
    pub diff_loading: bool,
    pub commits_loading: bool,
    /// 追加コミットが存在するか（無限スクロール用）
    pub commits_has_more: bool,
    /// 現在のページ番号（GitHub API: 1-indexed, ローカル: offset計算用）
    pub commits_page: u32,
    /// コミット一覧取得エラー
    pub commits_error: Option<String>,
    /// コミット diff 取得エラー
    pub diff_error: Option<String>,
    /// 非同期レスポンス競合防止: 現在取得中のコミット SHA
    pub pending_diff_sha: Option<String>,
    /// コミット一覧レシーバー
    pub(crate) commit_list_receiver:
        Option<mpsc::Receiver<Result<crate::github::CommitListPage, String>>>,
    /// コミット diff レシーバー（(sha, diff_text) タプル）
    pub(crate) commit_diff_receiver: Option<mpsc::Receiver<Result<(String, String), String>>>,
}

impl Default for GitLogState {
    fn default() -> Self {
        Self::new()
    }
}

impl GitLogState {
    pub fn new() -> Self {
        Self::with_max_cache(MAX_STORE_ENTRIES)
    }

    pub fn with_max_cache(max_diff_cache: usize) -> Self {
        Self {
            commits: Vec::new(),
            selected_commit: 0,
            commit_list_scroll_offset: 0,
            commit_diff: None,
            diff_store: DiffCacheStore::new(max_diff_cache),
            diff_scroll: DiffScrollState::new(ScrollMode::Edge),
            diff_loading: false,
            commits_loading: true,
            commits_has_more: false,
            commits_page: 1,
            commits_error: None,
            diff_error: None,
            pending_diff_sha: None,
            commit_list_receiver: None,
            commit_diff_receiver: None,
        }
    }
}

/// Git status のファイルステータス
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Ignored,
    Unmerged,
}

impl FileStatus {
    pub fn from_char(c: char) -> Self {
        match c {
            ' ' | '.' => Self::Unmodified,
            'M' => Self::Modified,
            'A' => Self::Added,
            'D' => Self::Deleted,
            'R' => Self::Renamed,
            'C' => Self::Copied,
            '?' => Self::Untracked,
            '!' => Self::Ignored,
            'U' => Self::Unmerged,
            _ => Self::Unmodified,
        }
    }

    pub fn as_char(self) -> char {
        match self {
            Self::Unmodified => ' ',
            Self::Modified => 'M',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::Untracked => '?',
            Self::Ignored => '!',
            Self::Unmerged => 'U',
        }
    }
}

/// git status --porcelain=v1 の1エントリ
#[derive(Debug, Clone)]
pub struct GitStatusEntry {
    pub path: String,
    pub index_status: FileStatus,
    pub worktree_status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub staged_additions: u32,
    pub staged_deletions: u32,
    pub orig_path: Option<String>,
    pub unmerged: bool,
}

impl GitStatusEntry {
    /// staged 状態か（index が Unmodified/Untracked/Ignored 以外）
    pub fn is_staged(&self) -> bool {
        !matches!(
            self.index_status,
            FileStatus::Unmodified | FileStatus::Untracked | FileStatus::Ignored
        )
    }

    /// worktree 変更があるか
    pub fn has_worktree_changes(&self) -> bool {
        !matches!(
            self.worktree_status,
            FileStatus::Unmodified | FileStatus::Ignored
        )
    }

    /// 変更種別ラベル: ファイルの性質を固定幅2文字で返す
    ///
    /// stage/unstage で変化しない。色だけが変わる。
    /// 判定ロジック: index/worktree の両方を見て「このファイルは何の変更か」を決定。
    /// optimistic_stage/unstage で index/worktree が入れ替わっても結果が同じになるよう、
    /// 両方の非trivialな状態から種別を判定する。
    pub fn change_type_label(&self) -> &'static str {
        // untracked/added は同じ「新規ファイル」
        if self.index_status == FileStatus::Untracked
            || self.worktree_status == FileStatus::Untracked
            || (self.index_status == FileStatus::Added
                && self.worktree_status == FileStatus::Unmodified)
        {
            return "??";
        }

        // index 側が非trivial ならそれを使う（staged 状態）
        let kind = if self.index_status != FileStatus::Unmodified {
            self.index_status
        } else {
            self.worktree_status
        };

        match kind {
            FileStatus::Modified => "M ",
            FileStatus::Added => "A ",
            FileStatus::Deleted => "D ",
            FileStatus::Renamed => "R ",
            FileStatus::Copied => "C ",
            FileStatus::Unmerged => "U ",
            _ => "  ",
        }
    }
}

/// git update-index --cacheinfo で使用するインデックスエントリ
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub mode: String,
    pub hash: String,
    pub path: String,
}

/// Undo スタックのアクション
pub enum UndoAction {
    /// commit を取り消す（git reset --soft HEAD~1）
    Commit,
    /// stage を取り消す（インデックスを前の状態に精密復元）
    ///
    /// `previous_index_entries` に操作前のインデックスエントリを保持。
    /// MM ファイルの部分ステージを安全に復元するため、
    /// `git restore --staged` ではなく `git update-index --cacheinfo` を使用。
    Stage {
        paths: Vec<String>,
        previous_index_entries: Vec<IndexEntry>,
    },
    /// unstage を取り消す（git add -- <paths>）
    Unstage { paths: Vec<String> },
    /// stage all を取り消す（インデックスツリーを前の状態に復元）
    ///
    /// `tree_hash` に `git write-tree` で保存したツリーハッシュを保持。
    /// undo 時に `git read-tree` で完全復元。
    StageAll { tree_hash: Option<String> },
}

/// GitOps 画面の全状態
pub struct GitOpsState {
    pub entries: Vec<GitStatusEntry>,
    pub selected_index: usize,
    pub tree_scroll_offset: usize,
    pub diff_store: DiffCacheStore<String>,
    pub diff_scroll: DiffScrollState,
    /// 呼び出し元の AppState（close 時に復帰）
    pub return_state: AppState,
    /// 非同期 git status 受信
    pub(crate) status_receiver: Option<mpsc::Receiver<Result<Vec<GitStatusEntry>, String>>>,
    /// 非同期 git diff patch 受信（ファイルパスごとの on-demand diff）
    pub(crate) diff_patch_receiver: Option<mpsc::Receiver<SingleFileDiffResult>>,
    /// 非同期 git 操作結果受信（stage/unstage/discard/commit undo etc.）
    pub(crate) op_receiver: Option<mpsc::Receiver<Result<String, String>>>,
    /// 操作結果メッセージ（タイマー付き自動消去）
    pub op_message: Option<(String, std::time::Instant)>,
    /// Undo スタック
    pub undo_stack: Vec<UndoAction>,
    /// ツリービュー: 展開中ディレクトリのパス集合
    pub expanded_dirs: std::collections::HashSet<String>,
    /// 表示行 → entries インデックスのマッピング（ツリー表示用）
    pub visible_rows: Vec<TreeRow>,
    /// status 更新フラグ（prefetch トリガー用）
    pub(crate) status_updated: bool,
}

/// ツリー表示の1行
#[derive(Debug, Clone)]
pub enum TreeRow {
    /// ディレクトリ行（パス, 深さ, 展開中か）
    Dir(String, usize, bool),
    /// ファイル行（entries インデックス, 深さ）
    File(usize, usize),
}

impl GitOpsState {
    pub fn new(entries: Vec<GitStatusEntry>) -> Self {
        Self {
            entries,
            selected_index: 0,
            tree_scroll_offset: 0,
            diff_store: DiffCacheStore::new(MAX_STORE_ENTRIES),
            diff_scroll: DiffScrollState::new(ScrollMode::Margin),
            return_state: AppState::FileList,
            status_receiver: None,
            diff_patch_receiver: None,
            op_receiver: None,
            op_message: None,
            undo_stack: Vec::new(),
            expanded_dirs: std::collections::HashSet::new(),
            visible_rows: Vec::new(),
            status_updated: false,
        }
    }

    /// staged ファイルが存在するか
    pub fn has_staged_files(&self) -> bool {
        self.entries.iter().any(|e| e.is_staged())
    }

    /// unmerged ファイルが存在するか
    pub fn has_unmerged_files(&self) -> bool {
        self.entries.iter().any(|e| e.unmerged)
    }

    /// 現在選択中のエントリのパスを返す
    pub fn selected_path(&self) -> Option<&str> {
        match self.visible_rows.get(self.selected_index) {
            Some(TreeRow::File(idx, _)) => self.entries.get(*idx).map(|e| e.path.as_str()),
            _ => None,
        }
    }
}

/// Receiver with origin issue_number tracking (stale response prevention)
pub(crate) type IssueReceiver<T> = Option<(u32, mpsc::Receiver<Result<T, String>>)>;

/// Issue詳細画面のフォーカス
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum IssueDetailFocus {
    #[default]
    Body,
    LinkedPrs,
}

/// Issue画面の全状態（GitLogState パターン）
///
/// `App.issue_state: Option<IssueState>` として保持。
/// 画面クローズ時に `None` で全破棄。
pub struct IssueState {
    // List
    pub issues: Option<Vec<IssueSummary>>,
    pub selected_issue: usize,
    pub issue_list_scroll_offset: usize,
    pub issue_list_loading: bool,
    pub issue_list_has_more: bool,
    pub issue_list_state_filter: IssueStateFilter,
    pub issue_list_filter: Option<crate::filter::ListFilter>,
    /// true = load_more_issues (append), false = reload (replace)
    pub(crate) issue_list_appending: bool,
    // Detail
    pub issue_detail: Option<IssueDetail>,
    pub issue_detail_loading: bool,
    pub issue_detail_scroll_offset: usize,
    pub issue_detail_cache: Option<DiffCache>,
    pub selected_linked_pr: usize,
    pub detail_focus: IssueDetailFocus,
    // Comment list
    pub issue_comments: Option<Vec<IssueComment>>,
    pub selected_issue_comment: usize,
    pub issue_comment_list_scroll_offset: usize,
    pub issue_comment_detail_mode: bool,
    pub issue_comment_detail_scroll: usize,
    // Comment submission
    pub(crate) issue_comment_submit_receiver:
        Option<(u32, mpsc::Receiver<Result<IssueComment, String>>)>,
    pub(crate) issue_comment_submitting: bool,
    // Linked PRs（IssueDetail から分離管理）
    pub linked_prs: Option<Vec<LinkedPr>>,
    pub linked_prs_loading: bool,
    // Receivers（origin issue_number 追跡で stale 防止）
    pub(crate) issue_list_receiver: Option<mpsc::Receiver<Result<IssueListPage, String>>>,
    pub(crate) issue_detail_receiver: IssueReceiver<IssueDetail>,
    pub(crate) linked_prs_receiver: IssueReceiver<Vec<LinkedPr>>,
}

impl Default for IssueState {
    fn default() -> Self {
        Self::new()
    }
}

impl IssueState {
    pub fn new() -> Self {
        Self {
            issues: None,
            selected_issue: 0,
            issue_list_scroll_offset: 0,
            issue_list_loading: false,
            issue_list_has_more: false,
            issue_list_state_filter: IssueStateFilter::default(),
            issue_list_filter: None,
            issue_list_appending: false,
            issue_detail: None,
            issue_detail_loading: false,
            issue_detail_scroll_offset: 0,
            issue_detail_cache: None,
            issue_comments: None,
            selected_issue_comment: 0,
            issue_comment_list_scroll_offset: 0,
            issue_comment_detail_mode: false,
            issue_comment_detail_scroll: 0,
            issue_comment_submit_receiver: None,
            issue_comment_submitting: false,
            selected_linked_pr: 0,
            detail_focus: IssueDetailFocus::default(),
            linked_prs: None,
            linked_prs_loading: false,
            issue_list_receiver: None,
            issue_detail_receiver: None,
            linked_prs_receiver: None,
        }
    }
}

/// リポジトリ全体検索の結果
#[derive(Debug, Clone)]
pub struct RepoSymbolSearchResult {
    pub file_path: String,
    pub line_number: usize,
    pub repo_root: String,
}

/// リポジトリ全体シンボル検索の非同期更新
pub enum SymbolSearchUpdate {
    Found(RepoSymbolSearchResult),
    NotFound,
    Failed(String),
}

/// リポジトリ全体シンボル検索の状態
pub enum SymbolSearchState {
    Idle,
    Searching {
        receiver: mpsc::Receiver<SymbolSearchUpdate>,
        origin_file_index: usize,
    },
    Ready(RepoSymbolSearchResult, usize),
}

impl SymbolSearchState {
    /// 検索中かどうか
    pub fn is_searching(&self) -> bool {
        matches!(self, Self::Searching { .. })
    }

    /// submission_result に表示するためのタイムスタンプ付き結果を生成
    pub fn take_ready(&mut self) -> Option<RepoSymbolSearchResult> {
        if matches!(self, Self::Ready(..)) {
            let old = std::mem::replace(self, Self::Idle);
            if let Self::Ready(result, _) = old {
                return Some(result);
            }
        }
        None
    }
}
