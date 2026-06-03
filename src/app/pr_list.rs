use anyhow::Result;
use crossterm::event;
use tokio::sync::mpsc;

use crate::cache::PrCacheKey;
use crate::filter::ListFilter;
use crate::github::{self, PrStateFilter};
use crate::keybinding::{event_to_keybinding, SequenceMatch};

use crate::github::CiStatus;

use super::types::LoadState;
use super::{App, AppState, DataState};

impl App {
    pub(crate) async fn handle_pr_list_input(&mut self, key: event::KeyEvent) -> Result<()> {
        let kb = self.config.keybindings.clone();

        if self.handle_filter_input(&key, "pr") {
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            if self.home_state == Some(AppState::Cockpit) {
                self.return_to_cockpit();
            } else {
                self.should_quit = true;
            }
            return Ok(());
        }

        if self.prs.pr_list.is_loading() {
            return Ok(());
        }

        let pr_count = self.prs.pr_list.as_loaded().map(|l| l.len()).unwrap_or(0);
        let has_filter = self.prs.pr_list_filter.is_some();

        if self.matches_single_key(&key, &kb.move_down) {
            if has_filter {
                self.handle_filter_navigation("pr", true);
            } else if pr_count > 0 {
                self.prs.selected_pr = (self.prs.selected_pr + 1).min(pr_count.saturating_sub(1));
                if self.prs.pr_list_has_more
                    && !self.prs.pr_list.is_loading()
                    && self.prs.selected_pr + 5 >= pr_count
                {
                    self.load_more_prs();
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_up) {
            if has_filter {
                self.handle_filter_navigation("pr", false);
            } else {
                self.prs.selected_pr = self.prs.selected_pr.saturating_sub(1);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_down) || Self::is_shift_char_shortcut(&key, 'j') {
            if pr_count > 0 && !has_filter {
                let step = 20usize;
                self.prs.selected_pr =
                    (self.prs.selected_pr + step).min(pr_count.saturating_sub(1));
                if self.prs.pr_list_has_more
                    && !self.prs.pr_list.is_loading()
                    && self.prs.selected_pr + 5 >= pr_count
                {
                    self.load_more_prs();
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_up) || Self::is_shift_char_shortcut(&key, 'k') {
            if !has_filter {
                self.prs.selected_pr = self.prs.selected_pr.saturating_sub(20);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) && self.handle_filter_esc("pr") {
            return Ok(());
        }

        if let Some(kb_event) = event_to_keybinding(&key) {
            self.check_sequence_timeout();

            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);

                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    if !has_filter {
                        self.prs.selected_pr = 0;
                    }
                    return Ok(());
                }

                if self.try_match_sequence(&kb.filter) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    if let Some(ref mut filter) = self.prs.pr_list_filter {
                        filter.input_active = true;
                    } else {
                        let mut filter = ListFilter::new();
                        if let Some(prs) = self.prs.pr_list.as_loaded() {
                            filter.apply(prs, |_pr, _q| true);
                            if let Some(idx) = filter.sync_selection() {
                                self.prs.selected_pr = idx;
                            }
                        }
                        self.prs.pr_list_filter = Some(filter);
                    }
                    return Ok(());
                }

                self.clear_pending_keys();
            } else if self.key_could_match_sequence(&key, &kb.jump_to_first)
                || self.key_could_match_sequence(&key, &kb.filter)
            {
                self.push_pending_key(kb_event);
                return Ok(());
            }
        }

        if self.matches_single_key(&key, &kb.jump_to_last) {
            if pr_count > 0 && !has_filter {
                self.prs.selected_pr = pr_count.saturating_sub(1);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.open_panel) {
            if self.is_filter_selection_empty("pr") {
                return Ok(());
            }
            if let Some(prs) = self.prs.pr_list.as_loaded() {
                if let Some(pr) = prs.get(self.prs.selected_pr) {
                    self.select_pr(pr.number);
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.open_in_browser) {
            if self.is_filter_selection_empty("pr") {
                return Ok(());
            }
            if let Some(prs) = self.prs.pr_list.as_loaded() {
                if let Some(pr) = prs.get(self.prs.selected_pr) {
                    self.open_pr_in_browser(pr.number);
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.filter_open) {
            if self.prs.pr_list_state_filter != PrStateFilter::Open {
                self.prs.pr_list_state_filter = PrStateFilter::Open;
                self.reload_pr_list();
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.filter_closed) {
            if self.prs.pr_list_state_filter != PrStateFilter::Closed {
                self.prs.pr_list_state_filter = PrStateFilter::Closed;
                self.reload_pr_list();
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.filter_all) {
            if self.prs.pr_list_state_filter != PrStateFilter::All {
                self.prs.pr_list_state_filter = PrStateFilter::All;
                self.reload_pr_list();
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.refresh) {
            self.reload_pr_list();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.toggle_local_mode) {
            self.toggle_local_mode();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.toggle_zen_mode) {
            self.toggle_zen_mode();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.ci_checks) {
            if self.is_filter_selection_empty("pr") {
                return Ok(());
            }
            if let Some(prs) = self.prs.pr_list.as_loaded() {
                if let Some(pr) = prs.get(self.prs.selected_pr) {
                    self.open_checks_list(pr.number);
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.issue_list) {
            self.open_issue_list();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.help) {
            self.open_help(AppState::PullRequestList);
            return Ok(());
        }

        Ok(())
    }
    pub(crate) fn reload_pr_list(&mut self) {
        self.prs.selected_pr = 0;
        self.prs.pr_list_scroll_offset = 0;
        self.prs.pr_list = LoadState::Loading;
        self.prs.pr_list_has_more = false;
        self.prs.pr_list_filter = None;

        let (tx, rx) = mpsc::channel(2);
        self.prs.pr_list_receiver = Some(rx);

        let repo = self.repo.clone();
        let state = self.prs.pr_list_state_filter;

        tokio::spawn(async move {
            let result = github::fetch_pr_list(&repo, state, 30).await;
            let _ = tx.send(result.map_err(|e| e.to_string())).await;
        });
    }

    /// Load next page of PRs for infinite scroll.
    pub(crate) fn load_more_prs(&mut self) {
        if self.prs.pr_list.is_loading() {
            return;
        }

        let offset = self.prs.pr_list.as_loaded().map(|l| l.len()).unwrap_or(0) as u32;
        let existing = std::mem::take(&mut self.prs.pr_list)
            .into_loaded()
            .unwrap_or_default();
        self.prs.pr_list = LoadState::LoadingMore(existing);

        let (tx, rx) = mpsc::channel(2);
        self.prs.pr_list_receiver = Some(rx);

        let repo = self.repo.clone();
        let state = self.prs.pr_list_state_filter;

        tokio::spawn(async move {
            let result = github::fetch_pr_list_with_offset(&repo, state, offset, 30).await;
            let _ = tx.send(result.map_err(|e| e.to_string())).await;
        });
    }
    pub(crate) fn select_pr(&mut self, pr_number: u32) {
        self.pr_number = Some(pr_number);
        self.state = AppState::FileList;
        self.file_list_filter = None;
        self.cmt.pending_approve_body = None;
        self.cmt.review_comments = None;
        self.cmt.file_comment_counts.clear();
        self.cmt.reset_threads();

        self.tree_mode_active = false;
        self.file_tree_state = None;

        self.diff_store.clear();
        self.diff_scroll.reset();
        self.mark_viewed_receiver = None;
        self.batch_diff_receiver = None;
        self.lazy_diff_receiver = None;
        self.lazy_diff_pending_file = None;
        self.selected_file = 0;
        self.file_list_scroll_offset = 0;
        self.chk.checks = None;
        self.chk.checks_loading = false;
        self.chk.checks_target_pr = None;
        self.chk.checks_receiver = None;

        if let Some(prs) = self.prs.pr_list.as_loaded() {
            if let Some(pr_summary) = prs.iter().find(|p| p.number == pr_number) {
                let status = CiStatus::from_rollup(&pr_summary.status_check_rollup);
                self.chk.ci_status = Some(status);
            } else {
                self.chk.ci_status = None;
            }
        } else {
            self.chk.ci_status = None;
        }

        if self.pending_ai_rally {
            self.start_ai_rally_on_load = true;
        }

        self.update_data_receiver_origin(pr_number);

        let cache_key = PrCacheKey {
            repo: self.repo.clone(),
            pr_number,
        };
        if let Some(cached) = self.session_cache.get_pr_data(&cache_key) {
            let diff_line_count = Self::calc_diff_line_count(&cached.files, 0);
            self.data_state = DataState::Loaded {
                pr: cached.pr.clone(),
                files: cached.files.clone(),
            };
            self.diff_scroll.line_count = diff_line_count;
            self.start_prefetch_all_files();
            if self.start_ai_rally_on_load {
                self.start_ai_rally_on_load = false;
                self.start_ai_rally();
            }
        } else {
            self.data_state = DataState::Loading;
        }

        // Eagerly load review comments in the background so they're
        // available by the time the user opens the comment list or file list.
        if !self.local_mode {
            self.load_review_comments();
        }

        self.retry_load();
    }
    /// Clear transient state accumulated in PR detail screens.
    /// data_receiver / retry_sender are long-lived and kept intact.
    fn reset_pr_detail_state(&mut self) {
        if self.local_mode {
            self.deactivate_watcher();
            self.local_mode = false;
        }

        self.pr_number = None;
        self.data_state = DataState::Loading;
        self.cmt.review_comments = None;
        self.cmt.discussion_comments = None;
        self.cmt.file_comment_counts.clear();
        self.cmt.reset_threads();
        self.diff_store.clear();
        self.diff_scroll.reset();
        self.cmt.comment_receiver = None;
        self.cmt.discussion_comment_receiver = None;
        self.cmt.comment_submit_receiver = None;
        self.mark_viewed_receiver = None;
        self.batch_diff_receiver = None;
        self.lazy_diff_receiver = None;
        self.lazy_diff_pending_file = None;
        self.cmt.comment_submitting = false;
        self.cmt.pending_approve_body = None;
        self.cmt.comments_loading = false;
        self.cmt.discussion_comments_loading = false;
        self.selected_file = 0;
        self.file_list_scroll_offset = 0;
        self.file_list_filter = None;
        self.tree_mode_active = false;
        self.file_tree_state = None;
        self.chk.checks = None;
        self.chk.checks_loading = false;
        self.chk.checks_target_pr = None;
        self.chk.checks_receiver = None;
        self.chk.ci_status = None;
        self.chk.ci_status_receiver = None;
    }

    pub fn back_to_pr_list(&mut self) {
        if self.started_from_pr_list {
            let return_to_issue = self.issue_detail_return;
            if return_to_issue {
                self.issue_detail_return = false;
            }
            self.reset_pr_detail_state();
            self.state = if return_to_issue {
                AppState::IssueDetail
            } else {
                AppState::PullRequestList
            };
        }
    }
}
