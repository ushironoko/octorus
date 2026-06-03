use anyhow::Result;
use crossterm::event;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::collections::HashMap;
use std::io::Stdout;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::cache::{load_local_review_comments, LocalCommentMeta, LocalReviewComment, PrCacheKey};
use crate::github::{self, comment::ReviewComment};
use crate::keybinding::{event_to_keybinding, SequenceMatch};
use crate::ui;

use super::types::*;
use super::{App, AppState, CommentTab};

pub(crate) fn split_local_comments(
    entries: Vec<LocalReviewComment>,
) -> (Vec<ReviewComment>, HashMap<u64, LocalCommentMeta>) {
    let mut comments = Vec::with_capacity(entries.len());
    let mut meta = HashMap::with_capacity(entries.len());
    for entry in entries {
        let id = entry.comment.id;
        comments.push(entry.comment);
        if entry.meta.is_resolved || entry.meta.resolved_at.is_some() {
            meta.insert(id, entry.meta);
        }
    }
    (comments, meta)
}

impl App {
    pub(crate) fn enter_comment_input(&mut self) {
        let Some(file) = self.files().get(self.selected_file) else {
            return;
        };
        let Some(patch) = file.patch.as_ref() else {
            return;
        };

        // Get actual line number from diff
        let Some(line_info) = crate::diff::get_line_info(patch, self.diff_scroll.selected_line)
        else {
            return;
        };

        // Only allow comments on Added or Context lines (not Removed/Header/Meta)
        if !matches!(
            line_info.line_type,
            crate::diff::LineType::Added | crate::diff::LineType::Context
        ) {
            return;
        }

        let Some(line_number) = line_info.new_line_number else {
            return;
        };

        let Some(diff_position) = line_info.diff_position else {
            return;
        };

        self.input_mode = Some(InputMode::Comment(LineInputContext {
            file_index: self.selected_file,
            line_number,
            diff_position,
            start_line_number: None,
        }));
        self.input_text_area.clear();
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }
    pub(crate) async fn submit_review(
        &mut self,
        action: ReviewAction,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        tracing::debug!(?action, "submit_review: start");
        ui::restore_terminal(terminal)?;

        let editor_result = crate::editor::open_review_editor(self.config.editor.as_deref());
        tracing::debug!(?editor_result, "submit_review: editor returned");

        // エディタの成否に関わらずターミナルを再セットアップ
        *terminal = ui::setup_terminal()?;

        let body = match editor_result {
            Ok(body) => body,
            Err(e) => {
                tracing::debug!(%e, "submit_review: editor failed");
                self.cmt.submission_result = Some((false, format!("Editor failed: {}", e)));
                self.cmt.submission_result_time = Some(Instant::now());
                return Ok(());
            }
        };

        let Some(body) = body else {
            tracing::debug!("submit_review: body is None");
            if action == ReviewAction::Approve {
                // Empty comment → show approve confirmation UI
                self.cmt.pending_approve_body = Some(String::new());
            } else {
                self.cmt.submission_result = Some((false, "Review cancelled".to_string()));
                self.cmt.submission_result_time = Some(Instant::now());
            }
            return Ok(());
        };

        // All actions with non-empty body: submit immediately
        self.submit_review_with_body(action, &body).await
    }

    pub(crate) async fn submit_review_with_body(
        &mut self,
        action: ReviewAction,
        body: &str,
    ) -> Result<()> {
        tracing::debug!(body_len = body.len(), "submit_review: calling GitHub API");
        match github::submit_review(&self.repo, self.pr_number(), action, body).await {
            Ok(()) => {
                let action_str = match action {
                    ReviewAction::Approve => "approved",
                    ReviewAction::RequestChanges => "changes requested",
                    ReviewAction::Comment => "commented",
                };
                tracing::debug!(action_str, "submit_review: success");
                self.cmt.submission_result =
                    Some((true, format!("Review submitted ({})", action_str)));
                self.cmt.submission_result_time = Some(Instant::now());
            }
            Err(e) => {
                tracing::debug!(%e, "submit_review: API failed");
                self.cmt.submission_result = Some((false, format!("Review failed: {}", e)));
                self.cmt.submission_result_time = Some(Instant::now());
            }
        }
        self.cmt.pending_approve_body = None;
        Ok(())
    }
    pub(crate) fn enter_suggestion_input(&mut self) {
        let Some(file) = self.files().get(self.selected_file) else {
            return;
        };
        let Some(patch) = file.patch.as_ref() else {
            return;
        };

        // Check if this line can have a suggestion
        let Some(line_info) = crate::diff::get_line_info(patch, self.diff_scroll.selected_line)
        else {
            return;
        };

        // Only allow suggestions on Added or Context lines
        if !matches!(
            line_info.line_type,
            crate::diff::LineType::Added | crate::diff::LineType::Context
        ) {
            return;
        }

        let Some(line_number) = line_info.new_line_number else {
            return;
        };

        let Some(diff_position) = line_info.diff_position else {
            return;
        };

        let original_code = line_info.line_content.clone();

        self.input_mode = Some(InputMode::Suggestion {
            context: LineInputContext {
                file_index: self.selected_file,
                line_number,
                diff_position,
                start_line_number: None,
            },
            original_code: original_code.clone(),
        });
        // サジェスチョンは元コードを初期値として設定
        self.input_text_area.set_content(&original_code);
        self.update_suggestion_highlight_cache();
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }
    /// 複数行選択モードを開始する（Shift+Enter）
    pub(crate) fn enter_multiline_selection(&mut self) {
        // 現在の行がコメント可能な行であることを確認
        let Some(file) = self.files().get(self.selected_file) else {
            return;
        };
        let Some(patch) = file.patch.as_ref() else {
            return;
        };
        let Some(line_info) = crate::diff::get_line_info(patch, self.diff_scroll.selected_line)
        else {
            return;
        };
        if !matches!(
            line_info.line_type,
            crate::diff::LineType::Added | crate::diff::LineType::Context
        ) {
            return;
        }
        self.multiline_selection = Some(MultilineSelection {
            anchor_line: self.diff_scroll.selected_line,
            cursor_line: self.diff_scroll.selected_line,
        });
    }
    pub(crate) fn enter_multiline_comment_input(&mut self) {
        let Some(ref selection) = self.multiline_selection else {
            return;
        };
        let start = selection.start();
        let end = selection.end();

        let Some(file) = self.files().get(self.selected_file) else {
            return;
        };
        let Some(patch) = file.patch.as_ref() else {
            return;
        };

        // 範囲内の全行が同一ハンク内の new-side 行であることを検証
        if !crate::diff::validate_multiline_range(patch, start, end) {
            return;
        }
        let index = crate::diff::PatchIndex::build(patch);

        // 終了行の情報を取得（GitHub API の line パラメータ）
        let Some(end_info) = index.get(end) else {
            return;
        };
        if !matches!(
            end_info.line_type,
            crate::diff::LineType::Added | crate::diff::LineType::Context
        ) {
            return;
        }
        let Some(end_line_number) = end_info.new_line_number else {
            return;
        };
        let Some(diff_position) = end_info.diff_position else {
            return;
        };

        // 開始行の情報を取得（GitHub API の start_line パラメータ）
        let Some(start_info) = index.get(start) else {
            return;
        };
        let Some(start_line_number) = start_info.new_line_number else {
            return;
        };

        // 単一行の場合は start_line_number を None にする
        let start_line = if start_line_number < end_line_number {
            Some(start_line_number)
        } else {
            None
        };

        // バリデーション成功後にのみ選択状態をクリア
        self.multiline_selection = None;

        self.input_mode = Some(InputMode::Comment(LineInputContext {
            file_index: self.selected_file,
            line_number: end_line_number,
            diff_position,
            start_line_number: start_line,
        }));
        self.input_text_area.clear();
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }
    pub(crate) fn enter_multiline_suggestion_input(&mut self) {
        let Some(ref selection) = self.multiline_selection else {
            return;
        };
        let start = selection.start();
        let end = selection.end();

        let Some(file) = self.files().get(self.selected_file) else {
            return;
        };
        let Some(patch) = file.patch.as_ref() else {
            return;
        };

        // 範囲内の全行が同一ハンク内の new-side 行であることを検証
        if !crate::diff::validate_multiline_range(patch, start, end) {
            return;
        }
        let index = crate::diff::PatchIndex::build(patch);

        // 終了行の情報を取得
        let Some(end_info) = index.get(end) else {
            return;
        };
        if !matches!(
            end_info.line_type,
            crate::diff::LineType::Added | crate::diff::LineType::Context
        ) {
            return;
        }
        let Some(end_line_number) = end_info.new_line_number else {
            return;
        };
        let Some(diff_position) = end_info.diff_position else {
            return;
        };

        // 開始行の情報を取得
        let Some(start_info) = index.get(start) else {
            return;
        };
        let Some(start_line_number) = start_info.new_line_number else {
            return;
        };
        let mut original_lines = Vec::new();
        for line_idx in start..=end {
            if let Some(info) = index.get(line_idx) {
                if matches!(
                    info.line_type,
                    crate::diff::LineType::Added | crate::diff::LineType::Context
                ) {
                    original_lines.push(info.content);
                }
            }
        }
        let original_code = original_lines.join("\n");

        let start_line = if start_line_number < end_line_number {
            Some(start_line_number)
        } else {
            None
        };

        // バリデーション成功後にのみ選択状態をクリア
        self.multiline_selection = None;

        self.input_mode = Some(InputMode::Suggestion {
            context: LineInputContext {
                file_index: self.selected_file,
                line_number: end_line_number,
                diff_position,
                start_line_number: start_line,
            },
            original_code: original_code.clone(),
        });
        self.input_text_area.set_content(&original_code);
        self.update_suggestion_highlight_cache();
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }
    pub(crate) fn open_comment_list(&mut self) {
        self.state = AppState::CommentList;
        if self.local_mode {
            self.cmt.comment_tab = CommentTab::Review;
        }
        self.cmt.discussion_comment_detail_mode = false;
        self.cmt.discussion_comment_detail_scroll = 0;

        // Load review comments
        self.load_review_comments();
        if self.local_mode {
            self.cmt.discussion_comments = Some(vec![]);
            self.cmt.discussion_comments_loading = false;
        } else {
            // Load discussion comments
            self.load_discussion_comments();
        }
    }

    /// Group flat review comments into threads using `in_reply_to_id`.
    pub(crate) fn build_review_threads(&mut self) {
        use std::collections::HashMap;
        self.cmt.review_threads.clear();

        let Some(ref comments) = self.cmt.review_comments else {
            return;
        };

        // Map comment id → index for lookup
        let id_to_idx: HashMap<u64, usize> = comments
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, i))
            .collect();

        // Group by root
        let n = comments.len();
        let mut root_to_thread: HashMap<usize, usize> = HashMap::new();
        for i in 0..n {
            // Walk in_reply_to_id chain to find the thread root.
            // Bounded by n to guard against cycles.
            let mut root_idx = i;
            for _ in 0..n {
                match comments[root_idx]
                    .in_reply_to_id
                    .and_then(|pid| id_to_idx.get(&pid))
                {
                    Some(&parent) if parent != root_idx => root_idx = parent,
                    _ => break,
                }
            }
            if let Some(&thread_idx) = root_to_thread.get(&root_idx) {
                if i != root_idx {
                    self.cmt.review_threads[thread_idx].replies.push(i);
                }
            } else {
                let thread_idx = self.cmt.review_threads.len();
                root_to_thread.insert(root_idx, thread_idx);
                let mut thread = CommentThread {
                    root: root_idx,
                    replies: Vec::new(),
                };
                if i != root_idx {
                    thread.replies.push(i);
                }
                self.cmt.review_threads.push(thread);
            }
        }

        // Sort replies within each thread by created_at
        for thread in &mut self.cmt.review_threads {
            thread
                .replies
                .sort_by(|&a, &b| comments[a].created_at.cmp(&comments[b].created_at));
        }

        // Sort threads by root comment's created_at
        self.cmt.review_threads.sort_by(|a, b| {
            comments[a.root]
                .created_at
                .cmp(&comments[b.root].created_at)
        });
    }

    /// Apply a fetched (or cached) set of review comments to the UI state:
    /// file comment counts, thread grouping, and selection reset.
    ///
    /// Selection state is preserved when the set of thread roots is unchanged
    /// (i.e. a background poll returned the same threads, possibly with new
    /// replies). A structural change (new/removed threads) resets selection.
    pub(crate) fn apply_review_comments(&mut self, comments: Vec<ReviewComment>) {
        // Count all comments per file (including replies) so the badge
        // reflects total activity, not just thread count.
        self.cmt.file_comment_counts.clear();
        for c in &comments {
            if c.path != "[PR Review]" {
                *self
                    .cmt
                    .file_comment_counts
                    .entry(c.path.clone())
                    .or_insert(0) += 1;
            }
        }

        let old_root_ids: Vec<u64> = self
            .cmt
            .review_threads
            .iter()
            .filter_map(|t| {
                self.cmt
                    .review_comments
                    .as_ref()
                    .and_then(|cs| cs.get(t.root))
                    .map(|c| c.id)
            })
            .collect();

        self.cmt.review_comments = Some(comments);
        self.build_review_threads();

        let new_root_ids: Vec<u64> = self
            .cmt
            .review_threads
            .iter()
            .filter_map(|t| {
                self.cmt
                    .review_comments
                    .as_ref()
                    .and_then(|cs| cs.get(t.root))
                    .map(|c| c.id)
            })
            .collect();

        let threads_unchanged = old_root_ids == new_root_ids;
        if !threads_unchanged {
            self.cmt.selected_comment = 0;
            self.cmt.comment_list_scroll_offset = 0;
            self.cmt.selected_thread = 0;
            self.cmt.thread_scroll_offset = 0;
            self.cmt.expanded_thread = None;
            self.cmt.expanded_selected = 0;
            self.cmt.expanded_selected_comment_id = None;
            self.cmt.expanded_scroll_offset = 0;
        } else if let Some(expanded_idx) = self.cmt.expanded_thread {
            // Thread content may have changed (new replies). Restore
            // the positional index by comment ID so the user doesn't
            // get silently shifted to a different comment.
            if let Some(thread) = self.cmt.review_threads.get(expanded_idx) {
                if let Some(target_id) = self.cmt.expanded_selected_comment_id {
                    let indices: Vec<usize> = std::iter::once(thread.root)
                        .chain(thread.replies.iter().copied())
                        .collect();
                    let Some(comments) = self.cmt.review_comments.as_deref() else {
                        return;
                    };
                    match indices.iter().position(|&ci| comments[ci].id == target_id) {
                        Some(pos) => self.cmt.expanded_selected = pos,
                        None => {
                            self.cmt.expanded_thread = None;
                            self.cmt.expanded_selected = 0;
                            self.cmt.expanded_selected_comment_id = None;
                        }
                    }
                }
            } else {
                self.cmt.expanded_thread = None;
                self.cmt.expanded_selected = 0;
                self.cmt.expanded_selected_comment_id = None;
            }
        }

        self.cmt.comments_loading = false;
    }

    /// Whether a remote review-comment fetch should be kicked off: only for
    /// non-local PRs that have neither loaded comments nor an in-flight fetch.
    /// The receiver check prevents the eager load (on PR select) and the
    /// post-data-load fallback from both firing and double-fetching.
    pub(crate) fn needs_review_comment_load(&self) -> bool {
        !self.local_mode
            && self.cmt.review_comments.is_none()
            && self.cmt.comment_receiver.is_none()
    }

    pub(crate) fn load_review_comments(&mut self) {
        let cache_key = PrCacheKey {
            repo: self.repo.clone(),
            pr_number: self.pr_number(),
        };

        // ローカルモードはディスクから毎回読み直す。session_cache は
        // ReviewComment しか保持できず LocalCommentMeta を捨ててしまうため、
        // CLI `update-local-comment` で resolved 状態が変わると TUI 表示が
        // 古いままになる。
        if self.local_mode {
            match load_local_review_comments(&self.repo, self.working_dir.as_deref()) {
                Ok(local_comments) => {
                    let (comments, meta) = split_local_comments(local_comments);
                    self.session_cache
                        .put_review_comments(cache_key, comments.clone());
                    self.cmt.local_comment_meta = meta;
                    self.apply_review_comments(comments);
                    if matches!(
                        self.state,
                        AppState::DiffView | AppState::SplitViewDiff | AppState::SplitViewFileList
                    ) {
                        self.update_file_comment_positions();
                        self.ensure_diff_cache();
                    }
                }
                Err(e) => {
                    self.cmt.review_comments = Some(vec![]);
                    self.cmt.local_comment_meta.clear();
                    self.cmt.comments_loading = false;
                    self.cmt.submission_result =
                        Some((false, format!("Failed to load local comments: {}", e)));
                    self.cmt.submission_result_time = Some(Instant::now());
                }
            }
            return;
        }

        if let Some(comments) = self.session_cache.get_review_comments(&cache_key) {
            self.cmt.local_comment_meta.clear();
            self.apply_review_comments(comments.to_vec());
            return;
        }

        self.cmt.local_comment_meta.clear();
        self.cmt.comments_loading = true;
        let (tx, rx) = mpsc::channel(1);
        let pr_number = self.pr_number();
        self.cmt.comment_receiver = Some((pr_number, rx));

        let repo = self.repo.clone();

        tokio::spawn(async move {
            // Fetch both review comments and reviews
            let review_comments_result =
                github::comment::fetch_review_comments(&repo, pr_number).await;
            let reviews_result = github::comment::fetch_reviews(&repo, pr_number).await;

            // Combine results
            let mut all_comments: Vec<ReviewComment> = Vec::new();

            // Add review comments (inline comments)
            if let Ok(comments) = review_comments_result {
                all_comments.extend(comments);
            }

            // Convert reviews to ReviewComment format (only those with body)
            if let Ok(reviews) = reviews_result {
                for review in reviews {
                    if let Some(body) = review.body {
                        if !body.trim().is_empty() {
                            all_comments.push(ReviewComment {
                                id: review.id,
                                path: "[PR Review]".to_string(),
                                line: None,
                                start_line: None,
                                body,
                                user: review.user,
                                created_at: review.submitted_at.unwrap_or_default(),
                                in_reply_to_id: None,
                            });
                        }
                    }
                }
            }

            // Sort by created_at
            all_comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));

            let _ = tx.send(Ok(all_comments)).await;
        });
    }

    pub(crate) fn load_discussion_comments(&mut self) {
        let cache_key = PrCacheKey {
            repo: self.repo.clone(),
            pr_number: self.pr_number(),
        };

        if let Some(comments) = self.session_cache.get_discussion_comments(&cache_key) {
            self.cmt.discussion_comments = Some(comments.to_vec());
            self.cmt.selected_discussion_comment = 0;
            self.cmt.discussion_comments_loading = false;
            return;
        }

        self.cmt.discussion_comments_loading = true;
        let (tx, rx) = mpsc::channel(1);
        let pr_number = self.pr_number();
        self.cmt.discussion_comment_receiver = Some((pr_number, rx));

        let repo = self.repo.clone();

        tokio::spawn(async move {
            match github::comment::fetch_discussion_comments(&repo, pr_number).await {
                Ok(comments) => {
                    let _ = tx.send(Ok(comments)).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });
    }
    pub(crate) async fn handle_comment_list_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        if self.local_mode {
            return self.handle_local_comment_list_input(key, terminal).await;
        }

        let visible_lines = terminal.size()?.height.saturating_sub(8) as usize;

        if self.cmt.discussion_comment_detail_mode {
            return self.handle_discussion_detail_input(key, visible_lines);
        }

        let kb = self.config.keybindings.clone();

        if self.matches_single_key(&key, &kb.help) {
            self.open_help(AppState::CommentList);
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            // In expanded thread view, collapse first; otherwise go back
            if self.cmt.comment_tab == CommentTab::Review && self.cmt.expanded_thread.is_some() {
                self.cmt.expanded_thread = None;
                self.cmt.expanded_selected = 0;
                self.cmt.expanded_selected_comment_id = None;
            } else {
                self.state = self.previous_state;
            }
        } else if self.matches_single_key(&key, &kb.tab_prev)
            || self.matches_single_key(&key, &kb.tab_next)
        {
            self.cmt.comment_tab = match self.cmt.comment_tab {
                CommentTab::Review => CommentTab::Discussion,
                CommentTab::Discussion => CommentTab::Review,
            };
        } else if self.matches_single_key(&key, &kb.move_down) {
            match self.cmt.comment_tab {
                CommentTab::Review => self.review_nav_down(1),
                CommentTab::Discussion => {
                    if let Some(ref comments) = self.cmt.discussion_comments {
                        if !comments.is_empty() {
                            self.cmt.selected_discussion_comment =
                                (self.cmt.selected_discussion_comment + 1)
                                    .min(comments.len().saturating_sub(1));
                        }
                    }
                }
            }
        } else if self.matches_single_key(&key, &kb.move_up) {
            match self.cmt.comment_tab {
                CommentTab::Review => self.review_nav_up(1),
                CommentTab::Discussion => {
                    self.cmt.selected_discussion_comment =
                        self.cmt.selected_discussion_comment.saturating_sub(1);
                }
            }
        } else if self.matches_single_key(&key, &kb.page_down)
            || Self::is_shift_char_shortcut(&key, 'j')
        {
            let step = visible_lines.max(1);
            match self.cmt.comment_tab {
                CommentTab::Review => self.review_nav_down(step),
                CommentTab::Discussion => {
                    if let Some(ref comments) = self.cmt.discussion_comments {
                        if !comments.is_empty() {
                            self.cmt.selected_discussion_comment =
                                (self.cmt.selected_discussion_comment + step)
                                    .min(comments.len() - 1);
                        }
                    }
                }
            }
        } else if self.matches_single_key(&key, &kb.page_up)
            || Self::is_shift_char_shortcut(&key, 'k')
        {
            let step = visible_lines.max(1);
            match self.cmt.comment_tab {
                CommentTab::Review => self.review_nav_up(step),
                CommentTab::Discussion => {
                    self.cmt.selected_discussion_comment =
                        self.cmt.selected_discussion_comment.saturating_sub(step);
                }
            }
        } else if self.matches_single_key(&key, &kb.open_panel) {
            match self.cmt.comment_tab {
                CommentTab::Review => self.review_tab_open_panel(),
                CommentTab::Discussion => {
                    if self
                        .cmt
                        .discussion_comments
                        .as_ref()
                        .map(|c| !c.is_empty())
                        .unwrap_or(false)
                    {
                        self.cmt.discussion_comment_detail_mode = true;
                        self.cmt.discussion_comment_detail_scroll = 0;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_local_comment_list_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let visible_lines = terminal.size()?.height.saturating_sub(8) as usize;
        self.handle_local_comment_list_key(key, visible_lines)
    }

    /// Terminal-free core of the local-mode comment list input. Local mode
    /// renders the same threaded review list as GitHub mode, so navigation
    /// drives `selected_thread` / expanded-thread state — not the flat
    /// `selected_comment` the renderer no longer reads.
    pub(crate) fn handle_local_comment_list_key(
        &mut self,
        key: event::KeyEvent,
        visible_lines: usize,
    ) -> Result<()> {
        let kb = self.config.keybindings.clone();

        if !self.pending_keys.is_empty() {
            if let Some(kb_event) = event_to_keybinding(&key) {
                self.push_pending_key(kb_event);

                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    self.review_jump_to_first();
                    return Ok(());
                }

                self.clear_pending_keys();
            } else {
                self.clear_pending_keys();
            }
            return Ok(());
        }

        if self.key_could_match_sequence(&key, &kb.jump_to_first) {
            if let Some(kb_event) = event_to_keybinding(&key) {
                self.push_pending_key(kb_event);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            // In expanded thread view, collapse first; otherwise go back.
            if self.cmt.expanded_thread.is_some() {
                self.cmt.expanded_thread = None;
                self.cmt.expanded_selected = 0;
                self.cmt.expanded_selected_comment_id = None;
            } else {
                self.state = self.previous_state;
            }
        } else if self.matches_single_key(&key, &kb.move_down) {
            self.review_nav_down(1);
        } else if self.matches_single_key(&key, &kb.move_up) {
            self.review_nav_up(1);
        } else if self.matches_single_key(&key, &kb.page_down)
            || Self::is_shift_char_shortcut(&key, 'j')
        {
            self.review_nav_down(visible_lines.max(1));
        } else if self.matches_single_key(&key, &kb.page_up)
            || Self::is_shift_char_shortcut(&key, 'k')
        {
            self.review_nav_up(visible_lines.max(1));
        } else if self.matches_single_key(&key, &kb.jump_to_last) {
            self.review_jump_to_last();
        } else if self.matches_single_key(&key, &kb.open_panel) {
            self.review_tab_open_panel();
        }

        Ok(())
    }

    /// Enter on the review list: jump within an expanded thread, expand a
    /// thread that has replies, or jump straight to a single-comment file.
    /// Shared by GitHub-mode and local-mode comment list handlers.
    fn review_tab_open_panel(&mut self) {
        if let Some(thread_idx) = self
            .cmt
            .expanded_thread
            .filter(|&i| i < self.cmt.review_threads.len())
        {
            // Jump to the selected comment within the expanded thread.
            let thread = &self.cmt.review_threads[thread_idx];
            let indices: Vec<usize> = std::iter::once(thread.root)
                .chain(thread.replies.iter().copied())
                .collect();
            if let Some(&ci) = indices.get(self.cmt.expanded_selected) {
                self.cmt.selected_comment = ci;
                self.jump_to_comment();
            }
        } else if !self.cmt.review_threads.is_empty() {
            let last = self.cmt.review_threads.len() - 1;
            self.cmt.selected_thread = self.cmt.selected_thread.min(last);
            let thread = &self.cmt.review_threads[self.cmt.selected_thread];
            if thread.replies.is_empty() {
                // Single comment, jump to file.
                self.cmt.selected_comment = thread.root;
                self.jump_to_comment();
            } else {
                // Expand thread.
                self.cmt.expanded_thread = Some(self.cmt.selected_thread);
                self.cmt.expanded_selected = 0;
                self.cmt.expanded_scroll_offset = 0;
                self.sync_expanded_comment_id();
            }
        }
    }

    fn review_jump_to_first(&mut self) {
        if self.cmt.expanded_thread.is_some() {
            self.cmt.expanded_selected = 0;
            self.sync_expanded_comment_id();
        } else {
            self.cmt.selected_thread = 0;
        }
    }

    fn review_jump_to_last(&mut self) {
        if let Some(thread_idx) = self
            .cmt
            .expanded_thread
            .filter(|&i| i < self.cmt.review_threads.len())
        {
            self.cmt.expanded_selected = self.cmt.review_threads[thread_idx].replies.len();
            self.sync_expanded_comment_id();
        } else {
            self.cmt.selected_thread = self.cmt.review_threads.len().saturating_sub(1);
        }
    }

    fn review_nav_down(&mut self, step: usize) {
        if let Some(thread_idx) = self
            .cmt
            .expanded_thread
            .filter(|&i| i < self.cmt.review_threads.len())
        {
            let thread = &self.cmt.review_threads[thread_idx];
            let max = thread.replies.len(); // 0 = root, 1..=len = replies
            self.cmt.expanded_selected = (self.cmt.expanded_selected + step).min(max);
            self.sync_expanded_comment_id();
        } else {
            let max = self.cmt.review_threads.len().saturating_sub(1);
            self.cmt.selected_thread = (self.cmt.selected_thread + step).min(max);
        }
    }

    fn review_nav_up(&mut self, step: usize) {
        if self
            .cmt
            .expanded_thread
            .filter(|&i| i < self.cmt.review_threads.len())
            .is_some()
        {
            self.cmt.expanded_selected = self.cmt.expanded_selected.saturating_sub(step);
            self.sync_expanded_comment_id();
        } else {
            self.cmt.selected_thread = self.cmt.selected_thread.saturating_sub(step);
        }
    }

    /// Store the comment ID for the current `expanded_selected` position
    /// so it can be restored after a background poll rebuilds threads.
    fn sync_expanded_comment_id(&mut self) {
        let Some(thread_idx) = self.cmt.expanded_thread else {
            return;
        };
        let Some(thread) = self.cmt.review_threads.get(thread_idx) else {
            return;
        };
        let indices: Vec<usize> = std::iter::once(thread.root)
            .chain(thread.replies.iter().copied())
            .collect();
        self.cmt.expanded_selected_comment_id = indices
            .get(self.cmt.expanded_selected)
            .and_then(|&ci| self.cmt.review_comments.as_ref()?.get(ci))
            .map(|c| c.id);
    }

    pub(crate) fn handle_discussion_detail_input(
        &mut self,
        key: event::KeyEvent,
        visible_lines: usize,
    ) -> Result<()> {
        let kb = self.config.keybindings.clone();
        if self.matches_single_key(&key, &kb.quit) || self.matches_single_key(&key, &kb.open_panel)
        {
            self.cmt.discussion_comment_detail_mode = false;
            self.cmt.discussion_comment_detail_scroll = 0;
        } else if self.matches_single_key(&key, &kb.move_down) {
            self.cmt.discussion_comment_detail_scroll =
                self.cmt.discussion_comment_detail_scroll.saturating_add(1);
        } else if self.matches_single_key(&key, &kb.move_up) {
            self.cmt.discussion_comment_detail_scroll =
                self.cmt.discussion_comment_detail_scroll.saturating_sub(1);
        } else if Self::is_shift_char_shortcut(&key, 'j') {
            self.cmt.discussion_comment_detail_scroll = self
                .cmt
                .discussion_comment_detail_scroll
                .saturating_add(visible_lines.max(1));
        } else if Self::is_shift_char_shortcut(&key, 'k') {
            self.cmt.discussion_comment_detail_scroll = self
                .cmt
                .discussion_comment_detail_scroll
                .saturating_sub(visible_lines.max(1));
        } else if self.matches_single_key(&key, &kb.page_down) {
            self.cmt.discussion_comment_detail_scroll = self
                .cmt
                .discussion_comment_detail_scroll
                .saturating_add(visible_lines / 2);
        } else if self.matches_single_key(&key, &kb.page_up) {
            self.cmt.discussion_comment_detail_scroll = self
                .cmt
                .discussion_comment_detail_scroll
                .saturating_sub(visible_lines / 2);
        }
        Ok(())
    }
    pub(crate) fn jump_to_comment(&mut self) {
        let Some(ref comments) = self.cmt.review_comments else {
            return;
        };
        let Some(comment) = comments.get(self.cmt.selected_comment) else {
            return;
        };

        let target_path = &comment.path;

        // Find file index by path
        let file_index = self.files().iter().position(|f| &f.filename == target_path);

        if let Some(idx) = file_index {
            self.selected_file = idx;
            self.diff_view_return_state = AppState::FileList;
            self.state = AppState::DiffView;
            self.diff_scroll.selected_line = 0;
            self.diff_scroll.scroll_offset = 0;
            self.update_diff_line_count();
            self.update_file_comment_positions();
            self.ensure_diff_cache();

            // Find diff line index from pre-computed positions
            let diff_line_index = self
                .cmt
                .file_comment_positions
                .iter()
                .find(|pos| pos.comment_index == self.cmt.selected_comment)
                .map(|pos| pos.diff_line_index);

            if let Some(line_idx) = diff_line_index {
                self.diff_scroll.selected_line = line_idx;
                self.diff_scroll.scroll_offset = line_idx;
            }
        }
    }

    /// Update file_comment_positions based on current file and review_comments
    pub(crate) fn update_file_comment_positions(&mut self) {
        self.cmt.file_comment_positions.clear();
        self.cmt.file_comment_lines.clear();

        let Some(file) = self.files().get(self.selected_file) else {
            return;
        };
        let Some(patch) = file.patch.clone() else {
            return;
        };
        let filename = file.filename.clone();

        let Some(ref comments) = self.cmt.review_comments else {
            return;
        };

        for (i, comment) in comments.iter().enumerate() {
            // Skip comments for other files
            if comment.path != filename {
                continue;
            }
            // Skip PR-level comments (line: None)
            let Some(line_num) = comment.line else {
                continue;
            };
            if let Some(diff_index) = Self::find_diff_line_index(&patch, line_num) {
                self.cmt.file_comment_positions.push(CommentPosition {
                    diff_line_index: diff_index,
                    comment_index: i,
                });
                self.cmt.file_comment_lines.insert(diff_index);
            }
        }
        self.cmt
            .file_comment_positions
            .sort_by_key(|pos| pos.diff_line_index);
    }

    /// Static helper to find diff line index for a given line number
    pub(crate) fn find_diff_line_index(patch: &str, target_line: u32) -> Option<usize> {
        let lines: Vec<&str> = patch.lines().collect();
        let mut new_line_number: Option<u32> = None;

        for (i, line) in lines.iter().enumerate() {
            if line.starts_with("@@") {
                // Parse hunk header to get starting line number
                if let Some(plus_pos) = line.find('+') {
                    let after_plus = &line[plus_pos + 1..];
                    let end_pos = after_plus.find([',', ' ']).unwrap_or(after_plus.len());
                    if let Ok(num) = after_plus[..end_pos].parse::<u32>() {
                        new_line_number = Some(num);
                    }
                }
            } else if line.starts_with('+') || line.starts_with(' ') {
                if let Some(current) = new_line_number {
                    if current == target_line {
                        return Some(i);
                    }
                    new_line_number = Some(current + 1);
                }
            }
            // Removed lines don't increment new_line_number
        }

        None
    }
    /// Get comment indices at the current selected line
    pub fn get_comment_indices_at_current_line(&self) -> Vec<usize> {
        self.cmt
            .file_comment_positions
            .iter()
            .filter(|pos| pos.diff_line_index == self.diff_scroll.selected_line)
            .map(|pos| pos.comment_index)
            .collect()
    }

    /// Check if current line has any comments
    pub fn has_comment_at_current_line(&self) -> bool {
        self.cmt
            .file_comment_positions
            .iter()
            .any(|pos| pos.diff_line_index == self.diff_scroll.selected_line)
    }

    /// テキスト行がパネル幅内で折り返される表示行数を計算
    pub(crate) fn wrapped_line_count(text: &str, panel_width: usize) -> usize {
        if panel_width == 0 {
            return 1;
        }
        let char_count = text.chars().count();
        if char_count == 0 {
            return 1;
        }
        char_count.div_ceil(panel_width)
    }

    /// コメント本文の折り返しを考慮した表示行数を計算
    pub(crate) fn comment_body_wrapped_lines(body: &str, panel_width: usize) -> usize {
        body.lines()
            .map(|line| Self::wrapped_line_count(line, panel_width))
            .sum::<usize>()
            .max(1) // 空の本文でも最低1行
    }

    /// コメントパネルのコンテンツ行数を計算（スクロール上限算出用）
    pub(crate) fn comment_panel_content_lines(&self, panel_inner_width: usize) -> usize {
        let indices = self.get_comment_indices_at_current_line();
        if indices.is_empty() {
            return 1; // "No comments..." message
        }
        let Some(ref comments) = self.cmt.review_comments else {
            return 0;
        };
        let mut count = 0usize;
        for (i, &idx) in indices.iter().enumerate() {
            let Some(comment) = comments.get(idx) else {
                continue;
            };
            if i > 0 {
                count += 1; // separator
            }
            count += 1; // header
            count += Self::comment_body_wrapped_lines(&comment.body, panel_inner_width);
            count += 1; // spacing
        }
        count
    }

    /// 指定インラインコメントのパネル内行オフセットを計算（スクロール追従用）
    pub(crate) fn comment_panel_offset_for(&self, target: usize, panel_inner_width: usize) -> u16 {
        let indices = self.get_comment_indices_at_current_line();
        let Some(ref comments) = self.cmt.review_comments else {
            return 0;
        };
        let mut offset = 0usize;
        for (i, &idx) in indices.iter().enumerate() {
            if i == target {
                break;
            }
            let Some(comment) = comments.get(idx) else {
                continue;
            };
            if i > 0 {
                offset += 1; // separator
            }
            offset += 1; // header
            offset += Self::comment_body_wrapped_lines(&comment.body, panel_inner_width);
            offset += 1; // spacing
        }
        if target > 0 {
            offset += 1; // separator before target
        }
        offset as u16
    }

    /// コメントパネルの内側幅を計算（borders分の2を差し引く）
    pub(crate) fn comment_panel_inner_width(&self, terminal_width: usize) -> usize {
        let panel_width = match self.state {
            AppState::SplitViewDiff => terminal_width * 65 / 100,
            _ => terminal_width,
        };
        panel_width.saturating_sub(2) // borders
    }

    /// コメントパネルのスクロール上限を計算
    pub(crate) fn max_comment_panel_scroll(
        &self,
        terminal_height: usize,
        terminal_width: usize,
    ) -> u16 {
        let panel_inner_width = self.comment_panel_inner_width(terminal_width);
        let content_lines = self.comment_panel_content_lines(panel_inner_width);
        let panel_inner_height = (terminal_height.saturating_sub(8) * 40 / 100).max(1);
        content_lines.saturating_sub(panel_inner_height) as u16
    }
    pub(crate) fn jump_to_next_comment(&mut self) {
        let next = self
            .cmt
            .file_comment_positions
            .iter()
            .find(|pos| pos.diff_line_index > self.diff_scroll.selected_line);

        if let Some(pos) = next {
            self.diff_scroll.selected_line = pos.diff_line_index;
            self.diff_scroll.scroll_offset = self.diff_scroll.selected_line;
        }
    }

    /// Jump to previous comment in the diff (no wrap-around, scroll to top)
    pub(crate) fn jump_to_prev_comment(&mut self) {
        let prev = self
            .cmt
            .file_comment_positions
            .iter()
            .rev()
            .find(|pos| pos.diff_line_index < self.diff_scroll.selected_line);

        if let Some(pos) = prev {
            self.diff_scroll.selected_line = pos.diff_line_index;
            self.diff_scroll.scroll_offset = self.diff_scroll.selected_line;
        }
    }
    pub(crate) fn enter_reply_input(&mut self) {
        let indices = self.get_comment_indices_at_current_line();
        if indices.is_empty() {
            return;
        }

        let local_idx = self
            .cmt
            .selected_inline_comment
            .min(indices.len().saturating_sub(1));
        let comment_idx = indices[local_idx];

        let Some(ref comments) = self.cmt.review_comments else {
            return;
        };
        let Some(comment) = comments.get(comment_idx) else {
            return;
        };

        self.input_mode = Some(InputMode::Reply {
            comment_id: comment.id,
            reply_to_user: comment.user.login.clone(),
            reply_to_body: comment.body.clone(),
        });
        self.input_text_area.clear();
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }
}
