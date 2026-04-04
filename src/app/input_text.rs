use anyhow::Result;
use chrono::Utc;
use crossterm::event::{self, KeyEvent};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::cache::{load_local_review_comments, save_local_review_comments, PrCacheKey};
use crate::github;
use crate::github::comment::ReviewComment;
use crate::loader::CommentSubmitResult;
use crate::ui::text_area::TextAreaAction;

use super::types::*;
use super::{App, SuggestionHighlightCache};

impl App {
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
    pub(crate) fn cancel_input(&mut self) {
        self.input_mode = None;
        self.input_text_area.clear();
        self.suggestion_highlight_cache = None;
        self.state = self.preview_return_state;
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
        self.submit_review_comment_inner(ctx, body);
    }

    pub(crate) fn submit_suggestion(&mut self, ctx: LineInputContext, suggested_code: String) {
        let body = format!("```suggestion\n{}\n```", suggested_code.trim_end());
        self.submit_review_comment_inner(ctx, body);
    }

    fn submit_review_comment_inner(&mut self, ctx: LineInputContext, body: String) {
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
        let filename = file.filename.clone();
        let repo = self.repo.clone();
        let pr_number = self.pr_number();
        let position = ctx.diff_position;
        let start_line = ctx.start_line_number;
        let end_line = ctx.line_number;

        let (tx, rx) = mpsc::channel(1);
        self.cmt.comment_submit_receiver = Some((pr_number, rx));
        self.cmt.comment_submitting = true;

        tokio::spawn(async move {
            let result = if let Some(start) = start_line {
                github::create_multiline_review_comment(
                    &repo, pr_number, &commit_id, &filename, start, end_line, "RIGHT", &body,
                )
                .await
            } else {
                github::create_review_comment(
                    &repo, pr_number, &commit_id, &filename, position, &body,
                )
                .await
            };

            let _ = tx
                .send(match result {
                    Ok(_) => CommentSubmitResult::Success,
                    Err(e) => CommentSubmitResult::Error(e.to_string()),
                })
                .await;
        });
    }

    fn next_local_comment_id(comments: &[ReviewComment]) -> u64 {
        comments.iter().map(|c| c.id).max().unwrap_or(0) + 1
    }

    fn submit_local_review_comment(&mut self, ctx: LineInputContext, body: String) {
        let Some(file) = self.files().get(ctx.file_index) else {
            return;
        };

        let mut comments = match load_local_review_comments(&self.repo, self.working_dir.as_deref())
        {
            Ok(comments) => comments,
            Err(e) => {
                self.cmt.submission_result =
                    Some((false, format!("Failed to load local comments: {}", e)));
                self.cmt.submission_result_time = Some(Instant::now());
                return;
            }
        };

        let next_id = Self::next_local_comment_id(&comments);
        comments.push(ReviewComment {
            id: next_id,
            path: file.filename.clone(),
            line: Some(ctx.line_number),
            body,
            user: github::User {
                login: Self::local_comment_author(),
            },
            created_at: Utc::now().to_rfc3339(),
            is_resolved: false,
            resolved_at: None,
        });

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

    fn submit_local_reply(&mut self, comment_id: u64, body: String) {
        let Some(parent) = self
            .cmt.review_comments
            .as_ref()
            .and_then(|comments: &Vec<ReviewComment>| comments.iter().find(|comment| comment.id == comment_id))
            .cloned()
        else {
            self.cmt.submission_result = Some((false, "Reply target not found".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        };

        let Some(line_number) = parent.line else {
            self.cmt.submission_result = Some((false, "Reply target has no line".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        };

        let mut comments = match load_local_review_comments(&self.repo, self.working_dir.as_deref())
        {
            Ok(comments) => comments,
            Err(e) => {
                self.cmt.submission_result =
                    Some((false, format!("Failed to load local comments: {}", e)));
                self.cmt.submission_result_time = Some(Instant::now());
                return;
            }
        };

        let next_id = Self::next_local_comment_id(&comments);
        comments.push(ReviewComment {
            id: next_id,
            path: parent.path,
            line: Some(line_number),
            body,
            user: github::User {
                login: Self::local_comment_author(),
            },
            created_at: Utc::now().to_rfc3339(),
            is_resolved: false,
            resolved_at: None,
        });

        self.persist_local_review_comments(comments, "Saved local reply");
    }

    fn persist_local_review_comments(
        &mut self,
        comments: Vec<ReviewComment>,
        success_message: &str,
    ) {
        if let Err(e) =
            save_local_review_comments(&self.repo, self.working_dir.as_deref(), &comments)
        {
            self.cmt.submission_result = Some((false, format!("Failed to save local comments: {}", e)));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        }

        let cache_key = PrCacheKey {
            repo: self.repo.clone(),
            pr_number: self.pr_number(),
        };
        self.session_cache
            .put_review_comments(cache_key, comments.clone());
        self.cmt.review_comments = Some(comments);
        self.cmt.selected_comment = self
            .cmt.review_comments
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
        self.cmt.submission_result = Some((true, success_message.to_string()));
        self.cmt.submission_result_time = Some(Instant::now());
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
            self.cmt.submission_result_time = None;
            PendingApproveChoice::Cancel
        } else {
            PendingApproveChoice::Ignore
        }
    }
}
