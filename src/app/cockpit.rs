use anyhow::Result;
use crossterm::event;
use tokio::sync::mpsc;

use crate::github;

use super::types::{CockpitMenuItem, LoadState, PrListState};
use super::{App, AppState, DataState};

impl App {
    pub(crate) fn handle_cockpit_input(&mut self, key: event::KeyEvent) -> Result<()> {
        let kb = &self.config.keybindings;

        if self.matches_single_key(&key, &kb.quit) {
            self.should_quit = true;
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.help) {
            self.previous_state = AppState::Cockpit;
            self.state = AppState::Help;
            self.help_scroll_offset = 0;
            self.config_scroll_offset = 0;
            return Ok(());
        }

        let is_move_down = self.matches_single_key(&key, &kb.move_down);
        let is_move_up = self.matches_single_key(&key, &kb.move_up);
        let is_retry = self.matches_single_key(&key, &kb.retry);
        let is_open = self.matches_single_key(&key, &kb.open_panel);

        let Some(ref cockpit) = self.cockpit_state else {
            return Ok(());
        };
        let selected = cockpit.selected_item;
        let repo_available = cockpit.repo_available;

        if is_move_down {
            self.cockpit_state.as_mut().unwrap().selected_item = selected.next();
            return Ok(());
        }

        if is_move_up {
            self.cockpit_state.as_mut().unwrap().selected_item = selected.prev();
            return Ok(());
        }

        if is_retry && repo_available {
            self.reload_cockpit_counts();
            return Ok(());
        }

        if is_open {
            if selected.requires_repo() && !repo_available {
                return Ok(());
            }

            match selected {
                CockpitMenuItem::PrList => self.open_pr_list_from_cockpit(),
                CockpitMenuItem::IssueList => self.open_issue_list(),
                CockpitMenuItem::LocalDiff => self.enter_local_mode_from_cockpit(),
                CockpitMenuItem::GitOps => self.open_git_ops(),
            }
            return Ok(());
        }

        Ok(())
    }

    /// Single entry point for all cockpit return paths to prevent partial resets.
    pub(crate) fn return_to_cockpit(&mut self) {
        self.state = AppState::Cockpit;
        self.pr_number = None;
        self.local_mode = false;
        self.deactivate_watcher();
        self.started_from_pr_list = false;
        self.selected_file = 0;
        self.file_list_scroll_offset = 0;
        self.diff_scroll.reset();
        self.diff_store.clear();
        self.file_list_filter = None;
        self.issue_state = None;
        self.git_ops_state = None;
    }

    pub fn open_cockpit(&mut self) {
        debug_assert!(
            self.cockpit_state.is_some(),
            "open_cockpit requires prior new_cockpit initialization"
        );

        let repo_available = self
            .cockpit_state
            .as_ref()
            .map(|s| s.repo_available)
            .unwrap_or(false);

        self.state = AppState::Cockpit;

        if repo_available {
            self.reload_cockpit_counts();
        }
    }

    pub(crate) fn reload_cockpit_counts(&mut self) {
        let Some(ref mut cockpit) = self.cockpit_state else {
            return;
        };
        if !cockpit.repo_available {
            return;
        }

        cockpit.mentioned_issues_count = LoadState::Loading;
        cockpit.review_prs_count = LoadState::Loading;

        let repo = self.repo.clone();

        let (mention_tx, mention_rx) = mpsc::channel(1);
        cockpit.mentioned_receiver = Some(mention_rx);

        let repo_for_mention = repo.clone();
        tokio::spawn(async move {
            let result = github::fetch_mentioned_issues_count(&repo_for_mention)
                .await
                .map_err(|e| e.to_string());
            let _ = mention_tx.send(result).await;
        });

        let (review_tx, review_rx) = mpsc::channel(1);
        cockpit.review_receiver = Some(review_rx);

        tokio::spawn(async move {
            let result = github::fetch_review_requested_prs_count(&repo)
                .await
                .map_err(|e| e.to_string());
            let _ = review_tx.send(result).await;
        });
    }

    pub(crate) fn poll_cockpit_updates(&mut self) {
        let Some(ref mut cockpit) = self.cockpit_state else {
            return;
        };

        if let Some(ref mut rx) = cockpit.mentioned_receiver {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(count) => cockpit.mentioned_issues_count = LoadState::Loaded(count),
                    Err(msg) => cockpit.mentioned_issues_count = LoadState::Error(msg),
                }
                cockpit.mentioned_receiver = None;
            }
        }

        if let Some(ref mut rx) = cockpit.review_receiver {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(count) => cockpit.review_prs_count = LoadState::Loaded(count),
                    Err(msg) => cockpit.review_prs_count = LoadState::Error(msg),
                }
                cockpit.review_receiver = None;
            }
        }
    }

    pub(crate) fn open_pr_list_from_cockpit(&mut self) {
        self.state = AppState::PullRequestList;
        self.prs = PrListState::default();
        self.prs.pr_list = LoadState::Loading;
        self.started_from_pr_list = true;
        self.reload_pr_list();
    }

    pub(crate) fn enter_local_mode_from_cockpit(&mut self) {
        self.local_mode = true;
        self.pr_number = Some(0);
        self.state = AppState::FileList;
        self.data_state = DataState::Loading;
        self.update_data_receiver_origin(0);
        self.activate_watcher();
        self.retry_load();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_cockpit_app() -> App {
        let config = crate::config::Config::default();
        App::new_cockpit("owner/repo", config, true)
    }

    fn make_cockpit_app_local_only() -> App {
        let config = crate::config::Config::default();
        App::new_cockpit("local", config, false)
    }

    #[tokio::test]
    async fn cockpit_navigate_down() {
        let mut app = make_cockpit_app();
        assert_eq!(
            app.cockpit_state.as_ref().unwrap().selected_item,
            CockpitMenuItem::PrList
        );

        app.handle_cockpit_input(press(KeyCode::Char('j'))).unwrap();
        assert_eq!(
            app.cockpit_state.as_ref().unwrap().selected_item,
            CockpitMenuItem::IssueList
        );

        app.handle_cockpit_input(press(KeyCode::Char('j'))).unwrap();
        assert_eq!(
            app.cockpit_state.as_ref().unwrap().selected_item,
            CockpitMenuItem::LocalDiff
        );

        app.handle_cockpit_input(press(KeyCode::Char('j'))).unwrap();
        assert_eq!(
            app.cockpit_state.as_ref().unwrap().selected_item,
            CockpitMenuItem::GitOps
        );

        // Clamp at last
        app.handle_cockpit_input(press(KeyCode::Char('j'))).unwrap();
        assert_eq!(
            app.cockpit_state.as_ref().unwrap().selected_item,
            CockpitMenuItem::GitOps
        );
    }

    #[tokio::test]
    async fn cockpit_navigate_up() {
        let mut app = make_cockpit_app();
        // Move to last item first
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::GitOps;

        app.handle_cockpit_input(press(KeyCode::Char('k'))).unwrap();
        assert_eq!(
            app.cockpit_state.as_ref().unwrap().selected_item,
            CockpitMenuItem::LocalDiff
        );

        // Clamp at first
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::PrList;
        app.handle_cockpit_input(press(KeyCode::Char('k'))).unwrap();
        assert_eq!(
            app.cockpit_state.as_ref().unwrap().selected_item,
            CockpitMenuItem::PrList
        );
    }

    #[tokio::test]
    async fn cockpit_quit() {
        let mut app = make_cockpit_app();
        assert!(!app.should_quit);

        app.handle_cockpit_input(press(KeyCode::Char('q'))).unwrap();
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn cockpit_help() {
        let mut app = make_cockpit_app();
        app.handle_cockpit_input(press(KeyCode::Char('?'))).unwrap();
        assert_eq!(app.state, AppState::Help);
        assert_eq!(app.previous_state, AppState::Cockpit);
    }

    #[tokio::test]
    async fn cockpit_enter_pr_list() {
        let mut app = make_cockpit_app();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::PrList;
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::PullRequestList);
    }

    #[tokio::test]
    async fn cockpit_enter_issue_list() {
        let mut app = make_cockpit_app();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::IssueList;
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::IssueList);
    }

    #[tokio::test]
    async fn cockpit_enter_local_diff() {
        let mut app = make_cockpit_app();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::LocalDiff;
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::FileList);
    }

    #[tokio::test]
    async fn cockpit_enter_git_ops() {
        let mut app = make_cockpit_app();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::GitOps;
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::GitOpsSplitTree);
    }

    #[tokio::test]
    async fn cockpit_local_only_blocks_pr_list() {
        let mut app = make_cockpit_app_local_only();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::PrList;
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::Cockpit);
    }

    #[tokio::test]
    async fn cockpit_local_only_blocks_issue_list() {
        let mut app = make_cockpit_app_local_only();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::IssueList;
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::Cockpit);
    }

    #[tokio::test]
    async fn cockpit_local_only_allows_local_diff() {
        let mut app = make_cockpit_app_local_only();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::LocalDiff;
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::FileList);
    }

    #[tokio::test]
    async fn cockpit_local_only_allows_git_ops() {
        let mut app = make_cockpit_app_local_only();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::GitOps;
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::GitOpsSplitTree);
    }

    // Scenario tests: round-trip navigation

    #[tokio::test]
    async fn scenario_cockpit_pr_list_return() {
        let mut app = make_cockpit_app();
        // Cockpit → PR List
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::PullRequestList);
        // PR List → q → Cockpit
        app.handle_pr_list_input(press(KeyCode::Char('q'))).await.unwrap();
        assert_eq!(app.state, AppState::Cockpit);
    }

    #[tokio::test]
    async fn scenario_cockpit_issue_list_return() {
        let mut app = make_cockpit_app();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::IssueList;
        // Cockpit → Issue List
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::IssueList);
        // Issue List → q → Cockpit
        app.handle_issue_list_input(press(KeyCode::Char('q'))).await.unwrap();
        assert_eq!(app.state, AppState::Cockpit);
    }

    #[tokio::test]
    async fn scenario_cockpit_git_ops_return() {
        let mut app = make_cockpit_app();
        app.cockpit_state.as_mut().unwrap().selected_item = CockpitMenuItem::GitOps;
        // Cockpit → Git Ops
        app.handle_cockpit_input(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.state, AppState::GitOpsSplitTree);
        // Git Ops → q → Cockpit
        app.close_git_ops();
        assert_eq!(app.state, AppState::Cockpit);
    }
}
