use lasso::{Rodeo, Spur};
use ratatui::style::Style;
use smallvec::SmallVec;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::ai::orchestrator::RallyEvent;
use crate::ai::RallyState;
use crate::diff::LineType;
use crate::diff_store::{DiffCacheStore, DiffScrollState, ScrollMode, MAX_STORE_ENTRIES};
use crate::github::{
    ChangedFile, CommitListPage, IssueComment, IssueDetail, IssueListPage, IssueStateFilter,
    IssueSummary, LinkedPr, PrCommit, PullRequest,
};
use crate::loader::SingleFileDiffResult;

/// Position of a comment within the diff.
#[derive(Debug, Clone)]
pub struct CommentPosition {
    pub diff_line_index: usize,
    pub comment_index: usize,
}

/// Single entry in the jump history stack (Go to Definition / Jump Back).
#[derive(Debug, Clone)]
pub struct JumpLocation {
    pub file_index: usize,
    pub line_index: usize,
    pub scroll_offset: usize,
}

/// State for the symbol selection popup.
#[derive(Debug, Clone)]
pub struct SymbolPopupState {
    /// Candidate symbols: (name, start, end).
    pub symbols: Vec<(String, usize, usize)>,
    pub selected: usize,
}

/// Interned span: a 4-byte `Spur` reference + style, reducing allocations
/// for repeated tokens.
#[derive(Clone)]
pub struct InternedSpan {
    pub content: Spur,
    pub style: Style,
}

pub type SpanVec = SmallVec<[InternedSpan; 8]>;

/// Cached diff line with syntax-highlighted spans.
#[derive(Clone)]
pub struct CachedDiffLine {
    /// Base spans (without REVERSED modifier).
    pub spans: SpanVec,
    /// Used to determine background color.
    pub line_type: LineType,
}

/// Diff rendering cache.
pub struct DiffCache {
    pub file_index: usize,
    /// Hash of the patch content for change detection.
    pub patch_hash: u64,
    pub lines: Vec<CachedDiffLine>,
    /// String interner shared across this cache.
    pub interner: Rodeo,
    /// False for plain caches (diff coloring only).
    pub highlighted: bool,
    pub markdown_rich: bool,
}

impl DiffCache {
    /// Resolve a `Spur` to a string reference.
    ///
    /// Lifetime is tied to DiffCache, enabling zero-copy rendering.
    pub fn resolve(&self, spur: Spur) -> &str {
        self.interner.resolve(&spur)
    }
}

/// Compute a hash for the given string.
pub fn hash_string(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Multiline selection state.
#[derive(Debug, Clone)]
pub struct MultilineSelection {
    /// Anchor line (diff index). Set on Shift+Enter.
    pub anchor_line: usize,
    /// Cursor line (diff index). Updated on cursor movement.
    pub cursor_line: usize,
}

impl MultilineSelection {
    /// First line of the selection (the smaller index).
    pub fn start(&self) -> usize {
        self.anchor_line.min(self.cursor_line)
    }

    /// Last line of the selection (the larger index).
    pub fn end(&self) -> usize {
        self.anchor_line.max(self.cursor_line)
    }
}

/// Line-based input context shared by comment and suggestion modes.
#[derive(Debug, Clone)]
pub struct LineInputContext {
    pub file_index: usize,
    pub line_number: u32,
    /// 1-based position within the patch; maps to GitHub API `position`.
    pub diff_position: u32,
    /// Start line number in the new file (for multiline selections).
    pub start_line_number: Option<u32>,
}

/// Unified input mode.
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
    IssueList,
    IssueDetail,
    IssueCommentList,
    GitOpsSplitTree,
    GitOpsSplitDiff,
    Cockpit,
}

impl AppState {
    /// Whether this screen is independent of PR DataState.
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
                | Self::Cockpit
        )
    }

    /// Whether this is an Issue-related screen.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitMenuItem {
    PrList,
    IssueList,
    LocalDiff,
    GitOps,
}

impl CockpitMenuItem {
    pub const ALL: [Self; 4] = [Self::PrList, Self::IssueList, Self::LocalDiff, Self::GitOps];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Self {
        Self::ALL[i.min(Self::ALL.len() - 1)]
    }

    pub fn next(self) -> Self {
        Self::from_index(self.index().saturating_add(1))
    }

    pub fn prev(self) -> Self {
        Self::from_index(self.index().saturating_sub(1))
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PrList => "PR List",
            Self::IssueList => "Issue List",
            Self::LocalDiff => "Local Diff",
            Self::GitOps => "Git Ops",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::PrList => "Browse pull requests",
            Self::IssueList => "Browse issues",
            Self::LocalDiff => "View local git diff",
            Self::GitOps => "Git operations (stage, commit, push)",
        }
    }

    /// Whether this item requires a GitHub repo to function.
    pub fn requires_repo(self) -> bool {
        matches!(self, Self::PrList | Self::IssueList)
    }
}

pub struct CockpitState {
    pub selected_item: CockpitMenuItem,
    pub mentioned_issues_count: LoadState<u32>,
    pub review_prs_count: LoadState<u32>,
    pub(crate) mentioned_receiver: Option<mpsc::Receiver<Result<u32, String>>>,
    pub(crate) review_receiver: Option<mpsc::Receiver<Result<u32, String>>>,
    pub repo_available: bool,
}

impl CockpitState {
    pub fn new(repo_available: bool) -> Self {
        Self {
            selected_item: CockpitMenuItem::PrList,
            mentioned_issues_count: if repo_available {
                LoadState::Loading
            } else {
                LoadState::NotLoaded
            },
            review_prs_count: if repo_available {
                LoadState::Loading
            } else {
                LoadState::NotLoaded
            },
            mentioned_receiver: None,
            review_receiver: None,
            repo_available,
        }
    }
}

/// Retry request variants dispatched through the unified retry loop.
#[derive(Debug, Clone)]
pub enum RefreshRequest {
    PrRefresh { pr_number: u32 },
    LocalRefresh,
}

/// Result of a file viewed-state mutation.
#[derive(Debug, Clone)]
pub(super) enum MarkViewedResult {
    Completed {
        marked_paths: Vec<String>,
        total_targets: usize,
        error: Option<String>,
        set_viewed: bool,
    },
}

/// Handle for the file watcher thread.
///
/// The `active` flag gates callback processing.
/// The thread itself lives in `_thread` and survives until process exit.
pub struct WatcherHandle {
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) _thread: std::thread::JoinHandle<()>,
}

/// PR data loading state.
///
/// Fields use `Box`/`Vec` instead of `Arc` because the app is single-threaded;
/// data is shared with `SessionCache` via `clone()` (only on PR refresh).
#[derive(Debug, Clone)]
pub enum DataState {
    Loading,
    Loaded {
        pr: Box<PullRequest>,
        files: Vec<ChangedFile>,
    },
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub enum LoadState<T> {
    #[default]
    NotLoaded,
    Loading,
    LoadingMore(T),
    Loaded(T),
    Error(String),
}

impl<T> LoadState<T> {
    pub fn as_loaded(&self) -> Option<&T> {
        match self {
            Self::Loaded(t) | Self::LoadingMore(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_loaded_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Loaded(t) | Self::LoadingMore(t) => Some(t),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading | Self::LoadingMore(_))
    }

    pub fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded(_))
    }

    pub fn into_loaded(self) -> Option<T> {
        match self {
            Self::Loaded(t) | Self::LoadingMore(t) => Some(t),
            _ => None,
        }
    }

    /// Recover from a failed load-more by transitioning back to Loaded.
    /// Preserves existing data from LoadingMore/Loaded; uses fallback otherwise.
    pub fn recover_or(&mut self, fallback: T) {
        let taken = std::mem::take(self);
        match taken {
            Self::LoadingMore(t) | Self::Loaded(t) => *self = Self::Loaded(t),
            _ => *self = Self::Loaded(fallback),
        }
    }
}

/// Git status file status.
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

/// Single entry from `git status --porcelain=v1`.
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
    /// Whether this file is staged (index is not Unmodified/Untracked/Ignored).
    pub fn is_staged(&self) -> bool {
        !matches!(
            self.index_status,
            FileStatus::Unmodified | FileStatus::Untracked | FileStatus::Ignored
        )
    }

    /// Whether the worktree has changes.
    pub fn has_worktree_changes(&self) -> bool {
        !matches!(
            self.worktree_status,
            FileStatus::Unmodified | FileStatus::Ignored
        )
    }

    pub fn describe_discard_command(&self) -> String {
        if self.worktree_status == FileStatus::Untracked
            && self.index_status == FileStatus::Untracked
        {
            format!("git clean -f -- {}", self.path)
        } else if self.is_staged() && !self.has_worktree_changes() {
            format!("git restore --staged --source=HEAD -- {}", self.path)
        } else {
            format!("git restore -- {}", self.path)
        }
    }

    /// Fixed-width 2-char change type label.
    ///
    /// Stable across stage/unstage: both index and worktree statuses are
    /// considered so that optimistic stage/unstage swaps produce the same label.
    pub fn change_type_label(&self) -> &'static str {
        // untracked and added are both "new file"
        if self.index_status == FileStatus::Untracked
            || self.worktree_status == FileStatus::Untracked
            || (self.index_status == FileStatus::Added
                && self.worktree_status == FileStatus::Unmodified)
        {
            return "??";
        }

        // Prefer index status when non-trivial (staged state).
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

/// Index entry for `git update-index --cacheinfo`.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub mode: String,
    pub hash: String,
    pub path: String,
}

/// Undo stack action.
pub enum UndoAction {
    /// Undo commit (git reset --soft HEAD~1).
    Commit,
    /// Undo stage (precise index restoration).
    ///
    /// Uses `git update-index --cacheinfo` instead of `git restore --staged`
    /// to safely restore partial staging of MM files.
    Stage {
        paths: Vec<String>,
        previous_index_entries: Vec<IndexEntry>,
    },
    /// Undo unstage (git add -- <paths>).
    Unstage { paths: Vec<String> },
    /// Undo stage-all (restore index tree).
    ///
    /// `tree_hash` holds the tree hash saved by `git write-tree`;
    /// undo restores it via `git read-tree`.
    StageAll { tree_hash: Option<String> },
}

impl UndoAction {
    pub fn describe_command(&self) -> String {
        match self {
            UndoAction::Commit => "git reset --soft HEAD~1".to_string(),
            UndoAction::Stage { paths, .. } => {
                format!("git update-index (restore {} file(s))", paths.len())
            }
            UndoAction::Unstage { paths } => {
                if paths.len() == 1 {
                    format!("git add -- {}", paths[0])
                } else {
                    format!("git add -- ({} files)", paths.len())
                }
            }
            UndoAction::StageAll { tree_hash } => {
                if let Some(hash) = tree_hash {
                    format!("git read-tree {}", &hash[..hash.len().min(7)])
                } else {
                    "git reset".to_string()
                }
            }
        }
    }

    pub fn to_destructive_op(&self) -> DestructiveOp {
        match self {
            UndoAction::Commit => DestructiveOp::UndoCommit,
            UndoAction::Stage { paths, .. } => DestructiveOp::UndoStage {
                paths: paths.clone(),
            },
            UndoAction::Unstage { paths } => DestructiveOp::UndoUnstage {
                paths: paths.clone(),
            },
            UndoAction::StageAll { tree_hash } => DestructiveOp::UndoStageAll {
                tree_hash: tree_hash.clone(),
            },
        }
    }
}

/// Structured destructive operation.
#[derive(Debug, Clone)]
pub enum DestructiveOp {
    Discard { path: String },
    UndoStage { paths: Vec<String> },
    UndoUnstage { paths: Vec<String> },
    UndoStageAll { tree_hash: Option<String> },
    UndoCommit,
    ResetSoft { sha: String, head_offset: usize },
    ForcePush { branch: String },
}

impl DestructiveOp {
    /// Generate operation strings for gitfilm (one per element).
    pub fn to_gitfilm_args(&self) -> Vec<String> {
        match self {
            Self::Discard { path } => vec![format!("restore {}", path)],
            Self::UndoStage { paths } => {
                vec![format!("reset --mixed HEAD -- {}", paths.join(" "))]
            }
            Self::UndoUnstage { paths } => {
                vec![format!("add {}", paths.join(" "))]
            }
            Self::UndoStageAll { tree_hash } => {
                if let Some(hash) = tree_hash {
                    vec![format!("reset --mixed {}", hash)]
                } else {
                    vec!["reset".into()]
                }
            }
            Self::UndoCommit => vec!["reset --soft HEAD~1".into()],
            Self::ResetSoft { head_offset, .. } => {
                vec![format!("reset --soft HEAD~{}", head_offset)]
            }
            Self::ForcePush { .. } => vec![],
        }
    }

    /// Human-readable command string for display.
    pub fn display_command(&self) -> String {
        match self {
            Self::Discard { path } => format!("git restore -- {}", path),
            Self::UndoStage { paths } => format!("git reset -- {}", paths.join(" ")),
            Self::UndoUnstage { paths } => format!("git add {}", paths.join(" ")),
            Self::UndoStageAll { tree_hash } => {
                if let Some(hash) = tree_hash {
                    format!("git read-tree {}", &hash[..hash.len().min(7)])
                } else {
                    "git reset".to_string()
                }
            }
            Self::UndoCommit => "git reset --soft HEAD~1".to_string(),
            Self::ResetSoft { sha, .. } => format!("git reset --soft {}", &sha[..sha.len().min(7)]),
            Self::ForcePush { branch } => format!("git push --force-with-lease origin {}", branch),
        }
    }
}

/// UI model for gitfilm simulation results.
#[derive(Debug, Clone)]
pub struct SimulationPreview {
    pub before: crate::gitfilm::GitfilmAreaSnapshot,
    pub after: crate::gitfilm::GitfilmAreaSnapshot,
}

/// Confirmation modal content.
#[derive(Debug, Clone)]
pub enum SimulationResult {
    Success(SimulationPreview),
    /// Message-only confirmation without simulation (e.g. force push).
    Message(String),
}

/// Pending confirmation state for GitOps destructive operations.
#[derive(Debug, Clone)]
pub enum PendingGitOpsConfirm {
    /// Fallback when gitfilm is unavailable (legacy-compatible).
    Simple { op: DestructiveOp },
    /// gitfilm simulation in progress.
    Simulating { op: DestructiveOp, abort_id: u64 },
    /// Displaying simulation results (modal).
    Previewing {
        op: DestructiveOp,
        result: SimulationResult,
        scroll_offset: usize,
    },
}

/// GitOps left pane sub-focus.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LeftPaneFocus {
    #[default]
    Tree,
    Commits,
}

/// Commit history state (sub-struct of GitOpsState).
pub struct CommitLogState {
    pub commits: Vec<PrCommit>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub diff_store: DiffCacheStore<String>,
    pub diff_scroll: DiffScrollState,
    pub diff_loading: bool,
    pub loading: bool,
    pub has_more: bool,
    pub page: u32,
    pub error: Option<String>,
    pub diff_error: Option<String>,
    pub pending_diff_sha: Option<String>,
    pub(crate) list_receiver: Option<mpsc::Receiver<Result<CommitListPage, String>>>,
    pub(crate) diff_receiver: Option<mpsc::Receiver<Result<(String, String), String>>>,
    pub initialized: bool,
}

impl Default for CommitLogState {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitLogState {
    pub fn new() -> Self {
        Self {
            commits: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            diff_store: DiffCacheStore::new(MAX_STORE_ENTRIES),
            diff_scroll: DiffScrollState::new(ScrollMode::Edge),
            diff_loading: false,
            loading: false,
            has_more: false,
            page: 0,
            error: None,
            diff_error: None,
            pending_diff_sha: None,
            list_receiver: None,
            diff_receiver: None,
            initialized: false,
        }
    }
}

/// Full state for the GitOps screen.
pub struct GitOpsState {
    pub entries: Vec<GitStatusEntry>,
    /// Tree view state (delegated to FileTreeState).
    pub tree: crate::app::file_tree::FileTreeState,
    pub diff_store: DiffCacheStore<String>,
    pub diff_scroll: DiffScrollState,
    /// Caller's AppState to restore on close.
    pub return_state: AppState,
    pub(crate) status_receiver: Option<mpsc::Receiver<Result<Vec<GitStatusEntry>, String>>>,
    /// Per-file on-demand diff patch receiver.
    pub(crate) diff_patch_receiver: Option<mpsc::Receiver<SingleFileDiffResult>>,
    /// Git operation result receiver (stage/unstage/discard/commit undo etc.).
    pub(crate) op_receiver: Option<mpsc::Receiver<Result<String, String>>>,
    /// Operation result message with auto-clear timer.
    pub op_message: Option<(String, std::time::Instant)>,
    pub undo_stack: Vec<UndoAction>,
    pub pending_confirm: Option<PendingGitOpsConfirm>,
    /// Status-updated flag used to trigger prefetch.
    pub(crate) status_updated: bool,
    pub pushing: bool,
    /// Number of local commits ahead of the remote.
    pub ahead_count: u32,
    pub(crate) ahead_receiver: Option<mpsc::Receiver<u32>>,
    pub left_focus: LeftPaneFocus,
    /// Left sub-pane to return to from diff.
    pub left_return_focus: LeftPaneFocus,
    pub commit_log: CommitLogState,
    /// Cached gitfilm binary path (resolved once at init).
    pub gitfilm_path: Option<std::path::PathBuf>,
    pub(crate) simulate_receiver: Option<(
        u64,
        mpsc::Receiver<Result<crate::gitfilm::GitfilmSimOutput, String>>,
    )>,
}

/// Single row in the tree view.
#[derive(Debug, Clone)]
pub enum TreeRow {
    /// Directory row.
    Dir {
        path: String,
        depth: usize,
        expanded: bool,
    },
    /// File row.
    File { index: usize, depth: usize },
}

impl GitOpsState {
    pub fn new(entries: Vec<GitStatusEntry>) -> Self {
        Self {
            entries,
            tree: crate::app::file_tree::FileTreeState::new(),
            diff_store: DiffCacheStore::new(MAX_STORE_ENTRIES),
            diff_scroll: DiffScrollState::new(ScrollMode::Margin),
            return_state: AppState::FileList,
            status_receiver: None,
            diff_patch_receiver: None,
            op_receiver: None,
            op_message: None,
            undo_stack: Vec::new(),
            pending_confirm: None,
            pushing: false,
            ahead_count: 0,
            ahead_receiver: None,
            status_updated: false,
            left_focus: LeftPaneFocus::Tree,
            left_return_focus: LeftPaneFocus::Tree,
            commit_log: CommitLogState::new(),
            gitfilm_path: crate::gitfilm::extract_gitfilm(),
            simulate_receiver: None,
        }
    }

    /// Whether any staged files exist.
    pub fn has_staged_files(&self) -> bool {
        self.entries.iter().any(|e| e.is_staged())
    }

    /// Whether any unmerged files exist.
    pub fn has_unmerged_files(&self) -> bool {
        self.entries.iter().any(|e| e.unmerged)
    }

    /// Return the path of the currently selected entry.
    pub fn selected_path(&self) -> Option<&str> {
        self.tree
            .selected_file_index()
            .and_then(|idx| self.entries.get(idx).map(|e| e.path.as_str()))
    }
}

/// Receiver with origin issue_number tracking (stale response prevention)
pub(crate) type IssueReceiver<T> = Option<(u32, mpsc::Receiver<Result<T, String>>)>;

/// Focus target in the issue detail screen.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum IssueDetailFocus {
    #[default]
    Body,
    LinkedPrs,
}

/// Full state for the Issue screen.
///
/// Held as `App.issue_state: Option<IssueState>`;
/// set to `None` on screen close to discard everything.
pub struct IssueState {
    // List
    pub issues: LoadState<Vec<IssueSummary>>,
    pub selected_issue: usize,
    pub issue_list_scroll_offset: usize,
    pub issue_list_has_more: bool,
    pub issue_list_state_filter: IssueStateFilter,
    pub issue_list_filter: Option<crate::filter::ListFilter>,
    // Detail
    pub issue_detail: LoadState<IssueDetail>,
    pub issue_detail_scroll_offset: usize,
    pub issue_detail_cache: Option<DiffCache>,
    pub selected_linked_pr: usize,
    pub detail_focus: IssueDetailFocus,
    pub issue_comments: Option<Vec<IssueComment>>,
    pub selected_issue_comment: usize,
    pub issue_comment_list_scroll_offset: usize,
    pub issue_comment_detail_mode: bool,
    pub issue_comment_detail_scroll: usize,
    // Comment submission
    pub(crate) issue_comment_submit_receiver:
        Option<(u32, mpsc::Receiver<Result<IssueComment, String>>)>,
    pub(crate) issue_comment_submitting: bool,
    // Linked PRs (managed separately from IssueDetail)
    pub linked_prs: LoadState<Vec<LinkedPr>>,
    // Receivers (track origin issue_number to prevent stale updates)
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
            issues: LoadState::NotLoaded,
            selected_issue: 0,
            issue_list_scroll_offset: 0,
            issue_list_has_more: false,
            issue_list_state_filter: IssueStateFilter::default(),
            issue_list_filter: None,
            issue_detail: LoadState::NotLoaded,
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
            linked_prs: LoadState::NotLoaded,
            issue_list_receiver: None,
            issue_detail_receiver: None,
            linked_prs_receiver: None,
        }
    }
}

#[derive(Default)]
pub struct CommentState {
    pub review_comments: Option<Vec<crate::github::comment::ReviewComment>>,
    /// Local-only metadata (resolved state) keyed by comment id. Populated from
    /// the on-disk [`crate::cache::LocalReviewComment`] records when in local
    /// mode; empty otherwise.
    pub local_comment_meta: std::collections::HashMap<u64, crate::cache::LocalCommentMeta>,
    pub selected_comment: usize,
    pub comment_list_scroll_offset: usize,
    pub comments_loading: bool,
    pub file_comment_positions: Vec<CommentPosition>,
    pub file_comment_lines: std::collections::HashSet<usize>,
    pub file_comment_counts: std::collections::HashMap<String, usize>,
    pub comment_panel_open: bool,
    pub comment_panel_scroll: u16,
    pub comment_tab: CommentTab,
    pub discussion_comments: Option<Vec<crate::github::comment::DiscussionComment>>,
    pub selected_discussion_comment: usize,
    pub discussion_comment_list_scroll_offset: usize,
    pub discussion_comments_loading: bool,
    pub discussion_comment_detail_mode: bool,
    pub discussion_comment_detail_scroll: usize,
    pub(crate) comment_receiver:
        super::PrReceiver<Result<Vec<crate::github::comment::ReviewComment>, String>>,
    pub(crate) discussion_comment_receiver:
        super::PrReceiver<Result<Vec<crate::github::comment::DiscussionComment>, String>>,
    pub(crate) comment_submit_receiver: super::PrReceiver<crate::loader::CommentSubmitResult>,
    pub comment_submitting: bool,
    pub submission_result: Option<(bool, String)>,
    pub(crate) submission_result_time: Option<std::time::Instant>,
    pub(crate) pending_approve_body: Option<String>,
    pub selected_inline_comment: usize,
}

#[derive(Default)]
pub struct PrListState {
    pub pr_list: LoadState<Vec<crate::github::PullRequestSummary>>,
    pub selected_pr: usize,
    pub pr_list_scroll_offset: usize,
    pub pr_list_has_more: bool,
    pub pr_list_state_filter: crate::github::PrStateFilter,
    pub pr_list_filter: Option<crate::filter::ListFilter>,
    pub(crate) pr_list_receiver:
        Option<tokio::sync::mpsc::Receiver<Result<crate::github::PrListPage, String>>>,
}

pub struct ChecksState {
    pub checks: Option<Vec<crate::github::CheckItem>>,
    pub selected_check: usize,
    pub checks_scroll_offset: usize,
    pub checks_loading: bool,
    pub checks_target_pr: Option<u32>,
    pub checks_return_state: AppState,
    pub ci_status: Option<crate::github::CiStatus>,
    pub(crate) checks_receiver: super::PrReceiver<Result<Vec<crate::github::CheckItem>, String>>,
    pub(crate) ci_status_receiver: Option<tokio::sync::mpsc::Receiver<crate::github::CiStatus>>,
}

impl Default for ChecksState {
    fn default() -> Self {
        Self {
            checks: None,
            selected_check: 0,
            checks_scroll_offset: 0,
            checks_loading: false,
            checks_target_pr: None,
            checks_return_state: AppState::FileList,
            ci_status: None,
            checks_receiver: None,
            ci_status_receiver: None,
        }
    }
}

/// Repository-wide symbol search result.
#[derive(Debug, Clone)]
pub struct RepoSymbolSearchResult {
    pub file_path: String,
    pub line_number: usize,
    pub repo_root: String,
}

/// Async update for repository-wide symbol search.
pub enum SymbolSearchUpdate {
    Found(RepoSymbolSearchResult),
    NotFound,
    Failed(String),
}

/// Repository-wide symbol search state.
pub enum SymbolSearchState {
    Idle,
    Searching {
        receiver: mpsc::Receiver<SymbolSearchUpdate>,
        origin_file_index: usize,
    },
    Ready(RepoSymbolSearchResult, usize),
}

impl SymbolSearchState {
    /// Whether a search is in progress.
    pub fn is_searching(&self) -> bool {
        matches!(self, Self::Searching { .. })
    }

    /// Generate a timestamped result for display in submission_result.
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

/// Operates as an overlay without changing AppState, so shell commands work in any screen.
#[derive(Debug, Clone)]
pub struct ShellState {
    pub input: String,
    pub cursor: usize,
    pub phase: ShellPhase,
    pub scroll_offset: usize,
}

#[derive(Debug, Clone)]
pub enum ShellPhase {
    Input,
    Running,
    Cancelling,
    Done(ShellCommandResult),
}

#[derive(Debug, Clone)]
pub struct ShellCommandResult {
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    /// Pre-built at Done transition to avoid re-computing on every render.
    pub cached_lines: Vec<CachedShellLine>,
    pub total_lines: usize,
}

#[derive(Debug, Clone)]
pub struct CachedShellLine {
    pub text: String,
    pub is_stderr: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cockpit_menu_item_next_clamps_at_last() {
        assert_eq!(CockpitMenuItem::PrList.next(), CockpitMenuItem::IssueList);
        assert_eq!(
            CockpitMenuItem::IssueList.next(),
            CockpitMenuItem::LocalDiff
        );
        assert_eq!(CockpitMenuItem::LocalDiff.next(), CockpitMenuItem::GitOps);
        assert_eq!(CockpitMenuItem::GitOps.next(), CockpitMenuItem::GitOps);
    }

    #[test]
    fn cockpit_menu_item_prev_clamps_at_first() {
        assert_eq!(CockpitMenuItem::GitOps.prev(), CockpitMenuItem::LocalDiff);
        assert_eq!(
            CockpitMenuItem::LocalDiff.prev(),
            CockpitMenuItem::IssueList
        );
        assert_eq!(CockpitMenuItem::IssueList.prev(), CockpitMenuItem::PrList);
        assert_eq!(CockpitMenuItem::PrList.prev(), CockpitMenuItem::PrList);
    }

    #[test]
    fn cockpit_menu_item_from_index_clamps_overflow() {
        assert_eq!(CockpitMenuItem::from_index(0), CockpitMenuItem::PrList);
        assert_eq!(CockpitMenuItem::from_index(3), CockpitMenuItem::GitOps);
        assert_eq!(CockpitMenuItem::from_index(100), CockpitMenuItem::GitOps);
    }

    #[test]
    fn cockpit_menu_item_requires_repo() {
        assert!(CockpitMenuItem::PrList.requires_repo());
        assert!(CockpitMenuItem::IssueList.requires_repo());
        assert!(!CockpitMenuItem::LocalDiff.requires_repo());
        assert!(!CockpitMenuItem::GitOps.requires_repo());
    }

    #[test]
    fn cockpit_state_new_repo_available() {
        let state = CockpitState::new(true);
        assert!(state.repo_available);
        assert!(state.mentioned_issues_count.is_loading());
        assert!(state.review_prs_count.is_loading());
    }

    #[test]
    fn cockpit_state_new_repo_unavailable() {
        let state = CockpitState::new(false);
        assert!(!state.repo_available);
        assert!(!state.mentioned_issues_count.is_loading());
        assert!(!state.review_prs_count.is_loading());
    }

    #[test]
    fn cockpit_is_data_state_independent() {
        assert!(AppState::Cockpit.is_data_state_independent());
    }
}
