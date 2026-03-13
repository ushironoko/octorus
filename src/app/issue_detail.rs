use anyhow::Result;
use crossterm::event::{self, KeyCode};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;

use crate::syntax::ParserPool;

use super::diff_cache::build_pr_description_patch;
use super::types::*;
use super::{App, AppState};

impl App {
    pub(crate) fn enter_pr_from_issue(&mut self, pr_number: u32) {
        self.issue_detail_return = true;
        self.select_pr(pr_number);
    }

    /// Issue body からキャッシュを構築（rebuild_pr_description_cache と同パターン）
    pub(crate) fn rebuild_issue_detail_cache(&mut self) {
        let Some(ref mut state) = self.issue_state else {
            return;
        };
        let body = state
            .issue_detail
            .as_ref()
            .and_then(|d| d.body.as_deref())
            .unwrap_or("")
            .to_string();

        let body_hash = hash_string(&body);
        let markdown_rich = self.markdown_rich;
        if let Some(ref cache) = state.issue_detail_cache {
            if cache.patch_hash == body_hash && cache.markdown_rich == markdown_rich {
                return;
            }
        }

        if body.is_empty() {
            state.issue_detail_cache = None;
            return;
        }

        let patch = build_pr_description_patch(&body);
        let tab_width = self.config.diff.tab_width;
        let theme = self.config.diff.theme.clone();

        let mut parser_pool = ParserPool::new();
        let mut cache = crate::ui::diff_view::build_diff_cache(
            &patch,
            "description.md",
            &theme,
            &mut parser_pool,
            markdown_rich,
            tab_width,
        );
        cache.file_index = usize::MAX;
        cache.patch_hash = body_hash;
        state.issue_detail_cache = Some(cache);
    }

    pub(crate) fn handle_issue_detail_input(
        &mut self,
        key: event::KeyEvent,
        _terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let kb = self.config.keybindings.clone();

        // Quit / back
        if self.matches_single_key(&key, &kb.quit) || key.code == KeyCode::Esc {
            self.state = AppState::IssueList;
            return Ok(());
        }

        let Some(ref state) = self.issue_state else {
            return Ok(());
        };

        // Loading中はquit以外受け付けない
        if state.issue_detail_loading {
            return Ok(());
        }

        let focus = state.detail_focus;

        match focus {
            IssueDetailFocus::Body => {
                // Body scroll
                if self.matches_single_key(&key, &kb.move_down) || key.code == KeyCode::Down {
                    let state = self.issue_state.as_mut().unwrap();
                    state.issue_detail_scroll_offset =
                        state.issue_detail_scroll_offset.saturating_add(1);
                    return Ok(());
                }
                if self.matches_single_key(&key, &kb.move_up) || key.code == KeyCode::Up {
                    let state = self.issue_state.as_mut().unwrap();
                    state.issue_detail_scroll_offset =
                        state.issue_detail_scroll_offset.saturating_sub(1);
                    return Ok(());
                }
                if self.matches_single_key(&key, &kb.page_down)
                    || Self::is_shift_char_shortcut(&key, 'j')
                {
                    let state = self.issue_state.as_mut().unwrap();
                    state.issue_detail_scroll_offset =
                        state.issue_detail_scroll_offset.saturating_add(20);
                    return Ok(());
                }
                if self.matches_single_key(&key, &kb.page_up)
                    || Self::is_shift_char_shortcut(&key, 'k')
                {
                    let state = self.issue_state.as_mut().unwrap();
                    state.issue_detail_scroll_offset =
                        state.issue_detail_scroll_offset.saturating_sub(20);
                    return Ok(());
                }
            }
            IssueDetailFocus::LinkedPrs => {
                let pr_count = state.linked_prs.as_ref().map(|p| p.len()).unwrap_or(0);

                if self.matches_single_key(&key, &kb.move_down) || key.code == KeyCode::Down {
                    if pr_count > 0 {
                        let state = self.issue_state.as_mut().unwrap();
                        state.selected_linked_pr =
                            (state.selected_linked_pr + 1).min(pr_count.saturating_sub(1));
                    }
                    return Ok(());
                }
                if self.matches_single_key(&key, &kb.move_up) || key.code == KeyCode::Up {
                    let state = self.issue_state.as_mut().unwrap();
                    state.selected_linked_pr = state.selected_linked_pr.saturating_sub(1);
                    return Ok(());
                }

                // Enter: linked PRを選択してPR viewに遷移
                if self.matches_single_key(&key, &kb.open_panel) {
                    let pr_number = state
                        .linked_prs
                        .as_ref()
                        .and_then(|prs| prs.get(state.selected_linked_pr))
                        .map(|pr| pr.number);
                    if let Some(number) = pr_number {
                        self.enter_pr_from_issue(number);
                    }
                    return Ok(());
                }
            }
        }

        // Tab: フォーカス切替（linked PRs が存在する場合のみ）
        if key.code == KeyCode::Tab {
            let has_linked_prs = state
                .linked_prs
                .as_ref()
                .map(|p| !p.is_empty())
                .unwrap_or(false);
            if has_linked_prs {
                let state = self.issue_state.as_mut().unwrap();
                state.detail_focus = match state.detail_focus {
                    IssueDetailFocus::Body => IssueDetailFocus::LinkedPrs,
                    IssueDetailFocus::LinkedPrs => IssueDetailFocus::Body,
                };
            }
            return Ok(());
        }

        // M: toggle markdown rich
        if self.matches_single_key(&key, &kb.toggle_markdown_rich) {
            self.markdown_rich = !self.markdown_rich;
            self.rebuild_issue_detail_cache();
            return Ok(());
        }

        // O: ブラウザで開く
        if self.matches_single_key(&key, &kb.open_in_browser) {
            if let Some(detail) = self
                .issue_state
                .as_ref()
                .and_then(|s| s.issue_detail.as_ref())
            {
                self.open_issue_in_browser(detail.number);
            }
            return Ok(());
        }

        // ?: ヘルプ
        if self.matches_single_key(&key, &kb.help) {
            self.previous_state = AppState::IssueDetail;
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

    #[test]
    fn test_enter_pr_from_issue_sets_return_flag() {
        let mut app = App::new_for_test();
        app.started_from_pr_list = true;
        app.issue_state = Some(IssueState::new());
        app.state = AppState::IssueDetail;
        app.enter_pr_from_issue(123);
        assert!(app.issue_detail_return);
        assert_eq!(app.state, AppState::FileList);
    }

    #[test]
    fn test_back_to_pr_list_returns_to_issue_detail_when_flag_set() {
        let mut app = App::new_for_test();
        app.started_from_pr_list = true;
        app.issue_detail_return = true;
        app.issue_state = Some(IssueState::new());
        app.state = AppState::FileList;
        app.back_to_pr_list();
        assert_eq!(app.state, AppState::IssueDetail);
        assert!(!app.issue_detail_return);
    }

    #[test]
    fn test_back_to_pr_list_normal_without_issue_flag() {
        let mut app = App::new_for_test();
        app.started_from_pr_list = true;
        app.issue_detail_return = false;
        app.state = AppState::FileList;
        app.back_to_pr_list();
        assert_eq!(app.state, AppState::PullRequestList);
    }

    #[test]
    fn test_back_to_issue_detail_clears_local_mode() {
        let mut app = App::new_for_test();
        app.started_from_pr_list = true;
        app.issue_detail_return = true;
        app.issue_state = Some(IssueState::new());
        app.state = AppState::FileList;
        app.local_mode = true;

        app.back_to_pr_list();

        assert_eq!(app.state, AppState::IssueDetail);
        assert!(!app.local_mode, "local_mode must be cleared when returning to IssueDetail");
        assert!(!app.issue_detail_return);
    }

}
