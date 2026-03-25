use anyhow::Result;
use crossterm::event::{self, KeyCode};
use tokio::sync::mpsc;

use crate::cache::PrCacheKey;
use crate::filter::ListFilter;
use crate::github::{self, PrStateFilter};
use crate::keybinding::{event_to_keybinding, SequenceMatch};

use crate::github::CiStatus;

use super::{App, AppState, DataState};

impl App {
    pub(crate) async fn handle_pr_list_input(&mut self, key: event::KeyEvent) -> Result<()> {
        let kb = self.config.keybindings.clone();

        if self.handle_filter_input(&key, "pr") {
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            self.should_quit = true;
            return Ok(());
        }

        if self.pr_list_loading {
            return Ok(());
        }

        let pr_count = self.pr_list.as_ref().map(|l| l.len()).unwrap_or(0);
        let has_filter = self.pr_list_filter.is_some();

        if self.matches_single_key(&key, &kb.move_down) {
            if has_filter {
                self.handle_filter_navigation("pr", true);
            } else if pr_count > 0 {
                self.selected_pr = (self.selected_pr + 1).min(pr_count.saturating_sub(1));
                if self.pr_list_has_more
                    && !self.pr_list_loading
                    && self.selected_pr + 5 >= pr_count
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
                self.selected_pr = self.selected_pr.saturating_sub(1);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_down) || Self::is_shift_char_shortcut(&key, 'j') {
            if pr_count > 0 && !has_filter {
                let step = 20usize;
                self.selected_pr = (self.selected_pr + step).min(pr_count.saturating_sub(1));
                if self.pr_list_has_more
                    && !self.pr_list_loading
                    && self.selected_pr + 5 >= pr_count
                {
                    self.load_more_prs();
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_up) || Self::is_shift_char_shortcut(&key, 'k') {
            if !has_filter {
                self.selected_pr = self.selected_pr.saturating_sub(20);
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
                        self.selected_pr = 0;
                    }
                    return Ok(());
                }

                if self.try_match_sequence(&kb.filter) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    if let Some(ref mut filter) = self.pr_list_filter {
                        filter.input_active = true;
                    } else {
                        let mut filter = ListFilter::new();
                        if let Some(prs) = self.pr_list.as_ref() {
                            filter.apply(prs, |_pr, _q| true);
                            if let Some(idx) = filter.sync_selection() {
                                self.selected_pr = idx;
                            }
                        }
                        self.pr_list_filter = Some(filter);
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
                self.selected_pr = pr_count.saturating_sub(1);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.open_panel) {
            if self.is_filter_selection_empty("pr") {
                return Ok(());
            }
            if let Some(ref prs) = self.pr_list {
                if let Some(pr) = prs.get(self.selected_pr) {
                    self.select_pr(pr.number);
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.open_in_browser) {
            if self.is_filter_selection_empty("pr") {
                return Ok(());
            }
            if let Some(ref prs) = self.pr_list {
                if let Some(pr) = prs.get(self.selected_pr) {
                    self.open_pr_in_browser(pr.number);
                }
            }
            return Ok(());
        }

        if key.code == KeyCode::Char('o') {
            if self.pr_list_state_filter != PrStateFilter::Open {
                self.pr_list_state_filter = PrStateFilter::Open;
                self.reload_pr_list();
            }
            return Ok(());
        }

        if key.code == KeyCode::Char('c') {
            if self.pr_list_state_filter != PrStateFilter::Closed {
                self.pr_list_state_filter = PrStateFilter::Closed;
                self.reload_pr_list();
            }
            return Ok(());
        }

        if key.code == KeyCode::Char('a') {
            if self.pr_list_state_filter != PrStateFilter::All {
                self.pr_list_state_filter = PrStateFilter::All;
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
            if let Some(ref prs) = self.pr_list {
                if let Some(pr) = prs.get(self.selected_pr) {
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
            self.previous_state = AppState::PullRequestList;
            self.state = AppState::Help;
            self.help_scroll_offset = 0;
            self.config_scroll_offset = 0;
            return Ok(());
        }

        Ok(())
    }
    pub(crate) fn reload_pr_list(&mut self) {
        self.selected_pr = 0;
        self.pr_list_scroll_offset = 0;
        self.pr_list_loading = true;
        self.pr_list = None;
        self.pr_list_has_more = false;
        self.pr_list_filter = None;

        let (tx, rx) = mpsc::channel(2);
        self.pr_list_receiver = Some(rx);

        let repo = self.repo.clone();
        let state = self.pr_list_state_filter;

        tokio::spawn(async move {
            let result = github::fetch_pr_list(&repo, state, 30).await;
            let _ = tx.send(result.map_err(|e| e.to_string())).await;
        });
    }

    /// 追加のPRを読み込み（無限スクロール用）
    pub(crate) fn load_more_prs(&mut self) {
        if self.pr_list_loading {
            return;
        }

        let offset = self.pr_list.as_ref().map(|l| l.len()).unwrap_or(0) as u32;

        self.pr_list_loading = true;

        let (tx, rx) = mpsc::channel(2);
        self.pr_list_receiver = Some(rx);

        let repo = self.repo.clone();
        let state = self.pr_list_state_filter;

        tokio::spawn(async move {
            let result = github::fetch_pr_list_with_offset(&repo, state, offset, 30).await;
            let _ = tx.send(result.map_err(|e| e.to_string())).await;
        });
    }
    pub(crate) fn select_pr(&mut self, pr_number: u32) {
        self.pr_number = Some(pr_number);
        self.state = AppState::FileList;
        self.file_list_filter = None;
        self.pending_approve_body = None;

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
        self.checks = None;
        self.checks_loading = false;
        self.checks_target_pr = None;
        self.checks_receiver = None;

        if let Some(ref prs) = self.pr_list {
            if let Some(pr_summary) = prs.iter().find(|p| p.number == pr_number) {
                let status = CiStatus::from_rollup(&pr_summary.status_check_rollup);
                self.ci_status = Some(status);
            } else {
                self.ci_status = None;
            }
        } else {
            self.ci_status = None;
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

        self.retry_load();
    }
    pub fn back_to_pr_list(&mut self) {
        if self.started_from_pr_list {
            if self.issue_detail_return {
                self.issue_detail_return = false;

                if self.local_mode {
                    self.deactivate_watcher();
                    self.local_mode = false;
                }

                self.pr_number = None;
                self.data_state = DataState::Loading;
                self.review_comments = None;
                self.discussion_comments = None;
                self.diff_store.clear();
                self.diff_scroll.reset();
                self.comment_receiver = None;
                self.discussion_comment_receiver = None;
                self.comment_submit_receiver = None;
                self.mark_viewed_receiver = None;
                self.batch_diff_receiver = None;
                self.lazy_diff_receiver = None;
                self.lazy_diff_pending_file = None;
                self.comment_submitting = false;
                self.pending_approve_body = None;
                self.comments_loading = false;
                self.discussion_comments_loading = false;
                self.selected_file = 0;
                self.file_list_scroll_offset = 0;
                self.file_list_filter = None;
                self.tree_mode_active = false;
                self.file_tree_state = None;
                self.checks = None;
                self.checks_loading = false;
                self.checks_target_pr = None;
                self.checks_receiver = None;
                self.ci_status = None;
                self.ci_status_receiver = None;
                self.state = AppState::IssueDetail;
                return;
            }

            if self.local_mode {
                self.deactivate_watcher();
                self.local_mode = false;
            }

            self.pr_number = None;
            self.data_state = DataState::Loading;
            self.review_comments = None;
            self.discussion_comments = None;
            self.diff_store.clear();
            self.diff_scroll.reset();
            // in-flight view 系レシーバーをクリア（late response による panic 防止）
            // data_receiver / retry_sender は永続のため維持
            self.comment_receiver = None;
            self.discussion_comment_receiver = None;
            self.comment_submit_receiver = None;
            self.mark_viewed_receiver = None;
            self.batch_diff_receiver = None;
            self.lazy_diff_receiver = None;
            self.lazy_diff_pending_file = None;
            self.comment_submitting = false;
            self.pending_approve_body = None;
            self.comments_loading = false;
            self.discussion_comments_loading = false;
            self.selected_file = 0;
            self.file_list_scroll_offset = 0;
            self.file_list_filter = None;
            self.tree_mode_active = false;
            self.file_tree_state = None;
            self.checks = None;
            self.checks_loading = false;
            self.checks_target_pr = None;
            self.checks_receiver = None;
            self.ci_status = None;
            self.ci_status_receiver = None;

            self.state = AppState::PullRequestList;
        }
    }
}
