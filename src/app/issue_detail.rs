use anyhow::Result;
use crossterm::event::{self, KeyCode};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;

use crate::github::{self, parse_issue_comments};
use crate::syntax::ParserPool;

use super::diff_cache::build_pr_description_patch;
use super::types::*;
use super::{App, AppState};

impl App {
    /// Linked PR を開く。同一リポならPR viewに遷移、クロスリポならブラウザで開く
    pub(crate) fn enter_pr_from_issue(&mut self, pr_number: u32, pr_repo: Option<&str>) {
        if let Some(repo) = pr_repo {
            // クロスリポPR: 別リポのPRデータをキャッシュに混ぜないためブラウザで開く
            let repo = repo.to_string();
            let pr_number_str = pr_number.to_string();
            tokio::spawn(async move {
                let _ =
                    github::gh_command(&["pr", "view", &pr_number_str, "-R", &repo, "--web"]).await;
            });
        } else {
            self.issue_detail_return = true;
            self.select_pr(pr_number);
        }
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

                // Enter: linked PRを選択してPR viewに遷移（クロスリポはブラウザで開く）
                if self.matches_single_key(&key, &kb.open_panel) {
                    let pr_info = state
                        .linked_prs
                        .as_ref()
                        .and_then(|prs| prs.get(state.selected_linked_pr))
                        .map(|pr| (pr.number, pr.repo.clone()));
                    if let Some((number, repo)) = pr_info {
                        self.enter_pr_from_issue(number, repo.as_deref());
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

        // c: 新規コメント投稿
        if self.matches_single_key(&key, &kb.comment) {
            self.enter_issue_comment_input();
            return Ok(());
        }

        // C: コメントリスト
        if self.matches_single_key(&key, &kb.comment_list) {
            self.open_issue_comment_list();
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

    pub(crate) fn open_issue_comment_list(&mut self) {
        let Some(ref mut state) = self.issue_state else {
            return;
        };

        // 初回のみパース（キャッシュ再利用）
        if state.issue_comments.is_none() {
            let raw_comments = state
                .issue_detail
                .as_ref()
                .map(|d| d.comments.as_slice())
                .unwrap_or(&[]);
            state.issue_comments = Some(parse_issue_comments(raw_comments));
        }

        state.selected_issue_comment = 0;
        state.issue_comment_list_scroll_offset = 0;
        state.issue_comment_detail_mode = false;
        state.issue_comment_detail_scroll = 0;
        self.state = AppState::IssueCommentList;
    }

    pub(crate) fn enter_issue_comment_input(&mut self) {
        // 送信中はTextInput再オープンを防止
        if self.is_issue_comment_submitting() {
            return;
        }
        let Some(ref state) = self.issue_state else {
            return;
        };
        let Some(ref detail) = state.issue_detail else {
            return;
        };
        let issue_number = detail.number;
        self.input_mode = Some(InputMode::IssueComment { issue_number });
        self.input_text_area.clear();
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }

    pub(crate) fn enter_issue_reply_input(&mut self) {
        // 送信中はTextInput再オープンを防止
        if self.is_issue_comment_submitting() {
            return;
        }
        let Some(ref state) = self.issue_state else {
            return;
        };
        let Some(ref detail) = state.issue_detail else {
            return;
        };
        let Some(ref comments) = state.issue_comments else {
            return;
        };
        let Some(comment) = comments.get(state.selected_issue_comment) else {
            return;
        };

        let issue_number = detail.number;
        // 引用テンプレート（長文は先頭3行に制限）
        let quote_lines: Vec<&str> = comment.body.lines().take(3).collect();
        let quote = quote_lines.join("\n> ");
        let ellipsis = if comment.body.lines().count() > 3 {
            "\n> ..."
        } else {
            ""
        };
        let template = format!(
            "> @{} wrote:\n> {}{}\n\n",
            comment.author.login, quote, ellipsis
        );

        self.input_mode = Some(InputMode::IssueComment { issue_number });
        self.input_text_area.clear();
        self.input_text_area.set_content(&template);
        self.input_text_area.move_to_end();
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }

    pub(crate) fn handle_issue_comment_list_input(&mut self, key: event::KeyEvent) -> Result<()> {
        let kb = self.config.keybindings.clone();

        let Some(ref state) = self.issue_state else {
            return Ok(());
        };

        // Detail mode は別ハンドラに委譲
        if state.issue_comment_detail_mode {
            return self.handle_issue_comment_detail_input(key);
        }

        // Quit / back → IssueDetail に戻る
        if self.matches_single_key(&key, &kb.quit) || key.code == KeyCode::Esc {
            self.state = AppState::IssueDetail;
            return Ok(());
        }

        let comment_count = state.issue_comments.as_ref().map(|c| c.len()).unwrap_or(0);

        // Move down
        if self.matches_single_key(&key, &kb.move_down) || key.code == KeyCode::Down {
            if comment_count > 0 {
                let state = self.issue_state.as_mut().unwrap();
                state.selected_issue_comment =
                    (state.selected_issue_comment + 1).min(comment_count.saturating_sub(1));
            }
            return Ok(());
        }

        // Move up
        if self.matches_single_key(&key, &kb.move_up) || key.code == KeyCode::Up {
            let state = self.issue_state.as_mut().unwrap();
            state.selected_issue_comment = state.selected_issue_comment.saturating_sub(1);
            return Ok(());
        }

        // Page down
        if self.matches_single_key(&key, &kb.page_down) || Self::is_shift_char_shortcut(&key, 'j') {
            if comment_count > 0 {
                let state = self.issue_state.as_mut().unwrap();
                state.selected_issue_comment =
                    (state.selected_issue_comment + 20).min(comment_count.saturating_sub(1));
            }
            return Ok(());
        }

        // Page up
        if self.matches_single_key(&key, &kb.page_up) || Self::is_shift_char_shortcut(&key, 'k') {
            let state = self.issue_state.as_mut().unwrap();
            state.selected_issue_comment = state.selected_issue_comment.saturating_sub(20);
            return Ok(());
        }

        // g: 先頭へ
        if self.matches_single_key(&key, &kb.jump_to_last) {
            // G → 末尾
            if comment_count > 0 {
                let state = self.issue_state.as_mut().unwrap();
                state.selected_issue_comment = comment_count.saturating_sub(1);
            }
            return Ok(());
        }

        // Enter: detail mode
        if self.matches_single_key(&key, &kb.open_panel) {
            if comment_count > 0 {
                let state = self.issue_state.as_mut().unwrap();
                state.issue_comment_detail_mode = true;
                state.issue_comment_detail_scroll = 0;
            }
            return Ok(());
        }

        // O: ブラウザで開く
        if self.matches_single_key(&key, &kb.open_in_browser) {
            let url = state
                .issue_comments
                .as_ref()
                .and_then(|c| c.get(state.selected_issue_comment))
                .map(|c| c.url.clone())
                .filter(|u| !u.is_empty());
            if let Some(url) = url {
                Self::open_url_in_browser(&url);
            } else if let Some(detail) = state.issue_detail.as_ref() {
                self.open_issue_in_browser(detail.number);
            }
            return Ok(());
        }

        // c: 新規コメント投稿
        if self.matches_single_key(&key, &kb.comment) {
            self.enter_issue_comment_input();
            return Ok(());
        }

        // ?: ヘルプ
        if self.matches_single_key(&key, &kb.help) {
            self.previous_state = AppState::IssueCommentList;
            self.state = AppState::Help;
            self.help_scroll_offset = 0;
            self.config_scroll_offset = 0;
            return Ok(());
        }

        Ok(())
    }

    fn handle_issue_comment_detail_input(&mut self, key: event::KeyEvent) -> Result<()> {
        let kb = self.config.keybindings.clone();

        // Quit / Esc / Enter: detail を閉じてリストに戻る
        if self.matches_single_key(&key, &kb.quit)
            || key.code == KeyCode::Esc
            || self.matches_single_key(&key, &kb.open_panel)
        {
            let state = self.issue_state.as_mut().unwrap();
            state.issue_comment_detail_mode = false;
            return Ok(());
        }

        // r: リプライ（detail mode で選択中のコメントに返信）
        if self.matches_single_key(&key, &kb.reply) {
            self.enter_issue_reply_input();
            return Ok(());
        }

        // Scroll down
        if self.matches_single_key(&key, &kb.move_down) || key.code == KeyCode::Down {
            let state = self.issue_state.as_mut().unwrap();
            state.issue_comment_detail_scroll = state.issue_comment_detail_scroll.saturating_add(1);
            return Ok(());
        }

        // Scroll up
        if self.matches_single_key(&key, &kb.move_up) || key.code == KeyCode::Up {
            let state = self.issue_state.as_mut().unwrap();
            state.issue_comment_detail_scroll = state.issue_comment_detail_scroll.saturating_sub(1);
            return Ok(());
        }

        // Page down
        if self.matches_single_key(&key, &kb.page_down) || Self::is_shift_char_shortcut(&key, 'j') {
            let state = self.issue_state.as_mut().unwrap();
            state.issue_comment_detail_scroll =
                state.issue_comment_detail_scroll.saturating_add(20);
            return Ok(());
        }

        // Page up
        if self.matches_single_key(&key, &kb.page_up) || Self::is_shift_char_shortcut(&key, 'k') {
            let state = self.issue_state.as_mut().unwrap();
            state.issue_comment_detail_scroll =
                state.issue_comment_detail_scroll.saturating_sub(20);
            return Ok(());
        }

        // Ctrl+d: half page down
        if key.code == KeyCode::Char('d') && key.modifiers.contains(event::KeyModifiers::CONTROL) {
            let state = self.issue_state.as_mut().unwrap();
            state.issue_comment_detail_scroll =
                state.issue_comment_detail_scroll.saturating_add(10);
            return Ok(());
        }

        // Ctrl+u: half page up
        if key.code == KeyCode::Char('u') && key.modifiers.contains(event::KeyModifiers::CONTROL) {
            let state = self.issue_state.as_mut().unwrap();
            state.issue_comment_detail_scroll =
                state.issue_comment_detail_scroll.saturating_sub(10);
            return Ok(());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::IssueDetail;

    fn make_issue_detail_with_comments(comments: Vec<serde_json::Value>) -> IssueDetail {
        IssueDetail {
            number: 42,
            title: "Test issue".to_string(),
            body: Some("Test body".to_string()),
            state: "OPEN".to_string(),
            author: crate::github::User {
                login: "test".to_string(),
            },
            labels: vec![],
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            comments,
        }
    }

    #[test]
    fn test_open_issue_comment_list_transitions_state() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        app.issue_state = Some(state);
        app.state = AppState::IssueDetail;

        app.open_issue_comment_list();

        assert_eq!(app.state, AppState::IssueCommentList);
    }

    #[test]
    fn test_open_issue_comment_list_parses_comments() {
        let mut app = App::new_for_test();
        let comments = vec![
            serde_json::json!({
                "id": "IC_1",
                "body": "Hello",
                "author": {"login": "user1"},
                "createdAt": "2026-01-01T00:00:00Z"
            }),
            serde_json::json!({
                "id": "IC_2",
                "body": "World",
                "author": {"login": "user2"},
                "createdAt": "2026-01-02T00:00:00Z"
            }),
        ];
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(comments));
        app.issue_state = Some(state);
        app.state = AppState::IssueDetail;

        app.open_issue_comment_list();

        let state = app.issue_state.as_ref().unwrap();
        let parsed = state.issue_comments.as_ref().unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].body, "Hello");
        assert_eq!(parsed[1].body, "World");
    }

    #[test]
    fn test_issue_comment_list_quit_returns_to_issue_detail() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        state.issue_comments = Some(vec![]);
        app.issue_state = Some(state);
        app.state = AppState::IssueCommentList;

        let key = crossterm::event::KeyEvent::new(KeyCode::Char('q'), event::KeyModifiers::NONE);
        app.handle_issue_comment_list_input(key).unwrap();

        assert_eq!(app.state, AppState::IssueDetail);
    }

    #[test]
    fn test_issue_comment_list_enter_activates_detail_mode() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        state.issue_comments = Some(vec![crate::github::IssueComment {
            id: "IC_1".to_string(),
            body: "Test".to_string(),
            author: crate::github::User {
                login: "user1".to_string(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            author_association: "OWNER".to_string(),
            url: String::new(),
        }]);
        app.issue_state = Some(state);
        app.state = AppState::IssueCommentList;

        let key = crossterm::event::KeyEvent::new(KeyCode::Enter, event::KeyModifiers::NONE);
        app.handle_issue_comment_list_input(key).unwrap();

        let state = app.issue_state.as_ref().unwrap();
        assert!(state.issue_comment_detail_mode);
    }

    #[test]
    fn test_issue_comment_detail_quit_returns_to_list_mode() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        state.issue_comments = Some(vec![crate::github::IssueComment {
            id: "IC_1".to_string(),
            body: "Test".to_string(),
            author: crate::github::User {
                login: "user1".to_string(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            author_association: "OWNER".to_string(),
            url: String::new(),
        }]);
        state.issue_comment_detail_mode = true;
        app.issue_state = Some(state);
        app.state = AppState::IssueCommentList;

        let key = crossterm::event::KeyEvent::new(KeyCode::Esc, event::KeyModifiers::NONE);
        app.handle_issue_comment_list_input(key).unwrap();

        let state = app.issue_state.as_ref().unwrap();
        assert!(!state.issue_comment_detail_mode);
    }

    #[test]
    fn test_open_issue_comment_list_with_zero_comments() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        app.issue_state = Some(state);
        app.state = AppState::IssueDetail;

        app.open_issue_comment_list();

        assert_eq!(app.state, AppState::IssueCommentList);
        let state = app.issue_state.as_ref().unwrap();
        assert!(state.issue_comments.as_ref().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_select_issue_resets_comment_state() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_comments = Some(vec![]);
        state.selected_issue_comment = 5;
        state.issue_comment_detail_mode = true;
        state.issue_comment_detail_scroll = 10;
        state.issue_comment_list_scroll_offset = 3;
        app.issue_state = Some(state);
        app.state = AppState::IssueList;

        app.select_issue(100);

        let state = app.issue_state.as_ref().unwrap();
        assert!(state.issue_comments.is_none());
        assert_eq!(state.selected_issue_comment, 0);
        assert!(!state.issue_comment_detail_mode);
        assert_eq!(state.issue_comment_detail_scroll, 0);
        assert_eq!(state.issue_comment_list_scroll_offset, 0);
    }

    #[test]
    fn test_enter_issue_comment_input_sets_input_mode() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        app.issue_state = Some(state);
        app.state = AppState::IssueDetail;

        app.enter_issue_comment_input();

        assert_eq!(app.state, AppState::TextInput);
        assert!(matches!(
            app.input_mode,
            Some(InputMode::IssueComment { issue_number: 42 })
        ));
        assert_eq!(app.preview_return_state, AppState::IssueDetail);
    }

    #[test]
    fn test_enter_issue_reply_input_sets_template() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        state.issue_comments = Some(vec![crate::github::IssueComment {
            id: "IC_1".to_string(),
            body: "Original comment".to_string(),
            author: crate::github::User {
                login: "author1".to_string(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            author_association: String::new(),
            url: String::new(),
        }]);
        app.issue_state = Some(state);
        app.state = AppState::IssueCommentList;

        app.enter_issue_reply_input();

        assert_eq!(app.state, AppState::TextInput);
        let content = app.input_text_area.content();
        assert!(content.contains("> @author1 wrote:"));
        assert!(content.contains("> Original comment"));
    }

    #[test]
    fn test_enter_issue_reply_input_truncates_long_quote() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        state.issue_comments = Some(vec![crate::github::IssueComment {
            id: "IC_1".to_string(),
            body: "line1\nline2\nline3\nline4\nline5".to_string(),
            author: crate::github::User {
                login: "u".to_string(),
            },
            created_at: String::new(),
            author_association: String::new(),
            url: String::new(),
        }]);
        app.issue_state = Some(state);
        app.state = AppState::IssueCommentList;

        app.enter_issue_reply_input();

        let content = app.input_text_area.content();
        assert!(content.contains("> ..."), "should contain ellipsis");
        assert!(!content.contains("line4"), "line4 should be truncated");
    }

    #[test]
    fn test_enter_pr_from_issue_sets_return_flag() {
        let mut app = App::new_for_test();
        app.started_from_pr_list = true;
        app.issue_state = Some(IssueState::new());
        app.state = AppState::IssueDetail;
        app.enter_pr_from_issue(123, None);
        assert!(app.issue_detail_return);
        assert_eq!(app.state, AppState::FileList);
    }

    #[tokio::test]
    async fn test_enter_pr_from_issue_cross_repo_does_not_set_return_flag() {
        let mut app = App::new_for_test();
        app.started_from_pr_list = true;
        app.issue_state = Some(IssueState::new());
        app.state = AppState::IssueDetail;
        app.enter_pr_from_issue(456, Some("other/repo"));
        // クロスリポPRはブラウザで開くため、issue_detail_returnは設定されない
        assert!(!app.issue_detail_return);
        assert_eq!(app.state, AppState::IssueDetail);
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
        assert!(
            !app.local_mode,
            "local_mode must be cleared when returning to IssueDetail"
        );
        assert!(!app.issue_detail_return);
    }

    #[test]
    fn test_reply_key_works_in_detail_mode() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        state.issue_comments = Some(vec![crate::github::IssueComment {
            id: "IC_1".to_string(),
            body: "Original".to_string(),
            author: crate::github::User {
                login: "author1".to_string(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            author_association: String::new(),
            url: String::new(),
        }]);
        state.issue_comment_detail_mode = true;
        app.issue_state = Some(state);
        app.state = AppState::IssueCommentList;

        // Press 'r' (default reply key) in detail mode
        let key = crossterm::event::KeyEvent::new(KeyCode::Char('r'), event::KeyModifiers::NONE);
        app.handle_issue_comment_list_input(key).unwrap();

        // Should transition to TextInput with reply template
        assert_eq!(app.state, AppState::TextInput);
        assert!(matches!(
            app.input_mode,
            Some(InputMode::IssueComment { issue_number: 42 })
        ));
        let content = app.input_text_area.content();
        assert!(
            content.contains("> @author1 wrote:"),
            "reply template should be set"
        );
    }

    #[test]
    fn test_enter_issue_comment_blocked_during_submit() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        state.issue_comment_submitting = true;
        app.issue_state = Some(state);
        app.state = AppState::IssueDetail;

        app.enter_issue_comment_input();

        // Should NOT transition to TextInput
        assert_eq!(app.state, AppState::IssueDetail);
        assert!(app.input_mode.is_none());
    }

    #[test]
    fn test_enter_issue_reply_blocked_during_submit() {
        let mut app = App::new_for_test();
        let mut state = IssueState::new();
        state.issue_detail = Some(make_issue_detail_with_comments(vec![]));
        state.issue_comments = Some(vec![crate::github::IssueComment {
            id: "IC_1".to_string(),
            body: "Test".to_string(),
            author: crate::github::User {
                login: "u".to_string(),
            },
            created_at: String::new(),
            author_association: String::new(),
            url: String::new(),
        }]);
        state.issue_comment_submitting = true;
        app.issue_state = Some(state);
        app.state = AppState::IssueCommentList;

        app.enter_issue_reply_input();

        // Should NOT transition to TextInput
        assert_eq!(app.state, AppState::IssueCommentList);
        assert!(app.input_mode.is_none());
    }
}
