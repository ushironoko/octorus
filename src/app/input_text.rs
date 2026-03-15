use anyhow::Result;
use crossterm::event::{self, KeyCode, KeyEvent};
use tokio::sync::mpsc;

use crate::github;
use crate::loader::CommentSubmitResult;
use crate::ui::text_area::TextAreaAction;

use super::types::*;
use super::App;

impl App {
    pub(crate) fn handle_text_input(&mut self, key: event::KeyEvent) -> Result<()> {
        // 送信中は入力を無視（各送信種別に対応するInputModeのみブロック）
        if self.comment_submitting {
            return Ok(());
        }
        if self.is_issue_comment_submitting() {
            if matches!(self.input_mode, Some(InputMode::IssueComment { .. })) {
                return Ok(());
            }
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
            TextAreaAction::Continue => {}
            TextAreaAction::PendingSequence => {
                // Waiting for more keys in a sequence, do nothing
            }
        }
        Ok(())
    }
    pub(crate) fn cancel_input(&mut self) {
        self.input_mode = None;
        self.input_text_area.clear();
        self.state = self.preview_return_state;
    }
    pub(crate) fn submit_comment(&mut self, ctx: LineInputContext, body: String) {
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
        self.comment_submit_receiver = Some((pr_number, rx));
        self.comment_submitting = true;

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

    pub(crate) fn submit_suggestion(&mut self, ctx: LineInputContext, suggested_code: String) {
        let Some(file) = self.files().get(ctx.file_index) else {
            return;
        };
        let Some(pr) = self.pr() else {
            return;
        };

        let commit_id = pr.head.sha.clone();
        let filename = file.filename.clone();
        let body = format!("```suggestion\n{}\n```", suggested_code.trim_end());
        let repo = self.repo.clone();
        let pr_number = self.pr_number();
        let position = ctx.diff_position;
        let start_line = ctx.start_line_number;
        let end_line = ctx.line_number;

        let (tx, rx) = mpsc::channel(1);
        self.comment_submit_receiver = Some((pr_number, rx));
        self.comment_submitting = true;

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

    pub(crate) fn submit_reply(&mut self, comment_id: u64, body: String) {
        let repo = self.repo.clone();
        let pr_number = self.pr_number();

        let (tx, rx) = mpsc::channel(1);
        self.comment_submit_receiver = Some((pr_number, rx));
        self.comment_submitting = true;

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
    pub(crate) fn is_issue_comment_submitting(&self) -> bool {
        self.issue_state
            .as_ref()
            .map_or(false, |s| s.issue_comment_submitting)
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
        if self.pending_approve_body.is_none() {
            return PendingApproveChoice::Ignore;
        }
        if self.matches_single_key(key, &self.config.keybindings.approve) {
            PendingApproveChoice::Submit
        } else if self.matches_single_key(key, &self.config.keybindings.quit)
            || key.code == KeyCode::Esc
        {
            self.pending_approve_body = None;
            self.submission_result = None;
            self.submission_result_time = None;
            PendingApproveChoice::Cancel
        } else {
            PendingApproveChoice::Ignore
        }
    }
}
