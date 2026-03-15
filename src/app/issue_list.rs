use anyhow::Result;
use crossterm::event::{self, KeyCode};
use tokio::sync::mpsc;

use crate::filter::ListFilter;
use crate::github::{self, IssueStateFilter};
use crate::keybinding::{event_to_keybinding, SequenceMatch};

use super::types::IssueState;
use super::{App, AppState};

impl App {
    pub fn open_issue_list(&mut self) {
        let mut state = IssueState::new();
        state.issue_list_loading = true;
        self.issue_state = Some(state);
        self.state = AppState::IssueList;
        self.reload_issue_list();
    }

    pub(crate) fn reload_issue_list(&mut self) {
        let Some(ref mut state) = self.issue_state else {
            return;
        };
        state.selected_issue = 0;
        state.issue_list_scroll_offset = 0;
        state.issue_list_loading = true;
        state.issue_list_has_more = false;
        state.issue_list_filter = None;
        state.issue_list_appending = false;

        let (tx, rx) = mpsc::channel(2);
        state.issue_list_receiver = Some(rx);

        let repo = self.repo.clone();
        let filter = state.issue_list_state_filter;

        tokio::spawn(async move {
            let result = github::fetch_issue_list(&repo, filter, 20).await;
            let _ = tx.send(result.map_err(|e| e.to_string())).await;
        });
    }

    pub(crate) fn load_more_issues(&mut self) {
        let Some(ref mut state) = self.issue_state else {
            return;
        };
        if state.issue_list_loading {
            return;
        }

        let offset = state.issues.as_ref().map(|l| l.len()).unwrap_or(0) as u32;
        state.issue_list_loading = true;
        state.issue_list_appending = true;

        let (tx, rx) = mpsc::channel(2);
        state.issue_list_receiver = Some(rx);

        let repo = self.repo.clone();
        let filter = state.issue_list_state_filter;

        tokio::spawn(async move {
            let result = github::fetch_issue_list_with_offset(&repo, filter, offset, 20).await;
            let _ = tx.send(result.map_err(|e| e.to_string())).await;
        });
    }

    pub fn select_issue(&mut self, issue_number: u32) {
        let Some(ref mut state) = self.issue_state else {
            return;
        };

        state.issue_detail = None;
        state.issue_detail_loading = true;
        state.issue_detail_scroll_offset = 0;
        state.issue_detail_cache = None;
        state.issue_comments = None;
        state.selected_issue_comment = 0;
        state.issue_comment_list_scroll_offset = 0;
        state.issue_comment_detail_mode = false;
        state.issue_comment_detail_scroll = 0;
        state.selected_linked_pr = 0;
        state.detail_focus = Default::default();
        state.linked_prs = None;
        state.linked_prs_loading = true;

        // Fetch issue detail
        let (detail_tx, detail_rx) = mpsc::channel(1);
        state.issue_detail_receiver = Some((issue_number, detail_rx));
        let repo = self.repo.clone();
        tokio::spawn(async move {
            let result = github::fetch_issue_detail(&repo, issue_number).await;
            let _ = detail_tx.send(result.map_err(|e| e.to_string())).await;
        });

        // Fetch linked PRs
        let (prs_tx, prs_rx) = mpsc::channel(1);
        state.linked_prs_receiver = Some((issue_number, prs_rx));
        let repo = self.repo.clone();
        tokio::spawn(async move {
            let result = github::fetch_linked_prs(&repo, issue_number).await;
            let _ = prs_tx.send(result.map_err(|e| e.to_string())).await;
        });

        self.state = AppState::IssueDetail;
    }

    pub(crate) fn open_issue_in_browser(&self, issue_number: u32) {
        let repo = self.repo.clone();
        tokio::spawn(async move {
            let _ = github::gh_command(&[
                "issue",
                "view",
                &issue_number.to_string(),
                "-R",
                &repo,
                "--web",
            ])
            .await;
        });
    }

    pub(crate) async fn handle_issue_list_input(&mut self, key: event::KeyEvent) -> Result<()> {
        let kb = self.config.keybindings.clone();

        // フィルタ入力中はフィルタ処理を優先
        if self.handle_filter_input(&key, "issue") {
            return Ok(());
        }

        // Quit / back
        if self.matches_single_key(&key, &kb.quit) || key.code == KeyCode::Esc {
            if self.handle_filter_esc("issue") {
                return Ok(());
            }
            self.issue_state = None;
            self.state = AppState::PullRequestList;
            return Ok(());
        }

        let Some(ref state) = self.issue_state else {
            return Ok(());
        };

        // ローディング中は操作を受け付けない
        if state.issue_list_loading && state.issues.is_none() {
            return Ok(());
        }

        let issue_count = state.issues.as_ref().map(|l| l.len()).unwrap_or(0);
        let has_filter = state.issue_list_filter.is_some();

        // Move down
        if self.matches_single_key(&key, &kb.move_down) || key.code == KeyCode::Down {
            if has_filter {
                self.handle_filter_navigation("issue", true);
            } else if issue_count > 0 {
                let needs_load_more = {
                    let state = self.issue_state.as_mut().unwrap();
                    state.selected_issue =
                        (state.selected_issue + 1).min(issue_count.saturating_sub(1));
                    state.issue_list_has_more
                        && !state.issue_list_loading
                        && state.selected_issue + 5 >= issue_count
                };
                if needs_load_more {
                    self.load_more_issues();
                }
            }
            return Ok(());
        }

        // Move up
        if self.matches_single_key(&key, &kb.move_up) || key.code == KeyCode::Up {
            if has_filter {
                self.handle_filter_navigation("issue", false);
            } else {
                let state = self.issue_state.as_mut().unwrap();
                state.selected_issue = state.selected_issue.saturating_sub(1);
            }
            return Ok(());
        }

        // Page down
        if self.matches_single_key(&key, &kb.page_down) || Self::is_shift_char_shortcut(&key, 'j') {
            if issue_count > 0 && !has_filter {
                let needs_load_more = {
                    let state = self.issue_state.as_mut().unwrap();
                    state.selected_issue =
                        (state.selected_issue + 20).min(issue_count.saturating_sub(1));
                    state.issue_list_has_more
                        && !state.issue_list_loading
                        && state.selected_issue + 5 >= issue_count
                };
                if needs_load_more {
                    self.load_more_issues();
                }
            }
            return Ok(());
        }

        // Page up
        if self.matches_single_key(&key, &kb.page_up) || Self::is_shift_char_shortcut(&key, 'k') {
            if !has_filter {
                let state = self.issue_state.as_mut().unwrap();
                state.selected_issue = state.selected_issue.saturating_sub(20);
            }
            return Ok(());
        }

        // gg/G/Space+/ シーケンス処理
        if let Some(kb_event) = event_to_keybinding(&key) {
            self.check_sequence_timeout();

            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);

                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    if !has_filter {
                        if let Some(ref mut state) = self.issue_state {
                            state.selected_issue = 0;
                        }
                    }
                    return Ok(());
                }

                if self.try_match_sequence(&kb.filter) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    let state = self.issue_state.as_mut().unwrap();
                    if let Some(ref mut filter) = state.issue_list_filter {
                        filter.input_active = true;
                    } else {
                        let mut filter = ListFilter::new();
                        if let Some(issues) = state.issues.as_ref() {
                            filter.apply(issues, |_issue, _q| true);
                            if let Some(idx) = filter.sync_selection() {
                                state.selected_issue = idx;
                            }
                        }
                        state.issue_list_filter = Some(filter);
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

        // G: 末尾へ
        if self.matches_single_key(&key, &kb.jump_to_last) {
            if issue_count > 0 && !has_filter {
                let state = self.issue_state.as_mut().unwrap();
                state.selected_issue = issue_count.saturating_sub(1);
            }
            return Ok(());
        }

        // Enter: Issue選択
        if self.matches_single_key(&key, &kb.open_panel) {
            if self.is_filter_selection_empty("issue") {
                return Ok(());
            }
            let issue_number = {
                let state = self.issue_state.as_ref().unwrap();
                state
                    .issues
                    .as_ref()
                    .and_then(|issues| issues.get(state.selected_issue))
                    .map(|i| i.number)
            };
            if let Some(number) = issue_number {
                self.select_issue(number);
            }
            return Ok(());
        }

        // ブラウザで開く
        if self.matches_single_key(&key, &kb.open_in_browser) {
            if self.is_filter_selection_empty("issue") {
                return Ok(());
            }
            let state = self.issue_state.as_ref().unwrap();
            if let Some(issues) = state.issues.as_ref() {
                if let Some(issue) = issues.get(state.selected_issue) {
                    self.open_issue_in_browser(issue.number);
                }
            }
            return Ok(());
        }

        // o: open のみ
        if key.code == KeyCode::Char('o') {
            let needs_reload = {
                let state = self.issue_state.as_mut().unwrap();
                if state.issue_list_state_filter != IssueStateFilter::Open {
                    state.issue_list_state_filter = IssueStateFilter::Open;
                    true
                } else {
                    false
                }
            };
            if needs_reload {
                self.reload_issue_list();
            }
            return Ok(());
        }

        // c: closed のみ
        if key.code == KeyCode::Char('c') {
            let needs_reload = {
                let state = self.issue_state.as_mut().unwrap();
                if state.issue_list_state_filter != IssueStateFilter::Closed {
                    state.issue_list_state_filter = IssueStateFilter::Closed;
                    true
                } else {
                    false
                }
            };
            if needs_reload {
                self.reload_issue_list();
            }
            return Ok(());
        }

        // a: all
        if key.code == KeyCode::Char('a') {
            let needs_reload = {
                let state = self.issue_state.as_mut().unwrap();
                if state.issue_list_state_filter != IssueStateFilter::All {
                    state.issue_list_state_filter = IssueStateFilter::All;
                    true
                } else {
                    false
                }
            };
            if needs_reload {
                self.reload_issue_list();
            }
            return Ok(());
        }

        // r: リフレッシュ
        if self.matches_single_key(&key, &kb.refresh) {
            self.reload_issue_list();
            return Ok(());
        }

        // ?: ヘルプ
        if self.matches_single_key(&key, &kb.help) {
            self.previous_state = AppState::IssueList;
            self.state = AppState::Help;
            self.help_scroll_offset = 0;
            self.config_scroll_offset = 0;
            return Ok(());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_open_issue_list_initializes_state() {
        let mut app = App::new_for_test();
        app.state = AppState::PullRequestList;
        app.open_issue_list();
        assert_eq!(app.state, AppState::IssueList);
        assert!(app.issue_state.is_some());
        let state = app.issue_state.as_ref().unwrap();
        assert!(state.issues.is_none());
        assert!(state.issue_list_loading);
    }

    #[test]
    fn test_issue_state_new_defaults() {
        let state = IssueState::new();
        assert!(state.issues.is_none());
        assert_eq!(state.selected_issue, 0);
        assert!(!state.issue_list_loading);
        assert!(state.linked_prs.is_none());
        assert!(!state.linked_prs_loading);
        assert!(state.issue_detail.is_none());
        assert!(!state.issue_detail_loading);
    }

    #[tokio::test]
    async fn test_select_issue_replaces_receivers_on_rapid_reselection() {
        let mut app = App::new_for_test();
        app.issue_state = Some(IssueState::new());
        app.state = AppState::IssueList;

        // Select issue A
        app.select_issue(100);
        let state = app.issue_state.as_ref().unwrap();
        assert_eq!(state.issue_detail_receiver.as_ref().unwrap().0, 100);
        assert_eq!(state.linked_prs_receiver.as_ref().unwrap().0, 100);

        // Rapidly select issue B before polling A's response
        app.select_issue(200);
        let state = app.issue_state.as_ref().unwrap();
        // Receivers should now track issue B, not A
        assert_eq!(state.issue_detail_receiver.as_ref().unwrap().0, 200);
        assert_eq!(state.linked_prs_receiver.as_ref().unwrap().0, 200);
        // Previous detail should be cleared
        assert!(state.issue_detail.is_none());
        assert!(state.linked_prs.is_none());
    }
}
