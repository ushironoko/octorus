use anyhow::Result;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::ai::orchestrator::{OrchestratorCommand, RallyEvent};
use crate::ai::prompt_loader::PromptLoader;
use crate::ai::Context as AiContext;
use crate::cache::SessionCache;
use crate::config::Config;
use crate::diff_store::{DiffCacheStore, DiffScrollState, ScrollMode, MAX_STORE_ENTRIES};
use crate::filter::ListFilter;
use crate::github;
use crate::keybinding::KeyBinding;
use crate::loader::{DataLoadResult, SingleFileDiffResult};
use crate::ui;
use crate::ui::text_area::TextArea;
use crate::ui::theme::{
    Attention, BrowseAttention, DiffAttention, GitOpsAttention, IssueDetailAttention,
    SplitAttention, Theme,
};
use std::time::Instant;

mod types;
pub use types::{
    hash_string, AiRallyState, AppState, CachedDiffLine, CachedShellLine, ChecksState,
    CockpitMenuItem, CockpitState, CommentPosition, CommentState, CommentTab, CommentThread,
    CommitLogState, DataState, DestructiveOp, DiffCache, DiscardInvocation, FileStatus,
    GitOpsState, GitStatusEntry, HelpTab, IndexEntry, InputMode, InternedSpan, IssueDetailFocus,
    IssueState, JumpLocation, LeftPaneFocus, LineInputContext, LoadState, LogEntry, LogEventType,
    MultilineSelection, PauseState, PendingGitOpsConfirm, PendingPostConfirmation, PermissionInfo,
    PrListState, PrOpenSource, RefreshRequest, RepoSymbolSearchResult, RepositoryAvailability,
    ReviewAction, ShellCommandResult, ShellPhase, ShellState, SimulationPreview, SimulationResult,
    SpanVec, SymbolPopupState, SymbolSearchState, SymbolSearchUpdate, TreeRow, UndoAction,
    WatcherHandle,
};
// Internal-only types (not re-exported from crate::app)
use types::MarkViewedResult;

mod ai_rally;
pub mod browse;
pub(crate) mod browse_discussion;
mod cockpit;
mod comments;
mod diff_cache;
pub mod file_tree;
mod filter;
mod git_ops;
mod input;
mod input_browse;
mod input_diff;
mod input_mouse;
mod input_text;
mod issue_detail;
mod issue_list;
mod key_sequence;
mod local_mode;
mod polling;
mod pr_list;
mod shell_command;
mod symbol;
#[cfg(test)]
mod tests;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// PR番号と紐づいたレシーバー（発信元PRを追跡してクロスPRキャッシュ汚染を防止）
pub(crate) type PrReceiver<T> = Option<(u32, mpsc::Receiver<T>)>;

/// サジェスチョン入力のシンタックスハイライトキャッシュ
///
/// 毎フレーム ParserPool を再生成するコストを回避するため、
/// コンテンツのハッシュ・ファイル名・テーマが変わった時のみハイライトを再構築する。
pub struct SuggestionHighlightCache {
    pub content_hash: u64,
    pub filename: String,
    pub theme_name: String,
    pub lines: Vec<ratatui::text::Line<'static>>,
}

pub struct App {
    pub repo: String,
    repository_availability: RepositoryAvailability,
    /// 選択されたPR番号（PR一覧から選択した場合は後から設定）
    pub pr_number: Option<u32>,
    pub(crate) pr_open_source: PrOpenSource,
    pub data_state: DataState,
    pub state: AppState,
    // PR list state
    pub prs: PrListState,
    /// PR一覧から開始したかどうか（戻り先判定用）
    pub started_from_pr_list: bool,
    /// ローカル差分監視モードかどうか
    local_mode: bool,
    /// `--auto-focus` オプション（ローカル差分時）
    local_auto_focus: bool,
    pub(crate) zen_mode: bool,
    /// 直近のローカルファイル署名（差分変更を検出、base: patch 除外）
    local_file_signatures: HashMap<String, u64>,
    /// patch 内容を含む完全シグネチャ（バッチ diff 完了後に更新）
    local_file_patch_signatures: HashMap<String, u64>,
    /// CLI で指定された元の PR 番号（モード復帰用）
    original_pr_number: Option<u32>,
    /// ファイルウォッチャーハンドル（遅延生成）
    watcher_handle: Option<WatcherHandle>,
    /// ウォッチャー用 debounce フラグ（watcher スレッドと共有）
    refresh_pending: Option<Arc<AtomicBool>>,
    /// DiffView で q/Esc を押した時の戻り先
    pub diff_view_return_state: AppState,
    /// CommentPreview/SuggestionPreview の戻り先
    pub preview_return_state: AppState,
    /// Help/CommentList など汎用的な戻り先
    pub previous_state: AppState,
    pub selected_file: usize,
    pub file_list_scroll_offset: usize,
    pub diff_scroll: DiffScrollState,
    /// 複数行選択モードの状態（None = 非選択モード）
    pub multiline_selection: Option<MultilineSelection>,
    /// 統一入力モード
    pub input_mode: Option<InputMode>,
    /// 統一入力テキストエリア
    pub input_text_area: TextArea,
    pub config: Config,
    pub should_quit: bool,
    pub cmt: CommentState,
    pub diff_store: DiffCacheStore<usize>,
    /// ヘルプ画面のスクロールオフセット（行単位）
    pub help_scroll_offset: usize,
    /// ヘルプ画面の現在のタブ
    pub help_tab: HelpTab,
    /// Config タブのスクロールオフセット（行単位）
    pub config_scroll_offset: usize,
    pub ai_rally_state: Option<AiRallyState>,
    pub working_dir: Option<String>,
    // Receivers
    data_receiver: PrReceiver<DataLoadResult>,
    retry_sender: Option<mpsc::Sender<RefreshRequest>>,
    rally_event_receiver: Option<mpsc::Receiver<RallyEvent>>,
    // Handle for aborting the rally orchestrator task
    rally_abort_handle: Option<AbortHandle>,
    // Command sender to communicate with the orchestrator
    rally_command_sender: Option<mpsc::Sender<OrchestratorCommand>>,
    // Context saved while waiting for config warning confirmation
    pending_rally_context: Option<AiContext>,
    // PromptLoader saved while waiting for config warning confirmation
    pending_rally_prompt_loader: Option<PromptLoader>,
    // Seed review saved while waiting for config warning confirmation
    pending_rally_seed_review: Option<crate::ai::ReviewerOutput>,
    // Flag to start AI Rally when data is loaded (set by --ai-rally CLI flag)
    start_ai_rally_on_load: bool,
    // Pending AI Rally flag (set when --ai-rally is passed with PR list mode)
    pending_ai_rally: bool,
    // File viewed-state mutation results
    mark_viewed_receiver: PrReceiver<MarkViewedResult>,
    /// Spinner animation frame counter (incremented each tick)
    pub spinner_frame: usize,
    /// ジャンプ履歴スタック（Go to Definition / Jump Back 用）
    pub jump_stack: Vec<JumpLocation>,
    /// Pending keys for multi-key sequences (e.g., "gg", "gd")
    pub pending_keys: SmallVec<[KeyBinding; 4]>,
    /// Timestamp when pending keys started (for timeout)
    pub pending_since: Option<Instant>,
    /// シンボル選択ポップアップの状態
    pub symbol_popup: Option<SymbolPopupState>,
    /// リポジトリ全体シンボル検索の非同期状態
    pub symbol_search: SymbolSearchState,
    /// インメモリセッションキャッシュ
    pub session_cache: SessionCache,
    /// Markdown リッチ表示モード（見出し太字・斜体等を適用）
    markdown_rich: bool,
    /// サジェスチョン入力のシンタックスハイライトキャッシュ
    pub suggestion_highlight_cache: Option<SuggestionHighlightCache>,
    /// PR description 画面のスクロールオフセット
    pub pr_description_scroll_offset: usize,
    /// PR description 用の DiffCache（マークダウンリッチ表示）
    pub pr_description_cache: Option<DiffCache>,
    /// ファイル一覧のキーワードフィルタ
    pub file_list_filter: Option<ListFilter>,
    /// BG バッチ diff ロード結果の受信チャネル（Phase 2）
    batch_diff_receiver: Option<mpsc::Receiver<Vec<SingleFileDiffResult>>>,
    /// 単一ファイル diff のオンデマンド受信チャネル
    lazy_diff_receiver: Option<mpsc::Receiver<SingleFileDiffResult>>,
    /// 現在オンデマンドロード要求中のファイル名（重複リクエスト防止）
    lazy_diff_pending_file: Option<String>,
    pub chk: ChecksState,
    /// Git Log 画面の全状態（None = 非表示）
    /// GitOps 画面の全状態（None = 非表示）
    pub git_ops_state: Option<GitOpsState>,
    /// Issue 画面の全状態（None = 非表示）
    pub issue_state: Option<IssueState>,
    /// Issue詳細からPR遷移した際の復帰フラグ
    pub issue_detail_return: bool,
    /// Latest available version (None = not checked yet or up-to-date)
    pub update_available: Option<String>,
    update_check_receiver: Option<mpsc::Receiver<Option<String>>>,
    /// ファイル一覧のツリー表示モード ON/OFF
    pub tree_mode_active: bool,
    /// ファイルツリー状態（初回トグルで生成、展開状態を保持）
    pub file_tree_state: Option<file_tree::FileTreeState>,
    pub shell_state: Option<ShellState>,
    shell_result_receiver: Option<mpsc::Receiver<ShellCommandResult>>,
    shell_abort_handle: Option<AbortHandle>,
    /// Full cockpit screen state (None = cockpit inactive).
    pub cockpit_state: Option<CockpitState>,
    /// Root return destination when launched from cockpit.
    pub home_state: Option<AppState>,
    /// Repository Browser state (None = browser inactive).
    pub browse_state: Option<browse::BrowseState>,
    /// マウスキャプチャの現在状態(F2 トグルで切替、mouse.enabled=false なら常に false)
    pub mouse_capture_active: bool,
    /// ドレイン中に読んでしまった未処理イベントの保留キュー(次イテレーションで消費)
    pub(crate) pending_events: std::collections::VecDeque<crossterm::event::Event>,
    /// 直近フレームのヒット領域(描画時に登録、入力時に参照)
    pub hit_map: crate::ui::hit::HitMap,
    /// マウスドラッグの状態機械
    pub(crate) drag_state: input_mouse::DragState,
    pub theme: Theme,
}

/// 入力ハンドラ用の画面サイズ。`Terminal::size()` は実端末への問い合わせのため
/// tty がない環境(テスト、CI)で失敗する — viewport 領域なら I/O なしで取れる。
pub(crate) fn viewport_size(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> ratatui::layout::Size {
    terminal.get_frame().area().as_size()
}

impl App {
    fn base_app(repo: String, config: Config) -> Self {
        let submit_key = config.keybindings.submit.clone();
        let mouse_capture_active = config.mouse.enabled;
        let theme = Theme::resolve(&config.theme, config.no_color);
        Self {
            repo,
            repository_availability: RepositoryAvailability::Unavailable,
            pr_number: Some(1),
            pr_open_source: PrOpenSource::Standard,
            data_state: DataState::Loading,
            state: AppState::FileList,
            prs: PrListState::default(),
            started_from_pr_list: false,
            local_mode: false,
            local_auto_focus: false,
            zen_mode: false,
            local_file_signatures: HashMap::new(),
            local_file_patch_signatures: HashMap::new(),
            original_pr_number: None,
            watcher_handle: None,
            refresh_pending: None,
            diff_view_return_state: AppState::FileList,
            preview_return_state: AppState::DiffView,
            previous_state: AppState::FileList,
            selected_file: 0,
            file_list_scroll_offset: 0,
            diff_scroll: DiffScrollState::new(ScrollMode::Margin),
            multiline_selection: None,
            input_mode: None,
            input_text_area: TextArea::with_submit_key(submit_key),
            config,
            should_quit: false,
            cmt: CommentState::default(),
            diff_store: DiffCacheStore::new(MAX_STORE_ENTRIES),
            help_scroll_offset: 0,
            help_tab: HelpTab::default(),
            config_scroll_offset: 0,
            ai_rally_state: None,
            working_dir: None,
            data_receiver: None,
            retry_sender: None,
            rally_event_receiver: None,
            rally_abort_handle: None,
            rally_command_sender: None,
            pending_rally_context: None,
            pending_rally_prompt_loader: None,
            pending_rally_seed_review: None,
            start_ai_rally_on_load: false,
            pending_ai_rally: false,
            mark_viewed_receiver: None,
            spinner_frame: 0,
            jump_stack: Vec::new(),
            pending_keys: SmallVec::new(),
            pending_since: None,
            symbol_popup: None,
            symbol_search: SymbolSearchState::Idle,
            session_cache: SessionCache::new(),
            markdown_rich: false,
            suggestion_highlight_cache: None,
            pr_description_scroll_offset: 0,
            pr_description_cache: None,
            file_list_filter: None,
            batch_diff_receiver: None,
            lazy_diff_receiver: None,
            lazy_diff_pending_file: None,
            chk: ChecksState::default(),
            git_ops_state: None,
            issue_state: None,
            issue_detail_return: false,
            update_available: None,
            update_check_receiver: None,
            tree_mode_active: false,
            file_tree_state: None,
            shell_state: None,
            shell_result_receiver: None,
            shell_abort_handle: None,
            cockpit_state: None,
            home_state: None,
            browse_state: None,
            mouse_capture_active,
            pending_events: std::collections::VecDeque::new(),
            hit_map: crate::ui::hit::HitMap::default(),
            drag_state: input_mouse::DragState::default(),
            theme,
        }
    }

    fn modal_open(&self) -> bool {
        if self.state == AppState::Help {
            return true;
        }
        if matches!(
            self.state,
            AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff
        ) && self.git_ops_state.as_ref().is_some_and(|ops| {
            matches!(
                ops.pending_confirm,
                Some(
                    PendingGitOpsConfirm::Simulating { .. }
                        | PendingGitOpsConfirm::Previewing { .. }
                )
            )
        }) {
            return true;
        }
        if self.state.is_repo_browse() {
            if let Some(state) = &self.browse_state {
                if !matches!(state.overlay, browse::BrowseOverlay::None) {
                    return true;
                }
                if matches!(state.pr_lookup, browse::PrLookupState::Selecting { .. }) {
                    return true;
                }
                if let browse_discussion::LineDiscussionState::Ready { view, .. } =
                    &state.line_discussion
                {
                    if !matches!(view, browse_discussion::DiscussionView::Closed) {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn sole_attention(&self) -> Attention {
        Attention::when(!self.modal_open())
    }

    pub fn split_attention(&self) -> SplitAttention {
        if self.modal_open() {
            return SplitAttention {
                files: Attention::Dim,
                diff: Attention::Dim,
                comments: Attention::Dim,
            };
        }
        match self.state {
            AppState::SplitViewFileList => SplitAttention {
                files: Attention::Focused,
                diff: Attention::Dim,
                comments: Attention::Dim,
            },
            AppState::SplitViewDiff if self.cmt.comment_panel_open => SplitAttention {
                files: Attention::Dim,
                diff: Attention::Dim,
                comments: Attention::Focused,
            },
            AppState::SplitViewDiff => SplitAttention {
                files: Attention::Dim,
                diff: Attention::Focused,
                comments: Attention::Dim,
            },
            _ => SplitAttention {
                files: Attention::Dim,
                diff: Attention::Dim,
                comments: Attention::Dim,
            },
        }
    }

    pub fn diff_attention(&self) -> DiffAttention {
        if self.modal_open() || self.state != AppState::DiffView {
            return DiffAttention {
                diff: Attention::Dim,
                comments: Attention::Dim,
            };
        }
        if self.cmt.comment_panel_open {
            DiffAttention {
                diff: Attention::Dim,
                comments: Attention::Focused,
            }
        } else {
            DiffAttention {
                diff: Attention::Focused,
                comments: Attention::Dim,
            }
        }
    }

    pub fn browse_attention(&self) -> BrowseAttention {
        let dim = BrowseAttention {
            tree: Attention::Dim,
            file: Attention::Dim,
            graph: Attention::Dim,
        };
        if self.modal_open() || !self.state.is_repo_browse() {
            return dim;
        }
        let graph_visible = self
            .browse_state
            .as_ref()
            .is_some_and(|state| state.module_graph_pane.is_visible());
        match self.state {
            AppState::RepoBrowseTree => BrowseAttention {
                tree: Attention::Focused,
                ..dim
            },
            AppState::RepoBrowseGraph if graph_visible => BrowseAttention {
                graph: Attention::Focused,
                ..dim
            },
            AppState::RepoBrowseFile | AppState::RepoBrowseGraph => BrowseAttention {
                file: Attention::Focused,
                ..dim
            },
            _ => dim,
        }
    }

    pub fn git_ops_attention(&self) -> GitOpsAttention {
        let dim = GitOpsAttention {
            tree: Attention::Dim,
            commits: Attention::Dim,
            diff: Attention::Dim,
        };
        if self.modal_open()
            || !matches!(
                self.state,
                AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff
            )
        {
            return dim;
        }
        let left = self
            .git_ops_state
            .as_ref()
            .map(|ops| ops.left_focus)
            .unwrap_or(LeftPaneFocus::Tree);
        let diff_focused = self.state == AppState::GitOpsSplitDiff;
        GitOpsAttention {
            tree: Attention::when(!diff_focused && left == LeftPaneFocus::Tree),
            commits: Attention::when(!diff_focused && left == LeftPaneFocus::Commits),
            diff: Attention::when(diff_focused),
        }
    }

    pub fn issue_detail_attention(&self) -> IssueDetailAttention {
        if self.modal_open() || self.state != AppState::IssueDetail {
            return IssueDetailAttention {
                body: Attention::Dim,
                linked_prs: Attention::Dim,
            };
        }
        let linked = self
            .issue_state
            .as_ref()
            .is_some_and(|state| state.detail_focus == IssueDetailFocus::LinkedPrs);
        IssueDetailAttention {
            body: Attention::when(!linked),
            linked_prs: Attention::when(linked),
        }
    }

    /// F2 トグルで移行すべきキャプチャ状態。mouse.enabled=false なら None(トグル無効)。
    pub(crate) fn mouse_capture_target(&self) -> Option<bool> {
        self.config
            .mouse
            .enabled
            .then_some(!self.mouse_capture_active)
    }

    pub(crate) fn set_mouse_capture_active(&mut self, active: bool) {
        self.mouse_capture_active = active;
    }

    pub fn new_loading(
        repo: &str,
        pr_number: u32,
        config: Config,
        repository_availability: RepositoryAvailability,
    ) -> (Self, mpsc::Sender<DataLoadResult>) {
        let (tx, rx) = mpsc::channel(2);
        let mut app = Self::base_app(repo.to_string(), config);
        // Overrides from base_app defaults
        app.repository_availability = repository_availability;
        app.pr_number = Some(pr_number);
        app.original_pr_number = Some(pr_number);
        app.data_receiver = Some((pr_number, rx));
        app.zen_mode = app.config.layout.zen_mode;
        (app, tx)
    }

    pub fn new_pr_list(
        repo: &str,
        config: Config,
        repository_availability: RepositoryAvailability,
    ) -> Self {
        let zen_mode = config.layout.zen_mode;
        let mut app = Self::base_app(repo.to_string(), config);
        // Overrides from base_app defaults
        app.repository_availability = repository_availability;
        app.pr_number = None;
        app.state = AppState::PullRequestList;
        app.prs.pr_list = LoadState::Loading;
        app.started_from_pr_list = true;
        app.previous_state = AppState::PullRequestList;
        app.zen_mode = zen_mode;
        app
    }

    pub fn new_cockpit(repo: &str, config: Config, repo_available: bool) -> Self {
        let zen_mode = config.layout.zen_mode;
        let mut app = Self::base_app(repo.to_string(), config);
        app.repository_availability = if repo_available {
            RepositoryAvailability::Available
        } else {
            RepositoryAvailability::Unavailable
        };
        app.pr_number = None;
        app.state = AppState::Cockpit;
        app.home_state = Some(AppState::Cockpit);
        app.zen_mode = zen_mode;
        app.cockpit_state = Some(CockpitState::new(repo_available));
        app
    }

    pub fn set_repository_availability(&mut self, availability: RepositoryAvailability) {
        self.repository_availability = availability;
    }

    pub fn repository_availability(&self) -> RepositoryAvailability {
        self.repository_availability
    }

    pub(crate) fn pr_open_notice(&self) -> Option<String> {
        if !self.state.is_pr_review() {
            return None;
        }
        match &self.pr_open_source {
            PrOpenSource::InferredCommitSubject { .. } => {
                self.pr_number.filter(|number| *number > 0).map(|number| {
                    format!("PR #{number} inferred from commit subject; GitHub did not confirm it")
                })
            }
            PrOpenSource::Standard | PrOpenSource::ConfirmedCommit { .. } => None,
        }
    }

    /// PR一覧受信チャンネルを設定
    pub fn set_pr_list_receiver(&mut self, rx: mpsc::Receiver<Result<github::PrListPage, String>>) {
        self.prs.pr_list_receiver = Some(rx);
    }

    /// データ受信チャンネルを設定
    pub fn set_data_receiver(&mut self, pr_number: u32, rx: mpsc::Receiver<DataLoadResult>) {
        self.data_receiver = Some((pr_number, rx));
    }

    pub fn set_retry_sender(&mut self, tx: mpsc::Sender<RefreshRequest>) {
        self.retry_sender = Some(tx);
    }

    /// Set receiver for background update check result
    pub fn set_update_check_receiver(&mut self, rx: mpsc::Receiver<Option<String>>) {
        self.update_check_receiver = Some(rx);
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut terminal = ui::setup_terminal(self.mouse_capture_active)?;

        // データが既にロード済み（キャッシュヒット）の場合、プリフェッチを開始
        if matches!(self.data_state, DataState::Loaded { .. }) {
            self.start_prefetch_all_files();
        }

        // Start AI Rally immediately if flag is set and data is already loaded (from cache)
        if self.start_ai_rally_on_load && matches!(self.data_state, DataState::Loaded { .. }) {
            self.start_ai_rally_on_load = false;
            self.start_ai_rally();
        }

        while !self.should_quit {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
            self.poll_pr_list_updates();
            self.poll_data_updates();
            self.poll_comment_updates();
            self.poll_diff_cache_updates();
            self.poll_prefetch_updates();
            self.poll_batch_diff_updates();
            self.poll_lazy_diff_updates();
            self.poll_discussion_comment_updates();
            self.poll_comment_submit_updates();
            self.poll_mark_viewed_updates();
            self.poll_rally_events();
            self.poll_checks_updates();
            self.poll_ci_status_updates();
            self.poll_git_ops_updates();
            self.poll_issue_list_updates();
            self.poll_issue_detail_updates();
            self.poll_linked_prs_updates();
            self.poll_cockpit_updates();
            self.poll_issue_comment_submit_updates();
            self.poll_update_check();
            self.poll_symbol_search_updates();
            self.poll_shell_result();
            self.poll_browse_updates();
            if let SymbolSearchState::Ready(..) = &self.symbol_search {
                if let Some(result) = self.symbol_search.take_ready() {
                    let full_path = std::path::Path::new(&result.repo_root).join(&result.file_path);
                    let path_str = full_path.to_string_lossy().to_string();
                    let line = result.line_number;
                    let editor = self.config.editor.clone();
                    ui::restore_terminal(&mut terminal)?;
                    let _ = crate::editor::open_file_at_line(editor.as_deref(), &path_str, line);
                    terminal = ui::setup_terminal(self.mouse_capture_active)?;
                }
            }
            terminal.draw(|frame| ui::render(frame, self))?;
            self.handle_input(&mut terminal).await?;
        }

        // Graceful shutdown: abort any running rally
        if let Some(handle) = self.rally_abort_handle.take() {
            handle.abort();
        }

        ui::restore_terminal(&mut terminal)?;
        Ok(())
    }

    /// Get the current spinner character for loading animations
    pub fn spinner_char(&self) -> &str {
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }

    pub fn set_working_dir(&mut self, dir: Option<String>) {
        self.working_dir = dir;
    }

    pub fn set_local_mode(&mut self, local: bool) {
        self.local_mode = local;
    }

    pub fn set_local_auto_focus(&mut self, enable: bool) {
        self.local_auto_focus = enable;
    }

    pub fn is_local_mode(&self) -> bool {
        self.local_mode
    }

    pub fn is_local_auto_focus(&self) -> bool {
        self.local_auto_focus
    }

    pub fn is_markdown_rich(&self) -> bool {
        self.markdown_rich
    }

    pub(crate) fn toggle_zen_mode(&mut self) {
        self.zen_mode = !self.zen_mode;
        let msg = if self.zen_mode {
            "Zen mode: ON"
        } else {
            "Zen mode: OFF"
        };
        self.cmt.submission_result = Some((true, msg.to_string()));
        self.cmt.submission_result_time = Some(Instant::now());
    }

    pub(crate) fn enter_diff_from_file_list(&mut self) {
        if self.state == AppState::SplitViewFileList {
            // Preserve split-view context regardless of zen mode
            self.state = AppState::SplitViewDiff;
        } else if self.zen_mode {
            self.diff_view_return_state = AppState::FileList;
            self.state = AppState::DiffView;
        } else {
            self.state = AppState::SplitViewDiff;
        }
    }

    /// Set flag to start AI Rally when data is loaded (used by --ai-rally CLI flag)
    pub fn set_start_ai_rally_on_load(&mut self, start: bool) {
        self.start_ai_rally_on_load = start;
    }

    /// Set pending AI Rally flag (for PR list mode with --ai-rally)
    pub fn set_pending_ai_rally(&mut self, pending: bool) {
        self.pending_ai_rally = pending;
    }

    /// PR番号を取得（未設定の場合はpanic）
    /// PR一覧から選択後は必ず設定されている前提
    pub fn pr_number(&self) -> u32 {
        self.pr_number
            .expect("pr_number should be set before accessing PR data")
    }

    /// コメント送信中かどうか
    pub fn is_submitting_comment(&self) -> bool {
        self.cmt.comment_submitting
    }

    /// Approve confirmation prompt is active.
    pub fn is_pending_approve_confirmation(&self) -> bool {
        self.cmt.pending_approve_body.is_some()
    }

    /// Build dynamic footer text for approve confirmation prompt.
    pub fn approve_confirmation_footer_text(&self) -> String {
        let kb = &self.config.keybindings;
        format!(
            "{}: confirm approve | {}/Esc: cancel",
            kb.approve.display(),
            kb.quit.display(),
        )
    }

    pub fn new_for_test() -> Self {
        Self::base_app("test/repo".to_string(), Config::default())
    }

    /// ファイルツリーモードが有効で表示すべきかを判定
    pub fn is_file_tree_active(&self) -> bool {
        self.tree_mode_active && self.file_tree_state.is_some() && self.file_list_filter.is_none()
    }

    /// Set the comment_submitting flag for testing.
    #[cfg(test)]
    pub fn set_submitting_for_test(&mut self, submitting: bool) {
        self.cmt.comment_submitting = submitting;
    }

    #[cfg(test)]
    pub fn set_pending_approve_body_for_test(&mut self, body: Option<String>) {
        self.cmt.pending_approve_body = body;
    }
}
