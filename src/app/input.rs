use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::filter::ListFilter;
use crate::github::{self, ChangedFile};
use crate::keybinding::{event_to_keybinding, SequenceMatch};

use super::types::*;
use super::{App, AppState, DataState};

impl App {
    pub(crate) async fn handle_input(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                // Kitty keyboard protocol が有効な場合、Release/Repeat イベントも
                // 報告されるため、Press のみ処理して二重実行を防止する。
                if key.kind != KeyEventKind::Press {
                    return Ok(());
                }

                // PR一覧画面は独自のLoading処理があるためスキップ
                // Help画面・PrDescription画面はデータ状態に依存しないためスキップ
                if !self.state.is_data_state_independent() {
                    // Error状態でのリトライ処理
                    if let DataState::Error(_) = &self.data_state {
                        match key.code {
                            KeyCode::Char('q') => self.should_quit = true,
                            KeyCode::Char('r') => self.retry_load(),
                            _ => {}
                        }
                        return Ok(());
                    }

                    // Loading状態ではqのみ受け付け
                    if matches!(self.data_state, DataState::Loading) {
                        if key.code == KeyCode::Char('q') {
                            self.should_quit = true;
                        }
                        return Ok(());
                    }

                    if self.cmt.pending_approve_body.is_some() {
                        match self.handle_pending_approve_choice(&key) {
                            PendingApproveChoice::Submit => {
                                let body = self.cmt.pending_approve_body.take().unwrap_or_default();
                                self.submit_review_with_body(ReviewAction::Approve, &body)
                                    .await?;
                            }
                            PendingApproveChoice::Cancel | PendingApproveChoice::Ignore => {}
                        }
                        return Ok(());
                    }
                }

                match self.state {
                    AppState::PullRequestList => self.handle_pr_list_input(key).await?,
                    AppState::FileList => self.handle_file_list_input(key, terminal).await?,
                    AppState::DiffView => self.handle_diff_view_input(key, terminal).await?,
                    AppState::TextInput => self.handle_text_input(key)?,
                    AppState::CommentList => self.handle_comment_list_input(key, terminal).await?,
                    AppState::Help => self.handle_help_input(key, terminal)?,
                    AppState::AiRally => self.handle_ai_rally_input(key, terminal).await?,
                    AppState::SplitViewFileList => {
                        self.handle_split_view_file_list_input(key, terminal)
                            .await?
                    }
                    AppState::SplitViewDiff => {
                        self.handle_split_view_diff_input(key, terminal).await?
                    }
                    AppState::PrDescription => self.handle_pr_description_input(key, terminal)?,
                    AppState::ChecksList => self.handle_checks_list_input(key)?,
AppState::IssueList => self.handle_issue_list_input(key).await?,
                    AppState::IssueDetail => self.handle_issue_detail_input(key, terminal)?,
                    AppState::IssueCommentList => self.handle_issue_comment_list_input(key)?,
                    AppState::GitOpsSplitTree => {
                        let focus = self
                            .git_ops_state
                            .as_ref()
                            .map(|ops| ops.left_focus)
                            .unwrap_or(LeftPaneFocus::Tree);
                        match focus {
                            LeftPaneFocus::Tree => {
                                self.handle_git_ops_tree_input(key, terminal);
                            }
                            LeftPaneFocus::Commits => {
                                self.handle_git_ops_commits_input(key);
                            }
                        }
                    }
                    AppState::GitOpsSplitDiff => {
                        self.handle_git_ops_diff_input(key);
                    }
                }
            }
        }
        Ok(())
    }
    pub(crate) fn retry_load(&mut self) {
        if let Some(ref tx) = self.retry_sender {
            // 既にデータがある場合は Loading に戻さない（バックグラウンド更新のみ）
            if !matches!(self.data_state, DataState::Loaded { .. }) {
                self.data_state = DataState::Loading;
            }
            let request = if self.local_mode {
                RefreshRequest::LocalRefresh
            } else {
                RefreshRequest::PrRefresh {
                    pr_number: self.pr_number.unwrap_or(0),
                }
            };
            let _ = tx.try_send(request);
        }
    }
    pub(crate) async fn handle_file_list_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        if self.handle_filter_input(&key, "file") {
            return Ok(());
        }

        // フィルタ結果が空の場合、ファイル操作を無効化（stale selection 防止）
        // ツリーモードで Dir 行にいる場合、v (mark_viewed) を無効化
        if !self.is_filter_selection_empty("file") {
            if self.is_file_tree_active() && self.is_file_tree_on_dir_row() {
                // Dir 行: V (mark_viewed_dir) はツリーパスベースで処理
                let kb = &self.config.keybindings;
                if !self.local_mode && self.matches_single_key(&key, &kb.mark_viewed_dir) {
                    self.start_mark_tree_directory_as_viewed();
                    return Ok(());
                }
                // Dir 行: v (mark_viewed) は無効
                if self.matches_single_key(&key, &kb.mark_viewed) {
                    return Ok(());
                }
            } else if self.handle_mark_viewed_key(key) {
                return Ok(());
            }
        }

        let kb = self.config.keybindings.clone();
        let has_filter = self.file_list_filter.is_some();
        let tree_active = self.is_file_tree_active();

        if self.matches_single_key(&key, &kb.tree_toggle) && !has_filter {
            self.toggle_file_tree();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            if self.handle_filter_esc("file") {
                return Ok(());
            }
            if self.started_from_pr_list {
                self.back_to_pr_list();
            } else {
                self.should_quit = true;
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_down) {
            if has_filter {
                self.handle_filter_navigation("file", true);
            } else if tree_active {
                self.file_tree_move_down();
            } else if !self.files().is_empty() {
                self.selected_file =
                    (self.selected_file + 1).min(self.files().len().saturating_sub(1));
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_up) {
            if has_filter {
                self.handle_filter_navigation("file", false);
            } else if tree_active {
                self.file_tree_move_up();
            } else {
                self.selected_file = self.selected_file.saturating_sub(1);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_down) || Self::is_shift_char_shortcut(&key, 'j') {
            if !has_filter {
                let page_step = terminal.size()?.height.saturating_sub(8) as usize;
                let step = page_step.max(1);
                if tree_active {
                    self.file_tree_page_down(step);
                } else if !self.files().is_empty() {
                    self.selected_file =
                        (self.selected_file + step).min(self.files().len().saturating_sub(1));
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_up) || Self::is_shift_char_shortcut(&key, 'k') {
            if !has_filter {
                let page_step = terminal.size()?.height.saturating_sub(8) as usize;
                let step = page_step.max(1);
                if tree_active {
                    self.file_tree_page_up(step);
                } else {
                    self.selected_file = self.selected_file.saturating_sub(step);
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.git_ops) {
            self.open_git_ops();
            return Ok(());
        }

        if let Some(kb_event) = event_to_keybinding(&key) {
            self.check_sequence_timeout();

            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);

                if self.try_match_sequence(&kb.filter) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    if let Some(ref mut filter) = self.file_list_filter {
                        filter.input_active = true;
                    } else {
                        let mut filter = ListFilter::new();
                        let files = self.files();
                        filter.apply(files, |_file, _q| true);
                        if let Some(idx) = filter.sync_selection() {
                            self.selected_file = idx;
                        }
                        self.file_list_filter = Some(filter);
                    }
                    return Ok(());
                }

                // gg: Jump to first (tree/flat 共通)
                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    if tree_active {
                        self.file_tree_jump_to_first();
                    } else {
                        self.selected_file = 0;
                    }
                    return Ok(());
                }

                self.clear_pending_keys();
            } else {
                let could_start_filter = self.key_could_match_sequence(&key, &kb.filter);
                let could_start_gg = self.key_could_match_sequence(&key, &kb.jump_to_first);
                if could_start_filter || could_start_gg {
                    self.push_pending_key(kb_event);
                    return Ok(());
                }
            }
        }

        // G: Jump to last (tree/flat 共通)
        if self.matches_single_key(&key, &kb.jump_to_last) {
            if tree_active {
                self.file_tree_jump_to_last();
            } else if !self.files().is_empty() {
                self.selected_file = self.files().len().saturating_sub(1);
            }
            return Ok(());
        }

        // Open split view (Enter, Right arrow, or l)
        if self.matches_single_key(&key, &kb.open_panel)
            || self.matches_single_key(&key, &kb.move_right)
                   {
            if self.is_filter_selection_empty("file") {
                return Ok(());
            }
            // ツリーモード: Dir 行なら展開トグル、File 行なら diff 遷移
            if tree_active && self.file_tree_enter() {
                return Ok(());
            }
            if !self.files().is_empty() {
                self.enter_diff_from_file_list();
                self.sync_diff_to_selected_file();
            }
            return Ok(());
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.approve) {
            self.submit_review(ReviewAction::Approve, terminal).await?;
            return Ok(());
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.request_changes) {
            self.submit_review(ReviewAction::RequestChanges, terminal)
                .await?;
            return Ok(());
        }

        // Using separate check for review comment in FileList context
        if !self.local_mode && self.matches_single_key(&key, &kb.comment) {
            self.submit_review(ReviewAction::Comment, terminal).await?;
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.comment_list) {
            self.previous_state = AppState::FileList;
            self.open_comment_list();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.refresh) {
            self.refresh_all();
            return Ok(());
        }

        // ローカルモードではコメント投稿等のAPI呼び出しはオーケストレーター側でスキップされる。
        if self.matches_single_key(&key, &kb.ai_rally) {
            self.resume_or_start_ai_rally();
            return Ok(());
        }

        // Open in browser (disabled in local mode)
        if !self.local_mode && self.matches_single_key(&key, &kb.open_in_browser) {
            if let Some(pr_number) = self.pr_number {
                self.open_pr_in_browser(pr_number);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.toggle_local_mode) {
            self.toggle_local_mode();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.toggle_auto_focus) {
            if self.local_mode {
                self.toggle_auto_focus();
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.toggle_zen_mode) {
            self.toggle_zen_mode();
            return Ok(());
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.pr_description) {
            self.open_pr_description();
            return Ok(());
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.ci_checks) {
            if let Some(pr_number) = self.pr_number {
                self.open_checks_list(pr_number);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.help) {
            self.previous_state = AppState::FileList;
            self.state = AppState::Help;
            self.help_scroll_offset = 0;
            self.config_scroll_offset = 0;
            return Ok(());
        }

        Ok(())
    }
    pub(crate) async fn handle_common_file_list_keys(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<bool> {
        // フィルタ結果が空の場合、ファイル操作を無効化（stale selection 防止）
        if !self.is_filter_selection_empty("file") && self.handle_mark_viewed_key(key) {
            return Ok(true);
        }

        let kb = &self.config.keybindings;

        if !self.local_mode && self.matches_single_key(&key, &kb.approve) {
            self.submit_review(ReviewAction::Approve, terminal).await?;
            return Ok(true);
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.request_changes) {
            self.submit_review(ReviewAction::RequestChanges, terminal)
                .await?;
            return Ok(true);
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.comment) {
            self.submit_review(ReviewAction::Comment, terminal).await?;
            return Ok(true);
        }

        if self.matches_single_key(&key, &kb.refresh) {
            self.refresh_all();
            return Ok(true);
        }

        if self.matches_single_key(&key, &kb.ai_rally) {
            self.resume_or_start_ai_rally();
            return Ok(true);
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.open_in_browser) {
            if let Some(pr_number) = self.pr_number {
                self.open_pr_in_browser(pr_number);
            }
            return Ok(true);
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.pr_description) {
            self.open_pr_description();
            return Ok(true);
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.ci_checks) {
            if let Some(pr_number) = self.pr_number {
                self.open_checks_list(pr_number);
            }
            return Ok(true);
        }

        if self.matches_single_key(&key, &kb.toggle_local_mode) {
            self.toggle_local_mode();
            return Ok(true);
        }

        if self.matches_single_key(&key, &kb.toggle_auto_focus) {
            if self.local_mode {
                self.toggle_auto_focus();
            }
            return Ok(true);
        }

        if self.matches_single_key(&key, &kb.toggle_zen_mode) {
            self.toggle_zen_mode();
            return Ok(true);
        }

        Ok(false)
    }
    pub(crate) fn handle_mark_viewed_key(&mut self, key: event::KeyEvent) -> bool {
        if self.local_mode {
            return false;
        }

        let kb = &self.config.keybindings;
        let is_mark_file = self.matches_single_key(&key, &kb.mark_viewed);
        let is_mark_directory = self.matches_single_key(&key, &kb.mark_viewed_dir);

        if !is_mark_file && !is_mark_directory {
            return false;
        }

        if self.mark_viewed_receiver.is_some() {
            self.cmt.submission_result = Some((false, "Mark viewed already in progress".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return true;
        }

        if is_mark_file {
            self.start_mark_selected_file_as_viewed();
            return true;
        }

        self.start_mark_selected_directory_as_viewed();
        true
    }

    pub(crate) fn start_mark_selected_file_as_viewed(&mut self) {
        let Some(file) = self.files().get(self.selected_file) else {
            return;
        };
        let set_viewed = !file.viewed;

        self.start_mark_paths_as_viewed(vec![file.filename.clone()], set_viewed);
    }

    pub(crate) fn start_mark_selected_directory_as_viewed(&mut self) {
        let target_paths = Self::collect_unviewed_directory_paths(self.files(), self.selected_file);

        if target_paths.is_empty() {
            self.cmt.submission_result = Some((true, "No unviewed files in directory".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        }

        self.start_mark_paths_as_viewed(target_paths, true);
    }

    pub(crate) fn start_mark_paths_as_viewed(&mut self, paths: Vec<String>, set_viewed: bool) {
        let total_targets = paths.len();
        if total_targets == 0 {
            return;
        }

        let Some(pr_number) = self.pr_number else {
            self.cmt.submission_result = Some((false, "PR number not set".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        };
        let Some(pr) = self.pr() else {
            self.cmt.submission_result = Some((false, "PR metadata not loaded".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        };
        let Some(pr_node_id) = pr.node_id.clone() else {
            self.cmt.submission_result = Some((false, "PR node ID is unavailable".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        };

        let repo = self.repo.clone();
        let (tx, rx) = mpsc::channel(1);
        self.mark_viewed_receiver = Some((pr_number, rx));
        let action_label = if set_viewed { "viewed" } else { "unviewed" };
        self.cmt.submission_result = Some((
            true,
            format!("Marking {} file(s) as {}...", total_targets, action_label),
        ));
        self.cmt.submission_result_time = Some(Instant::now());

        tokio::spawn(async move {
            let mut marked_paths = Vec::with_capacity(total_targets);
            let mut error = None;

            for path in paths {
                let result =
                    github::set_file_viewed(&repo, &pr_node_id, &path, set_viewed).await;
                match result {
                    Ok(()) => marked_paths.push(path),
                    Err(e) => {
                        error = Some(format!("{}: {}", path, e));
                        break;
                    }
                }
            }

            let _ = tx
                .send(MarkViewedResult::Completed {
                    marked_paths,
                    total_targets,
                    error,
                    set_viewed,
                })
                .await;
        });
    }

    pub(crate) fn directory_prefix_for(path: &str) -> String {
        path.rsplit_once('/')
            .map(|(dir, _)| format!("{}/", dir))
            .unwrap_or_default()
    }

    pub(crate) fn collect_unviewed_directory_paths(
        files: &[ChangedFile],
        selected_file: usize,
    ) -> Vec<String> {
        let Some(selected) = files.get(selected_file) else {
            return Vec::new();
        };
        let directory_prefix = Self::directory_prefix_for(&selected.filename);

        files
            .iter()
            .filter(|file| {
                let in_scope = if directory_prefix.is_empty() {
                    !file.filename.contains('/')
                } else {
                    file.filename.starts_with(&directory_prefix)
                };
                in_scope && !file.viewed
            })
            .map(|file| file.filename.clone())
            .collect()
    }
    pub(crate) fn refresh_all(&mut self) {
        self.session_cache.invalidate_all();
        self.cmt.review_comments = None;
        self.cmt.discussion_comments = None;
        self.cmt.comments_loading = false;
        self.cmt.discussion_comments_loading = false;
        self.file_list_filter = None;
        self.data_state = DataState::Loading;
        self.retry_load();
    }

    pub(crate) fn open_pr_in_browser(&self, pr_number: u32) {
        let repo = self.repo.clone();
        tokio::spawn(async move {
            let _ =
                github::gh_command(&["pr", "view", &pr_number.to_string(), "-R", &repo, "--web"])
                    .await;
        });
    }

    pub(crate) fn open_checks_list(&mut self, pr_number: u32) {
        if self.state != AppState::ChecksList {
            self.chk.checks_return_state = self.state;
        }
        self.state = AppState::ChecksList;
        self.chk.selected_check = 0;
        self.chk.checks_scroll_offset = 0;
        self.chk.checks_loading = true;
        self.chk.checks = None;
        self.chk.checks_target_pr = Some(pr_number);

        let (tx, rx) = mpsc::channel(1);
        self.chk.checks_receiver = Some((pr_number, rx));
        let repo = self.repo.clone();
        tokio::spawn(async move {
            let result = github::fetch_pr_checks(&repo, pr_number)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(result).await;
        });
    }

    pub(crate) fn handle_checks_list_input(&mut self, key: event::KeyEvent) -> Result<()> {
        let kb = self.config.keybindings.clone();
        let check_count = self.chk.checks.as_ref().map(|c| c.len()).unwrap_or(0);

        if self.matches_single_key(&key, &kb.quit) {
            self.state = self.chk.checks_return_state;
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_down) {
            if check_count > 0 {
                self.chk.selected_check = (self.chk.selected_check + 1).min(check_count.saturating_sub(1));
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_up) {
            self.chk.selected_check = self.chk.selected_check.saturating_sub(1);
            return Ok(());
        }

        if let Some(kb_event) = event_to_keybinding(&key) {
            self.check_sequence_timeout();

            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);
                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    self.chk.selected_check = 0;
                    return Ok(());
                }
                self.clear_pending_keys();
            } else if self.key_could_match_sequence(&key, &kb.jump_to_first) {
                self.push_pending_key(kb_event);
                return Ok(());
            }
        }

        if self.matches_single_key(&key, &kb.jump_to_last) {
            if check_count > 0 {
                self.chk.selected_check = check_count.saturating_sub(1);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.open_panel) {
            if let Some(ref checks) = self.chk.checks {
                if let Some(check) = checks.get(self.chk.selected_check) {
                    if let Some(ref url) = check.link {
                        Self::open_url_in_browser(url);
                    }
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.refresh) {
            if let Some(pr_number) = self.chk.checks_target_pr {
                self.open_checks_list(pr_number);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.open_in_browser) {
            if let Some(pr_number) = self.chk.checks_target_pr {
                self.open_pr_in_browser(pr_number);
            }
            return Ok(());
        }

        // ?: help
        if self.matches_single_key(&key, &kb.help) {
            self.previous_state = AppState::ChecksList;
            self.state = AppState::Help;
            self.help_scroll_offset = 0;
            self.config_scroll_offset = 0;
            return Ok(());
        }

        Ok(())
    }

    pub(crate) fn open_url_in_browser(url: &str) {
        let url = url.to_string();
        tokio::spawn(async move {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let _ = tokio::process::Command::new(opener)
                .arg(&url)
                .output()
                .await;
        });
    }

    // ============================================================
    // FileTree integration methods
    // ============================================================

    /// ファイル一覧のツリー表示をトグル。
    /// ON: FileTreeState を生成（初回）または再利用し、rebuild + カーソル復元。
    /// OFF: tree_mode_active を false にするが file_tree_state は保持（展開状態を失わない）。
    pub(crate) fn toggle_file_tree(&mut self) {
        if self.tree_mode_active {
            // OFF にする
            self.tree_mode_active = false;
            return;
        }

        // ON にする
        // self.files() を先に借りてパスを収集し、借用を解放してから tree を操作
        let paths: Vec<(usize, String)> = self
            .files()
            .iter()
            .enumerate()
            .map(|(i, f)| (i, f.filename.clone()))
            .collect();

        if paths.is_empty() {
            return;
        }

        self.tree_mode_active = true;
        let selected = self.selected_file;

        if let Some(ref mut tree) = self.file_tree_state {
            // 既存の tree_state を再利用（展開状態維持）
            tree.rebuild_owned(paths);
            if let Some(row) = tree.find_row_for_file(selected) {
                tree.selected_row = row;
            }
        } else {
            let mut tree = super::file_tree::FileTreeState::new();
            tree.rebuild_owned(paths);
            if let Some(row) = tree.find_row_for_file(selected) {
                tree.selected_row = row;
            }
            self.file_tree_state = Some(tree);
        }
    }

    fn with_file_tree(&mut self, f: impl FnOnce(&mut super::file_tree::FileTreeState)) {
        if let Some(ref mut tree) = self.file_tree_state {
            f(tree);
            if let Some(idx) = tree.selected_file_index() {
                self.selected_file = idx;
            }
        }
    }

    pub(crate) fn file_tree_move_down(&mut self) {
        self.with_file_tree(|t| t.move_down());
    }

    pub(crate) fn file_tree_move_up(&mut self) {
        self.with_file_tree(|t| t.move_up());
    }

    pub(crate) fn file_tree_page_down(&mut self, step: usize) {
        self.with_file_tree(|t| t.page_down(step));
    }

    pub(crate) fn file_tree_page_up(&mut self, step: usize) {
        self.with_file_tree(|t| t.page_up(step));
    }

    pub(crate) fn file_tree_jump_to_first(&mut self) {
        self.with_file_tree(|t| t.jump_to_first());
    }

    pub(crate) fn file_tree_jump_to_last(&mut self) {
        self.with_file_tree(|t| t.jump_to_last());
    }

    /// ツリーモードで Enter:
    /// - Dir 行: 展開/折畳トグル → true を返す
    /// - File 行: 何もしない → false を返す（呼び出し元が diff 遷移を行う）
    pub(crate) fn file_tree_enter(&mut self) -> bool {
        if let Some(ref mut tree) = self.file_tree_state {
            if tree.selected_dir_path().is_some() {
                tree.toggle_expand();
                return true;
            }
        }
        false
    }

    /// ツリーモードで現在 Dir 行にいるかどうか。
    pub(crate) fn is_file_tree_on_dir_row(&self) -> bool {
        if let Some(ref tree) = self.file_tree_state {
            tree.selected_dir_path().is_some()
        } else {
            false
        }
    }

    /// ツリーモードがアクティブなら、現在のファイル一覧でツリーを再構築する。
    /// データリロード後やフィルタ解除後に呼び出す。
    pub(crate) fn rebuild_file_tree_if_active(&mut self) {
        if !self.tree_mode_active || self.file_tree_state.is_none() {
            return;
        }
        // フィルタ中はスキップ（フラット表示を維持）
        if self.file_list_filter.is_some() {
            return;
        }

        // self.files() を先に借りてパスを収集し、借用を解放してから tree を操作
        let paths: Vec<(usize, String)> = self
            .files()
            .iter()
            .enumerate()
            .map(|(i, f)| (i, f.filename.clone()))
            .collect();

        if paths.is_empty() {
            return;
        }

        let tree = self.file_tree_state.as_mut().unwrap();

        // 選択復元用に現在の情報を保存
        let prev_file_idx = tree.selected_file_index();
        let prev_dir_path = tree.selected_dir_path().map(|s| s.to_string());

        tree.rebuild_owned(paths);

        // 選択復元
        if let Some(idx) = prev_file_idx {
            if let Some(row) = tree.find_row_for_file(idx) {
                tree.selected_row = row;
                return;
            }
        }
        if let Some(ref dir_path) = prev_dir_path {
            if let Some(row) = tree.find_row_for_dir(dir_path) {
                tree.selected_row = row;
                return;
            }
        }
        // フォールバック: 先頭
        tree.selected_row = 0;
    }

    /// ツリーモードで Dir 行の V: 配下全ファイルを mark viewed。
    pub(crate) fn start_mark_tree_directory_as_viewed(&mut self) {
        let dir_path = self
            .file_tree_state
            .as_ref()
            .and_then(|tree| tree.selected_dir_path())
            .map(|s| s.to_string());

        let Some(dir_path) = dir_path else {
            return;
        };

        let target_paths = Self::collect_unviewed_paths_under_dir(self.files(), &dir_path);

        if target_paths.is_empty() {
            self.cmt.submission_result = Some((true, "No unviewed files in directory".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        }

        self.start_mark_paths_as_viewed(target_paths, true);
    }

    /// ツリーモード用の mark_viewed_dir: ディレクトリパスプレフィックスで未読ファイルを収集。
    pub(crate) fn collect_unviewed_paths_under_dir(
        files: &[ChangedFile],
        dir_path: &str,
    ) -> Vec<String> {
        if dir_path.is_empty() {
            return Vec::new();
        }
        let prefix = format!("{}/", dir_path);
        files
            .iter()
            .filter(|file| file.filename.starts_with(&prefix) && !file.viewed)
            .map(|file| file.filename.clone())
            .collect()
    }
}
