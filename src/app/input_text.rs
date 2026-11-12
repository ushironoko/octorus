use anyhow::Result;
use chrono::Utc;
use crossterm::event::{self, KeyEvent};
use tokio::sync::mpsc;

use crate::cache::{
    load_local_review_comments, save_local_review_comments, LocalReviewComment, PrCacheKey,
};
use crate::github;
use crate::github::comment::ReviewComment;
use crate::loader::CommentSubmitResult;
use crate::ui::text_area::TextAreaAction;

use super::types::*;
use super::{App, SuggestionHighlightCache, INLINE_COMMENT_TIMEOUT};

/// GitHub's draft `side` for the diff side the composer is writing to.
fn draft_side(side: crate::diff::DiffSide) -> github::DraftSide {
    match side {
        crate::diff::DiffSide::Left => github::DraftSide::Left,
        crate::diff::DiffSide::Right => github::DraftSide::Right,
    }
}

impl App {
    /// Note: the help keybinding (?) is intentionally not handled here
    /// because this is a free-form text input and ? is a literal character.
    pub(crate) fn handle_text_input(&mut self, key: event::KeyEvent) -> Result<()> {
        // 送信中は入力を無視（各送信種別に対応するInputModeのみブロック）
        if self.cmt.comment_submitting {
            return Ok(());
        }
        if self.is_issue_comment_submitting()
            && matches!(self.input_mode, Some(InputMode::IssueComment { .. }))
        {
            return Ok(());
        }

        match self.input_text_area.input(key) {
            TextAreaAction::Submit => {
                let content = self.input_text_area.content();
                if content.trim().is_empty() {
                    // 空の場合はキャンセル扱い
                    self.cancel_input();
                    return Ok(());
                }

                match self.input_mode.take() {
                    Some(InputMode::Comment(ctx)) => {
                        self.submit_comment(ctx, content);
                    }
                    Some(InputMode::Suggestion {
                        context,
                        original_code: _,
                    }) => {
                        self.submit_suggestion(context, content);
                        self.suggestion_highlight_cache = None;
                    }
                    Some(InputMode::Reply { comment_id, .. }) => {
                        self.submit_reply(comment_id, content);
                    }
                    Some(InputMode::IssueComment { issue_number }) => {
                        self.submit_issue_comment(issue_number, content);
                    }
                    None => {}
                }
                self.state = self.preview_return_state;
            }
            TextAreaAction::Cancel => {
                self.cancel_input();
            }
            TextAreaAction::Continue => {
                // サジェスチョンモードではハイライトキャッシュを更新
                if matches!(self.input_mode, Some(InputMode::Suggestion { .. })) {
                    self.update_suggestion_highlight_cache();
                }
            }
            TextAreaAction::PendingSequence => {
                // Waiting for more keys in a sequence, do nothing
            }
        }
        Ok(())
    }
    /// Closes the composer. Text typed into a comment or suggestion is kept for its
    /// line rather than thrown away, so that reopening the line restores it.
    pub(crate) fn cancel_input(&mut self) {
        if let Some(kind) = self.keep_unsent_draft() {
            let key = match kind {
                DraftKind::Comment => self.config.keybindings.comment.display(),
                DraftKind::Suggestion => self.config.keybindings.suggestion.display(),
            };
            self.cmt.submission_result = Some(Toast::success(format!(
                "Draft kept for this line: {key} restores it"
            )));
        }
        self.input_mode = None;
        self.input_text_area.clear();
        self.suggestion_highlight_cache = None;
        self.state = self.preview_return_state;
    }

    /// The line an inline composer writes to. Replies and issue comments post at
    /// once and are not kept, so they have no anchor.
    pub(crate) fn inline_draft_anchor(&self, mode: &InputMode) -> Option<DraftAnchor> {
        let (ctx, kind) = match mode {
            InputMode::Comment(ctx) => (ctx, DraftKind::Comment),
            InputMode::Suggestion { context, .. } => (context, DraftKind::Suggestion),
            InputMode::Reply { .. } | InputMode::IssueComment { .. } => return None,
        };
        let path = self.files().get(ctx.file_index)?.filename.clone();
        Some(DraftAnchor {
            pr_number: self.pr_number.unwrap_or_default(),
            path,
            line: ctx.line_number,
            side: ctx.side,
            start_line: ctx.start_line_number,
            kind,
        })
    }

    /// Puts the composer into `mode`. The text starts as `initial`, unless a body
    /// was kept for the same line, which is restored instead.
    pub(crate) fn open_inline_composer(&mut self, mode: InputMode, initial: &str) {
        let restored = self
            .inline_draft_anchor(&mode)
            .and_then(|anchor| self.cmt.saved_drafts.remove(&anchor));
        self.input_mode = Some(mode);
        match restored {
            Some(body) => self.input_text_area.set_content_verbatim(&body),
            None => self.input_text_area.set_content(initial),
        }
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }

    /// Keeps the text of the open comment or suggestion for its line. Returns the
    /// kind kept, or `None` when there was no text to keep.
    fn keep_unsent_draft(&mut self) -> Option<DraftKind> {
        let anchor = self.inline_draft_anchor(self.input_mode.as_ref()?)?;
        let content = self.input_text_area.content();
        if content.trim().is_empty() {
            return None;
        }
        let kind = anchor.kind;
        self.cmt.saved_drafts.insert(anchor, content);
        Some(kind)
    }

    /// Keeps the text of the in-flight add when it ends without success, or is
    /// abandoned with its result lost. The same line can then retry it.
    pub(crate) fn keep_failed_add(&mut self) {
        if let Some(add) = self.cmt.pending_add.take() {
            self.cmt.saved_drafts.insert(add.anchor, add.body);
        }
    }

    /// サジェスチョン入力のハイライトキャッシュを更新する
    ///
    /// コンテンツのハッシュが変わった場合のみ再構築する。
    /// 毎フレーム ParserPool を再生成するコストを回避。
    pub(crate) fn update_suggestion_highlight_cache(&mut self) {
        let filename = match &self.input_mode {
            Some(InputMode::Suggestion { context, .. }) => self
                .files()
                .get(context.file_index)
                .map(|f| f.filename.clone()),
            _ => None,
        };

        let Some(filename) = filename else {
            return;
        };

        let content = self.input_text_area.content();
        let content_hash = hash_string(&content);
        let theme_name = self.config.diff.theme.clone();

        // キャッシュが有効ならスキップ（content_hash, filename, theme_name すべて一致時のみ）
        if let Some(ref cache) = self.suggestion_highlight_cache {
            if cache.content_hash == content_hash
                && cache.filename == filename
                && cache.theme_name == theme_name
            {
                return;
            }
        }

        let lines =
            crate::ui::diff_view::highlight_text_for_suggestion(&content, &filename, &theme_name);

        self.suggestion_highlight_cache = Some(SuggestionHighlightCache {
            content_hash,
            filename,
            theme_name,
            lines,
        });
    }
    pub(crate) fn submit_comment(&mut self, ctx: LineInputContext, body: String) {
        self.submit_review_comment_inner(ctx, DraftKind::Comment, body);
    }

    pub(crate) fn submit_suggestion(&mut self, ctx: LineInputContext, suggested_code: String) {
        self.submit_review_comment_inner(ctx, DraftKind::Suggestion, suggested_code);
    }

    /// Sends one inline comment or suggestion. `typed` is the text the user wrote.
    /// A suggestion is wrapped in a `suggestion` fence only for GitHub, so the text
    /// kept on failure is the unwrapped one the composer shows.
    fn submit_review_comment_inner(
        &mut self,
        ctx: LineInputContext,
        kind: DraftKind,
        typed: String,
    ) {
        let body = match kind {
            DraftKind::Comment => typed.clone(),
            DraftKind::Suggestion => format!("```suggestion\n{}\n```", typed.trim_end()),
        };
        if self.local_mode {
            self.submit_local_review_comment(ctx, body);
            return;
        }

        let Some(file) = self.files().get(ctx.file_index) else {
            return;
        };
        let Some(pr) = self.pr() else {
            return;
        };

        let commit_id = pr.head.sha.clone();
        let pr_node_id = pr.node_id.clone();
        let pr_number = self.pr_number();
        let path = file.filename.clone();
        let anchor = DraftAnchor {
            pr_number,
            path: path.clone(),
            line: ctx.line_number,
            side: ctx.side,
            start_line: ctx.start_line_number,
            kind,
        };
        // `side` comes from the selected rows. A removed line is LEFT and its
        // number is the old file. A range is one side, so start_side matches.
        let side = draft_side(ctx.side);
        let draft = github::InlineDraft {
            path,
            body,
            line: ctx.line_number,
            start_line: ctx.start_line_number,
            side,
            start_side: ctx.start_line_number.map(|_| side),
        };
        let repo = self.repo.clone();
        // Held until the result arrives, so that a failed add can keep the text.
        self.cmt.pending_add = Some(PendingAdd {
            anchor,
            body: typed,
        });

        // The comment joins the viewer's pending review. GitHub publishes it only
        // when a review is submitted, so it is not posted on its own (issue #169).
        self.spawn_inline_comment_add(pr_number, INLINE_COMMENT_TIMEOUT, async move {
            github::add_inline_draft(&repo, pr_number, pr_node_id.as_deref(), &commit_id, &draft)
                .await
        });
    }

    fn next_local_comment_id(comments: &[LocalReviewComment]) -> u64 {
        comments.iter().map(|c| c.comment.id).max().unwrap_or(0) + 1
    }

    fn submit_local_review_comment(&mut self, ctx: LineInputContext, body: String) {
        let Some(file) = self.files().get(ctx.file_index) else {
            return;
        };

        let mut comments = match load_local_review_comments(&self.repo, self.working_dir.as_deref())
        {
            Ok(comments) => comments,
            Err(e) => {
                self.cmt.submission_result = Some(Toast::failure(format!(
                    "Failed to load local comments: {}",
                    e
                )));
                return;
            }
        };

        let next_id = Self::next_local_comment_id(&comments);
        comments.push(LocalReviewComment::new(ReviewComment {
            id: next_id,
            path: file.filename.clone(),
            line: Some(ctx.line_number),
            start_line: ctx.start_line_number,
            body,
            user: github::User {
                login: Self::local_comment_author(),
            },
            created_at: Utc::now().to_rfc3339(),
            in_reply_to_id: None,
            location: github::comment::ReviewCommentLocation {
                side: Some(ctx.side.as_github_side().to_string()),
                start_side: ctx
                    .start_line_number
                    .map(|_| ctx.side.as_github_side().to_string()),
                ..Default::default()
            },
        }));

        self.persist_local_review_comments(comments, "Saved local comment");
    }

    pub(crate) fn submit_reply(&mut self, comment_id: u64, body: String) {
        if self.local_mode {
            self.submit_local_reply(comment_id, body);
            return;
        }

        let repo = self.repo.clone();
        let pr_number = self.pr_number();

        let (tx, rx) = mpsc::channel(1);
        self.cmt.comment_submit_receiver = Some((pr_number, rx));
        self.cmt.comment_submitting = true;

        tokio::spawn(async move {
            let result = github::create_reply_comment(&repo, pr_number, comment_id, &body).await;

            let _ = tx
                .send(match result {
                    Ok(_) => CommentSubmitResult::Success,
                    Err(e) => CommentSubmitResult::Error(e.to_string()),
                })
                .await;
        });
    }

    pub(crate) fn submit_local_reply(&mut self, comment_id: u64, body: String) {
        let Some(parent) = self
            .cmt
            .review_comments
            .as_ref()
            .and_then(|comments: &Vec<ReviewComment>| {
                comments.iter().find(|comment| comment.id == comment_id)
            })
            .cloned()
        else {
            self.cmt.submission_result = Some(Toast::failure("Reply target not found".to_string()));
            return;
        };

        let Some(line_number) = parent.line else {
            self.cmt.submission_result =
                Some(Toast::failure("Reply target has no line".to_string()));
            return;
        };

        let mut comments = match load_local_review_comments(&self.repo, self.working_dir.as_deref())
        {
            Ok(comments) => comments,
            Err(e) => {
                self.cmt.submission_result = Some(Toast::failure(format!(
                    "Failed to load local comments: {}",
                    e
                )));
                return;
            }
        };

        let next_id = Self::next_local_comment_id(&comments);
        comments.push(LocalReviewComment::new(ReviewComment {
            id: next_id,
            path: parent.path,
            line: Some(line_number),
            start_line: None,
            body,
            user: github::User {
                login: Self::local_comment_author(),
            },
            created_at: Utc::now().to_rfc3339(),
            in_reply_to_id: Some(comment_id),
            location: github::comment::ReviewCommentLocation {
                side: parent.location.side.clone(),
                ..Default::default()
            },
        }));

        self.persist_local_review_comments(comments, "Saved local reply");
    }

    fn persist_local_review_comments(
        &mut self,
        comments: Vec<LocalReviewComment>,
        success_message: &str,
    ) {
        if let Err(e) =
            save_local_review_comments(&self.repo, self.working_dir.as_deref(), &comments)
        {
            self.cmt.submission_result = Some(Toast::failure(format!(
                "Failed to save local comments: {}",
                e
            )));
            return;
        }

        let (review_comments, meta) = super::comments::split_local_comments(comments);
        let cache_key = PrCacheKey {
            repo: self.repo.clone(),
            pr_number: self.pr_number(),
        };
        self.session_cache
            .put_review_comments(cache_key, review_comments.clone());
        self.cmt.review_comments = Some(review_comments);
        self.cmt.local_comment_meta = meta;
        self.cmt.selected_comment = self
            .cmt
            .review_comments
            .as_ref()
            .map(|comments: &Vec<ReviewComment>| comments.len().saturating_sub(1))
            .unwrap_or(0);
        self.cmt.comments_loading = false;
        self.cmt.comment_submitting = false;
        self.cmt.comment_submit_receiver = None;
        self.update_file_comment_positions();
        // ensure_diff_cache() requires a tokio runtime for spawn_blocking.
        // In sync test contexts (e.g. #[test] without #[tokio::test]), no runtime
        // is available, so we skip the call to avoid a panic.
        if tokio::runtime::Handle::try_current().is_ok() {
            self.ensure_diff_cache();
        }
        self.cmt.submission_result = Some(Toast::success(success_message.to_string()));
    }

    fn local_comment_author() -> String {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "local".to_string())
    }

    pub(crate) fn is_issue_comment_submitting(&self) -> bool {
        self.issue_state
            .as_ref()
            .is_some_and(|s| s.issue_comment_submitting)
    }

    pub(crate) fn submit_issue_comment(&mut self, issue_number: u32, body: String) {
        let Some(ref mut state) = self.issue_state else {
            return;
        };

        let (tx, rx) = mpsc::channel(1);
        state.issue_comment_submit_receiver = Some((issue_number, rx));
        state.issue_comment_submitting = true;

        let repo = self.repo.clone();
        tokio::spawn(async move {
            let result = github::create_issue_comment(&repo, issue_number, &body).await;
            let _ = tx.send(result.map_err(|e| e.to_string())).await;
        });
    }

    pub(super) fn handle_pending_approve_choice(&mut self, key: &KeyEvent) -> PendingApproveChoice {
        if self.cmt.pending_approve_body.is_none() {
            return PendingApproveChoice::Ignore;
        }
        if self.matches_single_key(key, &self.config.keybindings.approve) {
            PendingApproveChoice::Submit
        } else if self.matches_single_key(key, &self.config.keybindings.quit) {
            self.cmt.pending_approve_body = None;
            self.cmt.submission_result = None;
            PendingApproveChoice::Cancel
        } else {
            PendingApproveChoice::Ignore
        }
    }
}
