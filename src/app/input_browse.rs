//! Key handling for the Repository Browser.
//!
//! Three focus states (tree / file / graph) plus modal outline and search overlays.
//! Overlays are checked first so they behave as modal layers rather than as an
//! extra set of conditionals sprinkled through the pane handlers.

use anyhow::Result;
use crossterm::event::{self, KeyCode, KeyModifiers};
use ratatui::{backend::CrosstermBackend, Terminal};
#[cfg(test)]
use ratatui::{layout::Rect, TerminalOptions, Viewport};
use std::io::Stdout;

use crate::filter::ListFilter;
use crate::keybinding::{event_to_keybinding, SequenceMatch};

#[cfg(test)]
use super::browse::ModuleGraphState;
use super::browse::{
    BrowseCommitDiffState, BrowseOverlay, IndexState, ModuleGraphDirection, ModuleGraphPaneState,
    ModuleGraphPanel, PrLookupState,
};
use super::browse_discussion::{DiscussionView, LineDiscussionState};
use super::{App, AppState, PrOpenSource};

/// Rows moved by a page key when the real viewport height is unknown.
const PAGE_STEP: usize = 15;

impl App {
    pub(crate) fn handle_repo_browse_tree_input(&mut self, key: event::KeyEvent) -> Result<()> {
        if self.handle_pr_lookup_selection_input(&key) {
            return Ok(());
        }
        if self.handle_browse_overlay_input(key) {
            return Ok(());
        }
        if self.handle_browse_filter_input(&key) {
            return Ok(());
        }
        if self.handle_browse_shared_input(&key) {
            return Ok(());
        }
        if self.handle_browse_tree_sequence_input(&key) {
            return Ok(());
        }

        let kb = self.config.keybindings.clone();

        if self.matches_single_key(&key, &kb.open_panel)
            || self.matches_single_key(&key, &kb.move_right)
        {
            self.open_selected_browse_entry();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            self.close_repo_browse();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.filter) {
            if let Some(state) = self.browse_state.as_mut() {
                state.filter = Some(ListFilter::new());
                state.apply_filter();
            }
            return Ok(());
        }

        let move_down = self.matches_single_key(&key, &kb.move_down);
        let move_up = self.matches_single_key(&key, &kb.move_up);
        let page_down = self.matches_single_key(&key, &kb.page_down);
        let page_up = self.matches_single_key(&key, &kb.page_up);
        let jump_last = self.matches_single_key(&key, &kb.jump_to_last);

        if move_down {
            self.browse_tree_move_down();
        } else if move_up {
            self.browse_tree_move_up();
        } else if let Some(state) = self.browse_state.as_mut() {
            if page_down {
                state.tree.page_down(PAGE_STEP);
            } else if page_up {
                state.tree.page_up(PAGE_STEP);
            } else if jump_last {
                state.tree.jump_to_last();
            }
        }

        Ok(())
    }

    pub(crate) fn browse_tree_move_down(&mut self) {
        if let Some(state) = self.browse_state.as_mut() {
            state.tree.move_down();
        }
    }

    pub(crate) fn browse_tree_move_up(&mut self) {
        if let Some(state) = self.browse_state.as_mut() {
            state.tree.move_up();
        }
    }

    pub(crate) fn open_selected_browse_entry(&mut self) {
        let is_dir = self
            .browse_state
            .as_ref()
            .is_some_and(|state| state.tree.selected_dir_path().is_some());
        if is_dir {
            if let Some(state) = self.browse_state.as_mut() {
                state.tree.toggle_expand();
            }
        } else {
            self.browse_open_selected();
        }
    }

    pub(crate) fn browse_tree_click_select(&mut self, row: usize) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        let already_selected = state.tree.selected_row == row;
        if !already_selected {
            state.tree.selected_row = row;
        }
        already_selected
    }

    pub(crate) fn handle_repo_browse_file_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        if self.handle_line_discussion_input(&key) {
            return Ok(());
        }
        if self.handle_pr_lookup_selection_input(&key) {
            return Ok(());
        }
        if self.handle_browse_overlay_input(key) {
            return Ok(());
        }
        if self.browse_commit_diff_is_active() {
            self.handle_browse_commit_diff_input(&key);
            return Ok(());
        }

        let kb = self.config.keybindings.clone();
        if self.matches_single_key(&key, &kb.open_blame_commit) {
            self.open_browse_blame_commit();
            return Ok(());
        }
        if self.matches_single_key(&key, &kb.open_blame_pr) {
            self.open_browse_blame_pr();
            return Ok(());
        }
        if self.matches_single_key(&key, &kb.open_line_discussion) {
            self.open_browse_line_discussion();
            return Ok(());
        }
        if self.handle_browse_shared_input(&key) {
            return Ok(());
        }
        if self.handle_browse_sequence_input(&key, terminal)? {
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.symbol_outline) {
            self.open_browse_outline();
            return Ok(());
        }
        if self.matches_single_key(&key, &kb.module_graph) {
            self.open_browse_module_graph();
            return Ok(());
        }
        if self.matches_single_key(&key, &kb.toggle_markdown_rich) {
            self.toggle_browse_markdown_rich();
            return Ok(());
        }

        let graph_visible = self
            .browse_state
            .as_ref()
            .is_some_and(|state| state.module_graph_pane.is_visible());
        if graph_visible
            && (key.code == KeyCode::Right || self.matches_single_key(&key, &kb.move_right))
        {
            self.state = AppState::RepoBrowseGraph;
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_left) || self.matches_single_key(&key, &kb.quit) {
            self.state = AppState::RepoBrowseTree;
            return Ok(());
        }

        let page_down =
            self.matches_single_key(&key, &kb.page_down) || key.code == KeyCode::PageDown;
        let page_up = self.matches_single_key(&key, &kb.page_up) || key.code == KeyCode::PageUp;
        let move_down = self.matches_single_key(&key, &kb.move_down);
        let move_up = self.matches_single_key(&key, &kb.move_up);
        let jump_last = self.matches_single_key(&key, &kb.jump_to_last);

        if move_down {
            self.browse_file_wheel_move(true);
        } else if move_up {
            self.browse_file_wheel_move(false);
        } else if let Some(state) = self.browse_state.as_mut() {
            if page_down {
                state.move_cursor(PAGE_STEP as isize);
            } else if page_up {
                state.move_cursor(-(PAGE_STEP as isize));
            } else if jump_last {
                let last = state
                    .open
                    .as_ref()
                    .map(|open| open.line_count().saturating_sub(1))
                    .unwrap_or(0);
                state.focus_line(last);
            }
        }

        Ok(())
    }

    pub(crate) fn browse_file_wheel_move(&mut self, down: bool) {
        if let Some(state) = self.browse_state.as_mut() {
            state.move_cursor(if down { 1 } else { -1 });
        }
    }

    pub(crate) fn browse_file_click_line(&mut self, line: usize) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        if state.cursor_line == line {
            return true;
        }
        state.focus_line(line);
        false
    }

    fn handle_pr_lookup_selection_input(&mut self, key: &event::KeyEvent) -> bool {
        let selecting = self
            .browse_state
            .as_ref()
            .is_some_and(|state| matches!(state.pr_lookup, PrLookupState::Selecting { .. }));
        if !selecting {
            return false;
        }

        let kb = self.config.keybindings.clone();
        let close = key.code == KeyCode::Esc || self.matches_single_key(key, &kb.quit);
        let down = self.matches_single_key(key, &kb.move_down);
        let up = self.matches_single_key(key, &kb.move_up);
        let confirm = self.matches_single_key(key, &kb.open_panel);

        if close {
            self.browse_pr_choice_dismiss();
        } else if down {
            self.browse_pr_choice_move(true);
        } else if up {
            self.browse_pr_choice_move(false);
        } else if confirm {
            self.browse_pr_choice_open_selected();
        }
        true
    }

    pub(crate) fn browse_pr_choice_move(&mut self, down: bool) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let PrLookupState::Selecting {
            pulls, selected, ..
        } = &mut state.pr_lookup
        else {
            return;
        };
        if down {
            *selected = (*selected + 1).min(pulls.len().saturating_sub(1));
        } else {
            *selected = selected.saturating_sub(1);
        }
    }

    pub(crate) fn browse_pr_choice_click_select(&mut self, row: usize) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        let PrLookupState::Selecting { selected, .. } = &mut state.pr_lookup else {
            return false;
        };
        let already_selected = *selected == row;
        if !already_selected {
            *selected = row;
        }
        already_selected
    }

    pub(crate) fn browse_pr_choice_open_selected(&mut self) {
        let selected_pull = self.browse_state.as_ref().and_then(|state| {
            let PrLookupState::Selecting {
                sha,
                pulls,
                selected,
            } = &state.pr_lookup
            else {
                return None;
            };
            pulls.get(*selected).map(|pull| (pull.number, sha.clone()))
        });
        if let Some((number, sha)) = selected_pull {
            self.open_pr_from_browse(number, PrOpenSource::ConfirmedCommit { sha });
        }
    }

    pub(crate) fn browse_pr_choice_dismiss(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        if matches!(state.pr_lookup, PrLookupState::Selecting { .. }) {
            state.pr_lookup = PrLookupState::Idle;
        }
    }

    fn handle_line_discussion_input(&mut self, key: &event::KeyEvent) -> bool {
        let active = self.browse_state.as_ref().is_some_and(|state| {
            matches!(
                state.line_discussion,
                LineDiscussionState::Ready {
                    view: DiscussionView::ThreadList { .. } | DiscussionView::Expanded { .. },
                    ..
                }
            )
        });
        if !active {
            return false;
        }

        let kb = self.config.keybindings.clone();
        let close = key.code == KeyCode::Esc || self.matches_single_key(key, &kb.quit);
        let down = self.matches_single_key(key, &kb.move_down);
        let up = self.matches_single_key(key, &kb.move_up);
        let confirm = self.matches_single_key(key, &kb.open_panel);

        if close {
            self.browse_line_discussion_dismiss();
        } else if down {
            self.browse_line_discussion_move(true);
        } else if up {
            self.browse_line_discussion_move(false);
        } else if confirm {
            self.browse_line_discussion_open_selected();
        }
        true
    }

    pub(crate) fn browse_line_discussion_move(&mut self, down: bool) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let LineDiscussionState::Ready { index, view, .. } = &mut state.line_discussion else {
            return;
        };
        let (selected, count) = match view {
            DiscussionView::ThreadList { line, selected, .. } => {
                (selected, index.thread_indices_at(*line).len())
            }
            DiscussionView::Expanded {
                line,
                thread_position,
                selected,
                ..
            } => {
                let count = index
                    .thread_indices_at(*line)
                    .get(*thread_position)
                    .and_then(|thread| index.threads.get(*thread))
                    .map(|thread| thread.replies.len() + 1)
                    .unwrap_or(0);
                (selected, count)
            }
            DiscussionView::Closed => return,
        };
        if down {
            *selected = (*selected + 1).min(count.saturating_sub(1));
        } else {
            *selected = selected.saturating_sub(1);
        }
    }

    pub(crate) fn browse_line_discussion_click_select(&mut self, row: usize) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        let LineDiscussionState::Ready { view, .. } = &mut state.line_discussion else {
            return false;
        };
        let selected = match view {
            DiscussionView::ThreadList { selected, .. }
            | DiscussionView::Expanded { selected, .. } => selected,
            DiscussionView::Closed => return false,
        };
        let already_selected = *selected == row;
        if !already_selected {
            *selected = row;
        }
        already_selected
    }

    pub(crate) fn browse_line_discussion_open_selected(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let LineDiscussionState::Ready { index, view, .. } = &mut state.line_discussion else {
            return;
        };
        let DiscussionView::ThreadList {
            line,
            selected,
            scroll: _,
        } = view
        else {
            return;
        };
        if !index.thread_indices_at(*line).is_empty() {
            *view = DiscussionView::Expanded {
                line: *line,
                thread_position: *selected,
                selected: 0,
                scroll: 0,
            };
        }
    }

    pub(crate) fn browse_line_discussion_dismiss(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let LineDiscussionState::Ready { view, .. } = &mut state.line_discussion else {
            return;
        };
        match view {
            DiscussionView::Closed => {}
            DiscussionView::ThreadList { .. } => *view = DiscussionView::Closed,
            DiscussionView::Expanded {
                line,
                thread_position,
                ..
            } => {
                *view = DiscussionView::ThreadList {
                    line: *line,
                    selected: *thread_position,
                    scroll: 0,
                };
            }
        }
    }

    /// Resolve tree-pane sequences (`Space /` filter, `gg` jump to first).
    ///
    /// The filter binding is a two-key sequence by default, so it cannot be
    /// matched with `matches_single_key` the way the other tree keys are.
    fn handle_browse_tree_sequence_input(&mut self, key: &event::KeyEvent) -> bool {
        self.check_sequence_timeout();

        let kb = self.config.keybindings.clone();
        let Some(binding) = event_to_keybinding(key) else {
            return false;
        };

        if !self.pending_keys.is_empty() {
            self.push_pending_key(binding);

            if self.try_match_sequence(&kb.filter) == SequenceMatch::Full {
                self.clear_pending_keys();
                if let Some(state) = self.browse_state.as_mut() {
                    state.filter = Some(ListFilter::new());
                    state.apply_filter();
                }
                return true;
            }
            if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                self.clear_pending_keys();
                if let Some(state) = self.browse_state.as_mut() {
                    state.tree.jump_to_first();
                }
                return true;
            }

            self.clear_pending_keys();
            return false;
        }

        let starts_sequence = self.key_could_match_sequence(key, &kb.filter)
            || self.key_could_match_sequence(key, &kb.jump_to_first);
        if starts_sequence {
            self.push_pending_key(binding);
            return true;
        }

        false
    }

    /// Keys shared by both panes. Returns `true` when the key was consumed.
    fn handle_browse_shared_input(&mut self, key: &event::KeyEvent) -> bool {
        let kb = self.config.keybindings.clone();

        if self.matches_single_key(key, &kb.symbol_search) {
            self.open_browse_symbol_search();
            return true;
        }
        if self.matches_single_key(key, &kb.help) {
            self.previous_state = self.state;
            self.state = AppState::Help;
            return true;
        }
        if self.matches_single_key(key, &kb.toggle_zen_mode) {
            self.toggle_zen_mode();
            return true;
        }
        if self.matches_single_key(key, &kb.jump_back) {
            if !self.browse_jump_back() {
                self.set_browse_status("No position to jump back to");
            }
            return true;
        }
        false
    }

    /// Resolve `gg` / `gb` / `gc` / `gp` / `gd` / `gf`.
    ///
    /// Returns `true` when the key was consumed.
    fn handle_browse_sequence_input(
        &mut self,
        key: &event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<bool> {
        self.check_sequence_timeout();

        let kb = self.config.keybindings.clone();
        let Some(binding) = event_to_keybinding(key) else {
            return Ok(false);
        };

        if !self.pending_keys.is_empty() {
            self.push_pending_key(binding);

            if self.try_match_sequence(&kb.toggle_blame) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.toggle_browse_blame();
                return Ok(true);
            }
            if self.try_match_sequence(&kb.open_blame_commit) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.open_browse_blame_commit();
                return Ok(true);
            }
            if self.try_match_sequence(&kb.open_blame_pr) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.open_browse_blame_pr();
                return Ok(true);
            }
            if self.try_match_sequence(&kb.open_line_discussion) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.open_browse_line_discussion();
                return Ok(true);
            }
            if self.try_match_sequence(&kb.module_graph) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.open_browse_module_graph();
                return Ok(true);
            }
            if self.try_match_sequence(&kb.go_to_definition) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.browse_run_go_to_definition();
                return Ok(true);
            }
            if self.try_match_sequence(&kb.go_to_file) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.open_browse_file_in_editor(terminal)?;
                return Ok(true);
            }
            if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                self.clear_pending_keys();
                if let Some(state) = self.browse_state.as_mut() {
                    state.focus_line(0);
                }
                return Ok(true);
            }

            self.clear_pending_keys();
            return Ok(false);
        }

        let starts_sequence = self.key_could_match_sequence(key, &kb.toggle_blame)
            || self.key_could_match_sequence(key, &kb.open_blame_commit)
            || self.key_could_match_sequence(key, &kb.open_blame_pr)
            || self.key_could_match_sequence(key, &kb.open_line_discussion)
            || (!kb.module_graph.is_single()
                && self.key_could_match_sequence(key, &kb.module_graph))
            || self.key_could_match_sequence(key, &kb.go_to_definition)
            || self.key_could_match_sequence(key, &kb.go_to_file)
            || self.key_could_match_sequence(key, &kb.jump_to_first);
        if starts_sequence {
            self.push_pending_key(binding);
            return Ok(true);
        }

        Ok(false)
    }

    fn handle_browse_commit_diff_input(&mut self, key: &event::KeyEvent) {
        let kb = self.config.keybindings.clone();

        if self.matches_single_key(key, &kb.open_blame_commit)
            || self.matches_single_key(key, &kb.jump_back)
            || self.matches_single_key(key, &kb.quit)
            || self.matches_single_key(key, &kb.move_left)
        {
            self.clear_pending_keys();
            if !self.return_from_browse_commit_diff() {
                self.set_browse_status("No position to jump back to");
            }
            return;
        }
        if self.matches_single_key(key, &kb.help) {
            self.clear_pending_keys();
            self.previous_state = self.state;
            self.state = AppState::Help;
            return;
        }
        if self.matches_single_key(key, &kb.toggle_zen_mode) {
            self.clear_pending_keys();
            self.toggle_zen_mode();
            return;
        }
        if self.handle_browse_commit_diff_sequence_input(key) {
            return;
        }

        let page_down =
            self.matches_single_key(key, &kb.page_down) || key.code == KeyCode::PageDown;
        let page_up = self.matches_single_key(key, &kb.page_up) || key.code == KeyCode::PageUp;
        let move_down = self.matches_single_key(key, &kb.move_down);
        let move_up = self.matches_single_key(key, &kb.move_up);
        let jump_last = self.matches_single_key(key, &kb.jump_to_last);

        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let BrowseCommitDiffState::Ready { scroll, .. } = &mut state.commit_diff else {
            return;
        };
        if move_down {
            scroll.move_down();
        } else if move_up {
            scroll.move_up();
        } else if page_down {
            scroll.page_down(PAGE_STEP);
        } else if page_up {
            scroll.page_up(PAGE_STEP);
        } else if jump_last {
            scroll.jump_to_last();
        }
    }

    fn handle_browse_commit_diff_sequence_input(&mut self, key: &event::KeyEvent) -> bool {
        self.check_sequence_timeout();

        let kb = self.config.keybindings.clone();
        let Some(binding) = event_to_keybinding(key) else {
            return false;
        };

        if !self.pending_keys.is_empty() {
            self.push_pending_key(binding);
            if self.try_match_sequence(&kb.open_blame_commit) == SequenceMatch::Full {
                self.clear_pending_keys();
                if !self.return_from_browse_commit_diff() {
                    self.set_browse_status("No position to jump back to");
                }
                return true;
            }
            if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                self.clear_pending_keys();
                if let Some(state) = self.browse_state.as_mut() {
                    if let BrowseCommitDiffState::Ready { scroll, .. } = &mut state.commit_diff {
                        scroll.jump_to_first();
                    }
                }
                return true;
            }

            self.clear_pending_keys();
            return false;
        }

        let starts_sequence = self.key_could_match_sequence(key, &kb.open_blame_commit)
            || self.key_could_match_sequence(key, &kb.jump_to_first);
        if starts_sequence {
            self.push_pending_key(binding);
            return true;
        }
        false
    }

    /// Jump to the definition under the cursor, reporting why when it cannot.
    fn browse_run_go_to_definition(&mut self) {
        // Checked before the index, because a pending load means we have not
        // read the cursor's line yet — there is no identifier to resolve, and
        // "No definition found" would be a claim about a file we have not seen.
        let pending = self
            .browse_state
            .as_ref()
            .is_some_and(|state| state.open_is_pending());
        if pending {
            self.set_browse_status("Still opening this file");
            return;
        }
        let blocked = self
            .browse_state
            .as_ref()
            .and_then(|state| state.index.blocked_message());
        if let Some(message) = blocked {
            self.set_browse_status(&message);
            return;
        }
        if !self.browse_go_to_definition() {
            self.set_browse_status("No definition found");
        }
    }

    /// Hand the open file to the configured external editor at the cursor line.
    fn open_browse_file_in_editor(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let Some(state) = self.browse_state.as_ref() else {
            return Ok(());
        };
        let Some(ref open) = state.open else {
            return Ok(());
        };
        let path = state.repo_root.join(&open.path);
        let line = state.cursor_line + 1;
        let editor = self.config.editor.clone();

        crate::ui::restore_terminal(terminal)?;
        let result =
            crate::editor::open_file_at_line(editor.as_deref(), &path.to_string_lossy(), line);
        *terminal = crate::ui::setup_terminal(self.mouse_capture_active)?;
        terminal.clear()?;

        if let Err(e) = result {
            self.set_browse_status(&format!("Editor failed: {e}"));
        }
        Ok(())
    }

    /// Handle keys for the tree's keyword filter. Returns `true` when consumed.
    fn handle_browse_filter_input(&mut self, key: &event::KeyEvent) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        if !state
            .filter
            .as_ref()
            .is_some_and(|filter| filter.input_active)
        {
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                state.filter = None;
                state.rebuild_tree();
            }
            KeyCode::Enter => {
                if let Some(filter) = state.filter.as_mut() {
                    filter.input_active = false;
                }
            }
            KeyCode::Backspace => {
                if let Some(filter) = state.filter.as_mut() {
                    filter.delete_char();
                }
                state.apply_filter();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(filter) = state.filter.as_mut() {
                    filter.clear_query();
                }
                state.apply_filter();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(filter) = state.filter.as_mut() {
                    filter.insert_char(c);
                }
                state.apply_filter();
            }
            _ => return false,
        }
        true
    }

    /// Dispatch to whichever overlay is open. Returns `true` when consumed.
    fn handle_browse_overlay_input(&mut self, key: event::KeyEvent) -> bool {
        let Some(state) = self.browse_state.as_ref() else {
            return false;
        };

        match state.overlay {
            BrowseOverlay::None => false,
            BrowseOverlay::Outline { .. } => {
                self.handle_browse_outline_input(key);
                true
            }
            BrowseOverlay::SymbolSearch { .. } => {
                self.handle_browse_symbol_search_input(key);
                true
            }
        }
    }

    fn handle_browse_outline_input(&mut self, key: event::KeyEvent) {
        let kb = self.config.keybindings.clone();
        let close = key.code == KeyCode::Esc || self.matches_single_key(&key, &kb.quit);
        let down = self.matches_single_key(&key, &kb.move_down);
        let up = self.matches_single_key(&key, &kb.move_up);
        let confirm = self.matches_single_key(&key, &kb.open_panel);

        if close {
            self.browse_outline_dismiss();
        } else if down {
            self.browse_outline_move(true);
        } else if up {
            self.browse_outline_move(false);
        } else if confirm {
            self.browse_outline_open_selected();
        }
    }

    pub(crate) fn browse_outline_move(&mut self, down: bool) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let count = state.outline_symbols().len();
        let BrowseOverlay::Outline { selected } = state.overlay else {
            return;
        };
        state.overlay = BrowseOverlay::Outline {
            selected: if down {
                (selected + 1).min(count.saturating_sub(1))
            } else {
                selected.saturating_sub(1)
            },
        };
    }

    pub(crate) fn browse_outline_click_select(&mut self, row: usize) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        let BrowseOverlay::Outline { selected } = &mut state.overlay else {
            return false;
        };
        let already_selected = *selected == row;
        if !already_selected {
            *selected = row;
        }
        already_selected
    }

    pub(crate) fn browse_outline_open_selected(&mut self) {
        let target = self.browse_state.as_ref().and_then(|state| {
            let BrowseOverlay::Outline { selected } = state.overlay else {
                return None;
            };
            state
                .outline_symbols()
                .get(selected)
                .map(|symbol| symbol.line)
        });
        self.browse_outline_dismiss();
        if let Some(line) = target {
            self.browse_push_jump();
            if let Some(state) = self.browse_state.as_mut() {
                state.focus_line(line.saturating_sub(1));
            }
            self.state = AppState::RepoBrowseFile;
        }
    }

    pub(crate) fn browse_outline_dismiss(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        if matches!(state.overlay, BrowseOverlay::Outline { .. }) {
            state.overlay = BrowseOverlay::None;
        }
    }

    fn handle_browse_symbol_search_input(&mut self, key: event::KeyEvent) {
        // Typing wins over navigation: only arrows and Ctrl-n/p move the
        // selection, so `j` and `k` stay usable inside the query.
        match key.code {
            KeyCode::Esc => {
                self.browse_symbol_search_dismiss();
                return;
            }
            KeyCode::Enter => {
                self.browse_symbol_search_open_selected();
                return;
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.browse_symbol_search_move(true);
                return;
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.browse_symbol_search_move(false);
                return;
            }
            KeyCode::Down => {
                self.browse_symbol_search_move(true);
                return;
            }
            KeyCode::Up => {
                self.browse_symbol_search_move(false);
                return;
            }
            _ => {}
        }

        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let BrowseOverlay::SymbolSearch {
            ref mut query,
            ref mut selected,
        } = state.overlay
        else {
            return;
        };
        match key.code {
            KeyCode::Backspace => {
                query.pop();
                *selected = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                query.clear();
                *selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                query.push(c);
                *selected = 0;
            }
            _ => return,
        }

        state.clamp_symbol_search_selection();
    }

    pub(crate) fn browse_symbol_search_move(&mut self, down: bool) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let BrowseOverlay::SymbolSearch { selected, .. } = &mut state.overlay else {
            return;
        };
        if down {
            *selected += 1;
        } else {
            *selected = selected.saturating_sub(1);
        }
        state.clamp_symbol_search_selection();
    }

    pub(crate) fn browse_symbol_search_click_select(&mut self, row: usize) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        let BrowseOverlay::SymbolSearch { selected, .. } = &mut state.overlay else {
            return false;
        };
        let already_selected = *selected == row;
        if !already_selected {
            *selected = row;
        }
        already_selected
    }

    pub(crate) fn browse_symbol_search_open_selected(&mut self) {
        let target = self.browse_state.as_ref().and_then(|state| {
            let BrowseOverlay::SymbolSearch { query, selected } = &state.overlay else {
                return None;
            };
            state
                .symbol_search_results(query)
                .into_iter()
                .nth(*selected)
                .map(|(path, line, _)| (path, line))
        });
        self.browse_symbol_search_dismiss();
        if let Some((path, line)) = target {
            self.browse_push_jump();
            self.browse_open_path(&path, line.saturating_sub(1));
        }
    }

    pub(crate) fn browse_symbol_search_dismiss(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        if matches!(state.overlay, BrowseOverlay::SymbolSearch { .. }) {
            state.overlay = BrowseOverlay::None;
        }
    }

    pub(crate) fn handle_repo_browse_graph_input(&mut self, key: event::KeyEvent) {
        if self.handle_line_discussion_input(&key) {
            return;
        }
        if self.handle_pr_lookup_selection_input(&key) {
            return;
        }
        if self.handle_browse_overlay_input(key) {
            return;
        }
        if self.handle_browse_shared_input(&key) {
            return;
        }
        if self.handle_browse_graph_module_sequence(&key) {
            return;
        }

        let kb = self.config.keybindings.clone();
        if self.matches_single_key(&key, &kb.module_graph) {
            if let Some(state) = self.browse_state.as_mut() {
                state.close_module_graph_pane();
            }
            self.state = AppState::RepoBrowseFile;
            return;
        }
        if key.code == KeyCode::Esc || self.matches_single_key(&key, &kb.quit) {
            self.state = AppState::RepoBrowseFile;
            return;
        }

        let down = self.matches_single_key(&key, &kb.move_down);
        let up = self.matches_single_key(&key, &kb.move_up);
        let confirm = self.matches_single_key(&key, &kb.open_panel);
        let show_dependencies =
            key.code == KeyCode::Left || self.matches_single_key(&key, &kb.move_left);
        let show_dependents =
            key.code == KeyCode::Right || self.matches_single_key(&key, &kb.move_right);
        let toggle = key.code == KeyCode::Tab;

        {
            let Some(state) = self.browse_state.as_ref() else {
                return;
            };
            if !matches!(state.module_graph_pane, ModuleGraphPaneState::Ready(_)) {
                if matches!(state.module_graph_pane, ModuleGraphPaneState::Closed) {
                    self.state = AppState::RepoBrowseFile;
                }
                return;
            }
        }

        if toggle {
            self.browse_graph_toggle_direction();
        } else if show_dependencies {
            let _ = self
                .with_graph_panel(|panel| panel.set_direction(ModuleGraphDirection::Dependencies));
        } else if show_dependents {
            let _ = self
                .with_graph_panel(|panel| panel.set_direction(ModuleGraphDirection::Dependents));
        } else if down || up {
            self.browse_graph_move(down);
        } else if confirm {
            self.browse_graph_open_selected();
        }
    }

    fn with_graph_panel<T>(&mut self, f: impl FnOnce(&mut ModuleGraphPanel) -> T) -> Option<T> {
        let state = self.browse_state.as_mut()?;
        let ModuleGraphPaneState::Ready(panel) = &mut state.module_graph_pane else {
            return None;
        };
        Some(f(panel))
    }

    pub(crate) fn browse_graph_move(&mut self, down: bool) {
        let _ = self.with_graph_panel(|panel| {
            if down {
                panel.selected =
                    (panel.selected + 1).min(panel.current_rows().len().saturating_sub(1));
            } else {
                panel.selected = panel.selected.saturating_sub(1);
            }
        });
    }

    pub(crate) fn browse_graph_click_select(&mut self, row: usize) -> bool {
        self.with_graph_panel(|panel| {
            if panel.selected == row {
                return true;
            }
            panel.selected = row.min(panel.current_rows().len().saturating_sub(1));
            false
        })
        .unwrap_or(false)
    }

    pub(crate) fn browse_graph_open_selected(&mut self) {
        let mut jump = None;
        let mut unlisted = false;
        let _ = self.with_graph_panel(|panel| {
            if let Some(row) = panel.current_rows().get(panel.selected) {
                jump = row.jump.clone();
                unlisted = jump.is_none();
            }
        });
        if unlisted {
            if let Some(state) = self.browse_state.as_mut() {
                state.status =
                    Some("This dependency target is not a listed repository file".into());
            }
        }
        if let Some(jump) = jump {
            self.browse_push_jump();
            self.browse_open_path(&jump.path, jump.line);
        }
    }

    pub(crate) fn browse_graph_toggle_direction(&mut self) {
        let _ = self.with_graph_panel(|panel| {
            panel.set_direction(match panel.direction {
                ModuleGraphDirection::Dependencies => ModuleGraphDirection::Dependents,
                ModuleGraphDirection::Dependents => ModuleGraphDirection::Dependencies,
            });
        });
    }

    fn handle_browse_graph_module_sequence(&mut self, key: &event::KeyEvent) -> bool {
        self.check_sequence_timeout();
        let binding = self.config.keybindings.module_graph.clone();
        if binding.is_single() {
            return false;
        }
        let Some(keybinding) = event_to_keybinding(key) else {
            return false;
        };

        if !self.pending_keys.is_empty() {
            self.push_pending_key(keybinding);
            let close = self.try_match_sequence(&binding) == SequenceMatch::Full;
            self.clear_pending_keys();
            if close {
                if let Some(state) = self.browse_state.as_mut() {
                    state.close_module_graph_pane();
                }
                self.state = AppState::RepoBrowseFile;
            }
            return close;
        }

        if self.key_could_match_sequence(key, &binding) {
            self.push_pending_key(keybinding);
            return true;
        }
        false
    }

    pub(crate) fn open_browse_module_graph(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        if state.module_graph_pane.is_visible() {
            self.state = AppState::RepoBrowseGraph;
            return;
        }
        if state.open_is_pending() {
            state.status = Some("Still opening this file".to_string());
            return;
        }
        let Some(path) = state.open.as_ref().map(|open| open.path.clone()) else {
            state.status = Some("No file is open".to_string());
            return;
        };
        match state.start_module_graph_query_for_path(path) {
            Ok(()) => {
                state.status = None;
                self.state = AppState::RepoBrowseGraph;
            }
            Err(message) => state.status = Some(message.to_string()),
        }
    }

    pub(crate) fn open_browse_outline(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        if state.outline_symbols().is_empty() {
            // The pending check must come first. During a background load
            // `open` is a placeholder with no symbols, so reporting on the
            // index alone would claim the file has none when it has not been
            // read yet.
            let message = if state.open_is_pending() {
                "Still opening this file".to_string()
            } else if let Some(message) = state.index.blocked_message() {
                message
            } else {
                "No symbols in this file".to_string()
            };
            state.status = Some(message);
            return;
        }

        // Land on the symbol containing the cursor rather than at the top —
        // the outline answers "where am I" as much as "where can I go".
        let cursor_line = state.cursor_line + 1;
        let selected = state
            .outline_symbols()
            .iter()
            .rposition(|symbol| symbol.line <= cursor_line)
            .unwrap_or(0);
        state.overlay = BrowseOverlay::Outline { selected };
    }

    pub(crate) fn open_browse_symbol_search(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        if let Some(message) = state.index.blocked_message() {
            state.status = Some(message);
            return;
        }
        state.overlay = BrowseOverlay::SymbolSearch {
            query: String::new(),
            selected: 0,
        };
    }

    pub(crate) fn set_browse_status(&mut self, message: &str) {
        if let Some(state) = self.browse_state.as_mut() {
            state.status = Some(message.to_string());
        }
    }
}

impl super::browse::BrowseState {
    /// Keep the highlighted row inside the current result set.
    pub fn clamp_symbol_search_selection(&mut self) {
        let count = match self.overlay {
            BrowseOverlay::SymbolSearch { ref query, .. } => self.symbol_search_hits(query).len(),
            _ => return,
        };
        if let BrowseOverlay::SymbolSearch {
            ref mut selected, ..
        } = self.overlay
        {
            *selected = (*selected).min(count.saturating_sub(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::browse::{
        build_file_patch, BlameGutter, BlameState, BrowseState, OpenFile, OpenLoad, PrLookupState,
    };
    use crate::app::browse_discussion::{DiscussionIndex, DiscussionOutcome};
    use crate::app::{PrOpenSource, RepositoryAvailability};
    use crate::diff_store::{DiffScrollState, ScrollMode};
    use crate::github::{
        parse_porcelain, CommitPrResolution, CommitPullRequest, CommitPullRequestState,
    };
    use crate::keybinding::{KeyBinding, KeySequence};
    use crate::symbols::{FileSymbols, Symbol, SymbolIndex, SymbolKind};
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn press_binding(binding: KeyBinding) -> KeyEvent {
        KeyEvent {
            code: binding.code.to_keycode(),
            modifiers: binding.modifiers.to_crossterm(),
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// An app parked in the browser with a fake repository already listed.
    fn browsing_app(paths: &[&str]) -> App {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        state.set_paths(paths.iter().map(|p| p.to_string()).collect());
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        app
    }

    fn attach_open_file(app: &mut App, path: &str, source: &str, symbols: Vec<Symbol>) {
        let patch = build_file_patch(source);
        let state = app.browse_state.as_mut().expect("browse state");
        state.open = Some(OpenFile {
            path: path.to_string(),
            cache: crate::ui::diff_view::build_plain_diff_cache(&patch, 4),
            patch,
            lines: source.lines().map(str::to_string).collect(),
            symbols,
            viewable: true,
            notice: None,
        });
        state.sync_tree_to_open_file();
    }

    fn attach_blame(app: &mut App, path: &str, porcelain: &str) {
        let state = app.browse_state.as_mut().expect("browse state");
        let buffer_lines = state.open.as_ref().map_or(0, OpenFile::line_count);
        state.blame = BlameState::Ready {
            path: path.to_string(),
            gutter: BlameGutter::from_file(parse_porcelain(porcelain), buffer_lines),
        };
    }

    fn press_open_blame_commit(app: &mut App, terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('c')), terminal)
            .unwrap();
    }

    fn render_browser(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn attach_index(app: &mut App, files: Vec<FileSymbols>) {
        let state = app.browse_state.as_mut().expect("browse state");
        state.index = IndexState::Ready(Arc::new(SymbolIndex::from_files(files)));
        state.refresh_open_file_symbols();
    }

    fn attach_code_index(app: &mut App, root: &std::path::Path, paths: &[&str]) {
        let paths: Vec<_> = paths.iter().map(|path| (*path).to_string()).collect();
        let crate::code_index::CodeIndexBuild::Completed(code) =
            crate::code_index::CodeIndex::build_cancellable(
                root,
                &paths,
                crate::module_graph::SourceUniverse::Complete,
                &tokio_util::sync::CancellationToken::new(),
            )
        else {
            panic!("code index fixture must build");
        };
        let crate::code_index::CodeIndex { symbols, modules } = *code;
        let state = app.browse_state.as_mut().expect("browse state");
        state.index = IndexState::Ready(Arc::new(symbols));
        state.module_graph = ModuleGraphState::Ready(Arc::new(modules));
        state.refresh_open_file_symbols();
    }

    /// A browser rooted at a real directory holding `files`.
    ///
    /// Jumps that land in a *different* file go through `browse_open_path`,
    /// which reads from disk — the in-memory `attach_open_file` fixture cannot
    /// reach that branch.
    fn browsing_app_on_disk(root: &std::path::Path, files: &[(&str, &str)]) -> App {
        for (path, source) in files {
            let absolute = root.join(path);
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).expect("fixture directory");
            }
            std::fs::write(&absolute, source).expect("fixture file");
        }

        let mut app = App::new_for_test();
        let mut state = BrowseState::new(root.to_path_buf(), AppState::FileList);
        state.set_paths(files.iter().map(|(path, _)| (*path).to_string()).collect());
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        app
    }

    fn symbol(name: &str, kind: SymbolKind, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            line,
            column: 0,
            depth: 0,
        }
    }

    fn open_path(app: &App) -> &str {
        app.browse_state
            .as_ref()
            .and_then(|state| state.open.as_ref())
            .map_or("", |open| open.path.as_str())
    }

    /// Drive the browse background channels until the pending load lands.
    async fn settle_browse(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state.as_ref().map(|state| &state.open_load),
                None | Some(OpenLoad::Idle | OpenLoad::Failed { .. })
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("browse load never settled");
    }

    async fn settle_module_graph(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if !matches!(
                app.browse_state
                    .as_ref()
                    .map(|state| &state.module_graph_pane),
                Some(ModuleGraphPaneState::Waiting { .. } | ModuleGraphPaneState::Loading { .. })
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("module graph query never settled");
    }

    async fn settle_blame(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state.as_ref().map(|state| &state.blame),
                Some(BlameState::Ready { .. } | BlameState::Failed)
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("blame load never settled");
    }

    /// Drive the browse channels until the pending highlight upgrade lands.
    async fn settle_highlight(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state
                    .as_ref()
                    .map(|state| &state.highlight_receiver),
                None | Some(None)
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("highlight never settled");
    }

    async fn settle_commit_diff(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state.as_ref().map(|state| &state.commit_diff),
                Some(BrowseCommitDiffState::Ready { .. } | BrowseCommitDiffState::Failed { .. })
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("commit diff load never settled");
    }

    // ===== scenario: navigate the tree and leave =====

    #[test]
    fn test_scenario_move_through_tree_then_quit_back_to_caller() {
        let mut app = browsing_app(&["src/a.rs", "src/b.rs"]);
        let start = app
            .browse_state
            .as_ref()
            .map(|state| state.tree.selected_row)
            .unwrap();

        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().tree.selected_row,
            start + 1
        );

        app.handle_repo_browse_tree_input(press(KeyCode::Char('k')))
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().tree.selected_row, start);

        app.handle_repo_browse_tree_input(press(KeyCode::Char('q')))
            .unwrap();
        assert_eq!(app.state, AppState::FileList);
        assert!(app.browse_state.is_none());
    }

    #[test]
    fn test_scenario_refocusing_the_open_file_keeps_the_cursor_position() {
        let source: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let mut app = browsing_app(&["a.rs"]);
        attach_open_file(&mut app, "a.rs", &source, Vec::new());
        app.state = AppState::RepoBrowseFile;
        let mut terminal = test_terminal();

        for _ in 0..10 {
            app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut terminal)
                .unwrap();
        }
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 10);

        app.handle_repo_browse_file_input(press(KeyCode::Char('h')), &mut terminal)
            .unwrap();
        assert_eq!(app.state, AppState::RepoBrowseTree);

        app.handle_repo_browse_tree_input(press(KeyCode::Char('l')))
            .unwrap();
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(
            app.browse_state.as_ref().unwrap().cursor_line,
            10,
            "re-entering the already-open file is a focus change, not a jump"
        );
    }

    #[tokio::test]
    async fn test_scenario_opening_a_different_file_still_starts_at_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let source: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let mut app =
            browsing_app_on_disk(dir.path(), &[("a.rs", source.as_str()), ("b.rs", "one\n")]);
        attach_open_file(&mut app, "a.rs", &source, Vec::new());
        app.state = AppState::RepoBrowseFile;
        let mut terminal = test_terminal();

        for _ in 0..10 {
            app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut terminal)
                .unwrap();
        }
        app.handle_repo_browse_file_input(press(KeyCode::Char('h')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Char('l')))
            .unwrap();

        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(
            app.browse_state.as_ref().unwrap().cursor_line,
            0,
            "a different file opens at the top"
        );
    }

    #[tokio::test]
    async fn test_scenario_open_file_toggle_blame_on_poll_then_toggle_off() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Scenario Author",
                    "-c",
                    "user.email=scenario@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir.path())
                .output();
            match output {
                Ok(output) if output.status.success() => true,
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && std::env::var_os("CI").is_none() =>
                {
                    false
                }
                Ok(output) => panic!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr)
                ),
                Err(error) => panic!("git {} failed: {error}", args.join(" ")),
            }
        };
        if !git(&["init"]) {
            return;
        }
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        assert!(git(&["add", "main.rs"]));
        assert!(git(&["commit", "-m", "baseline"]));

        let mut app = browsing_app_on_disk(dir.path(), &[("main.rs", "fn main() {}\n")]);
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Off
        ));

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('b')), &mut terminal)
            .unwrap();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Loading { .. }
        ));

        settle_blame(&mut app).await;
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.blame,
            BlameState::Ready {
                ref path,
                ref gutter
            } if path == "main.rs" && gutter.len() == 1
        ));

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('b')), &mut terminal)
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.blame, BlameState::Off));
        assert!(state.blame_receiver.is_none());
    }

    #[tokio::test]
    async fn test_scenario_markdown_toggle_rebuilds_highlight_for_the_open_markdown_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut app =
            browsing_app_on_disk(dir.path(), &[("README.md", "# Title\n\nSome prose.\n")]);
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;
        settle_highlight(&mut app).await;

        let cache_rich = |app: &App| {
            app.browse_state
                .as_ref()
                .and_then(|state| state.open.as_ref())
                .expect("open file")
                .cache
                .markdown_rich
        };
        assert!(!app.is_markdown_rich());
        assert!(!cache_rich(&app));

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('M')), &mut terminal)
            .unwrap();
        assert!(app.is_markdown_rich());
        assert!(
            app.browse_state
                .as_ref()
                .unwrap()
                .highlight_receiver
                .is_some(),
            "toggling markdown rich must restart the open markdown file's highlight"
        );

        settle_highlight(&mut app).await;
        assert!(
            cache_rich(&app),
            "the rebuilt cache must carry the rich transforms"
        );

        app.handle_repo_browse_file_input(press(KeyCode::Char('M')), &mut terminal)
            .unwrap();
        assert!(!app.is_markdown_rich());
        settle_highlight(&mut app).await;
        assert!(
            !cache_rich(&app),
            "toggling back must restore the base cache"
        );
    }

    #[tokio::test]
    async fn test_scenario_markdown_toggle_skips_rehighlight_for_non_markdown_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = browsing_app_on_disk(dir.path(), &[("main.rs", "fn main() {}\n")]);
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;
        settle_highlight(&mut app).await;

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('M')), &mut terminal)
            .unwrap();
        assert!(app.is_markdown_rich(), "the flag itself still toggles");
        assert!(
            app.browse_state
                .as_ref()
                .unwrap()
                .highlight_receiver
                .is_none(),
            "a non-markdown file's highlight is unaffected by the flag, so \
             rebuilding it would be wasted work"
        );
    }

    /// While a load is pending, `open` is a placeholder with no content.
    /// Highlighting it would race the landing file's cache with an empty one,
    /// so the toggle only flips the flag and lets the landing load pick it up.
    #[tokio::test]
    async fn test_scenario_markdown_toggle_during_a_pending_load_lands_rich() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = browsing_app_on_disk(dir.path(), &[("README.md", "# Title\n")]);
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        assert!(app.browse_state.as_ref().unwrap().open_is_pending());

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('M')), &mut terminal)
            .unwrap();
        assert!(app.is_markdown_rich());
        assert!(
            app.browse_state
                .as_ref()
                .unwrap()
                .highlight_receiver
                .is_none(),
            "a pending placeholder must not be highlighted"
        );

        settle_browse(&mut app).await;
        settle_highlight(&mut app).await;
        assert!(
            app.browse_state
                .as_ref()
                .and_then(|state| state.open.as_ref())
                .expect("open file")
                .cache
                .markdown_rich,
            "the landing load's highlight must read the toggled flag"
        );
    }

    #[test]
    fn test_scenario_open_blame_commit_explains_every_unavailable_cursor_state() {
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        const UNCOMMITTED: &str = "0000000000000000000000000000000000000000 1 1 1\n\
             author Not Committed Yet\n\
             author-time 0\n\
             summary working tree\n\
             \tone\n";

        let mut terminal = test_terminal();
        let mut messages = Vec::new();

        let mut no_file = browsing_app(&["a.rs"]);
        no_file.state = AppState::RepoBrowseFile;
        press_open_blame_commit(&mut no_file, &mut terminal);
        assert_eq!(
            no_file.browse_state.as_ref().unwrap().status.as_deref(),
            Some("No file is open")
        );
        messages.push(
            no_file
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .clone()
                .unwrap(),
        );

        let mut opening = browsing_app(&["a.rs"]);
        attach_open_file(&mut opening, "a.rs", "one\n", Vec::new());
        opening.state = AppState::RepoBrowseFile;
        opening.browse_state.as_mut().unwrap().open_load = OpenLoad::Pending {
            path: "a.rs".to_string(),
            line: 0,
            scroll: None,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        press_open_blame_commit(&mut opening, &mut terminal);
        assert_eq!(
            opening.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Still opening this file")
        );
        messages.push(
            opening
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .clone()
                .unwrap(),
        );

        let mut blame_off = browsing_app(&["a.rs"]);
        attach_open_file(&mut blame_off, "a.rs", "one\n", Vec::new());
        blame_off.state = AppState::RepoBrowseFile;
        press_open_blame_commit(&mut blame_off, &mut terminal);
        assert_eq!(
            blame_off.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Blame is off — press gb to enable")
        );
        messages.push(
            blame_off
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .clone()
                .unwrap(),
        );

        let mut blame_loading = browsing_app(&["a.rs"]);
        attach_open_file(&mut blame_loading, "a.rs", "one\n", Vec::new());
        blame_loading.state = AppState::RepoBrowseFile;
        let state = blame_loading.browse_state.as_mut().unwrap();
        state.blame = BlameState::Loading {
            path: "a.rs".to_string(),
            cancel: state.cancel_token.child_token(),
        };
        press_open_blame_commit(&mut blame_loading, &mut terminal);
        assert_eq!(
            blame_loading
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .as_deref(),
            Some("Blame is still loading")
        );
        messages.push(
            blame_loading
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .clone()
                .unwrap(),
        );

        let mut blame_failed = browsing_app(&["a.rs"]);
        attach_open_file(&mut blame_failed, "a.rs", "one\n", Vec::new());
        blame_failed.state = AppState::RepoBrowseFile;
        blame_failed.browse_state.as_mut().unwrap().blame = BlameState::Failed;
        press_open_blame_commit(&mut blame_failed, &mut terminal);
        assert_eq!(
            blame_failed
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .as_deref(),
            Some("Blame failed for this file")
        );
        messages.push(
            blame_failed
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .clone()
                .unwrap(),
        );

        let mut wrong_path = browsing_app(&["a.rs", "b.rs"]);
        attach_open_file(&mut wrong_path, "a.rs", "one\n", Vec::new());
        attach_blame(&mut wrong_path, "b.rs", COMMITTED);
        wrong_path.state = AppState::RepoBrowseFile;
        press_open_blame_commit(&mut wrong_path, &mut terminal);
        assert_eq!(
            wrong_path.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Blame belongs to another file")
        );
        messages.push(
            wrong_path
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .clone()
                .unwrap(),
        );

        let mut missing_line = browsing_app(&["a.rs"]);
        attach_open_file(&mut missing_line, "a.rs", "one\ntwo\n", Vec::new());
        attach_blame(&mut missing_line, "a.rs", COMMITTED);
        missing_line.state = AppState::RepoBrowseFile;
        missing_line.browse_state.as_mut().unwrap().cursor_line = 1;
        press_open_blame_commit(&mut missing_line, &mut terminal);
        assert_eq!(
            missing_line
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .as_deref(),
            Some("Blame is unavailable for this line")
        );
        messages.push(
            missing_line
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .clone()
                .unwrap(),
        );

        let mut uncommitted = browsing_app(&["a.rs"]);
        attach_open_file(&mut uncommitted, "a.rs", "one\n", Vec::new());
        attach_blame(&mut uncommitted, "a.rs", UNCOMMITTED);
        uncommitted.state = AppState::RepoBrowseFile;
        press_open_blame_commit(&mut uncommitted, &mut terminal);
        assert_eq!(
            uncommitted.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Uncommitted line has no commit to open")
        );
        messages.push(
            uncommitted
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .clone()
                .unwrap(),
        );

        let unique: std::collections::HashSet<&str> = messages.iter().map(String::as_str).collect();
        assert_eq!(unique.len(), messages.len(), "{messages:?}");
    }

    #[test]
    fn test_scenario_custom_single_key_opens_the_blame_commit_action() {
        let mut app = browsing_app(&["a.rs"]);
        attach_open_file(&mut app, "a.rs", "one\n", Vec::new());
        app.state = AppState::RepoBrowseFile;
        app.config.keybindings.open_blame_commit = KeySequence::single(KeyBinding::char('x'));

        app.handle_repo_browse_file_input(press(KeyCode::Char('x')), &mut test_terminal())
            .unwrap();

        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Blame is off — press gb to enable")
        );
    }

    #[test]
    fn test_scenario_custom_single_key_runs_blame_pr_lookup_without_waiting_for_a_prefix() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary pushed directly\n\
             \tone\n";
        let mut app = browsing_app(&["a.rs"]);
        app.set_repository_availability(RepositoryAvailability::Available);
        attach_open_file(&mut app, "a.rs", "one\n", Vec::new());
        attach_blame(&mut app, "a.rs", COMMITTED);
        app.state = AppState::RepoBrowseFile;
        app.config.keybindings.open_blame_pr = KeySequence::single(KeyBinding::char('x'));
        app.session_cache
            .put_commit_pr_resolution(SHA.to_string(), CommitPrResolution::NotFound);

        app.handle_repo_browse_file_input(press(KeyCode::Char('x')), &mut test_terminal())
            .unwrap();

        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("No pull request found for commit aaaaaaa")
        );
    }

    #[test]
    fn test_scenario_default_gp_sequence_runs_blame_pr_lookup() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary pushed directly\n\
             \tone\n";
        let mut app = browsing_app(&["a.rs"]);
        app.set_repository_availability(RepositoryAvailability::Available);
        attach_open_file(&mut app, "a.rs", "one\n", Vec::new());
        attach_blame(&mut app, "a.rs", COMMITTED);
        app.state = AppState::RepoBrowseFile;
        app.session_cache
            .put_commit_pr_resolution(SHA.to_string(), CommitPrResolution::NotFound);
        let mut terminal = test_terminal();

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('p')), &mut terminal)
            .unwrap();

        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("No pull request found for commit aaaaaaa")
        );
    }

    #[test]
    fn test_scenario_configurable_line_discussion_key_works_as_single_and_default_sequence() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             summary pushed directly\n\
             filename a.rs\n\
             \tone\n";

        for binding in [
            KeySequence::single(KeyBinding::char('x')),
            KeySequence::double(KeyBinding::char('g'), KeyBinding::char('r')),
        ] {
            let mut app = browsing_app(&["a.rs"]);
            app.set_repository_availability(RepositoryAvailability::Available);
            attach_open_file(&mut app, "a.rs", "one\n", Vec::new());
            attach_blame(&mut app, "a.rs", COMMITTED);
            app.state = AppState::RepoBrowseFile;
            app.config.keybindings.open_line_discussion = binding.clone();
            app.session_cache
                .put_commit_pr_resolution(SHA.to_string(), CommitPrResolution::NotFound);
            let mut terminal = test_terminal();

            for key in &binding.keys {
                app.handle_repo_browse_file_input(press_binding(*key), &mut terminal)
                    .unwrap();
            }

            assert_eq!(
                app.browse_state.as_ref().unwrap().status.as_deref(),
                Some("No confirmed pull request found for this file")
            );
            assert!(matches!(
                app.browse_state.as_ref().unwrap().line_discussion,
                LineDiscussionState::Failed {
                    failure: crate::app::browse_discussion::LineDiscussionFailure::NoPullRequest,
                    ..
                }
            ));
        }
    }

    #[tokio::test]
    async fn test_scenario_pressing_open_blame_commit_twice_cancels_and_returns_without_respawn() {
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let mut app = browsing_app(&["a.rs"]);
        attach_open_file(&mut app, "a.rs", "one\n", Vec::new());
        attach_blame(&mut app, "a.rs", COMMITTED);
        app.set_local_mode(true);
        app.state = AppState::RepoBrowseFile;
        let mut terminal = test_terminal();

        press_open_blame_commit(&mut app, &mut terminal);
        let first_cancel = match &app.browse_state.as_ref().unwrap().commit_diff {
            BrowseCommitDiffState::Loading { cancel, .. } => cancel.clone(),
            _ => panic!("first key did not start the commit diff"),
        };

        press_open_blame_commit(&mut app, &mut terminal);

        let state = app.browse_state.as_ref().unwrap();
        assert!(first_cancel.is_cancelled());
        assert!(matches!(state.commit_diff, BrowseCommitDiffState::Off));
        assert!(state.commit_diff_receiver.is_none());
        assert!(state.jump_stack.is_empty());
        assert_eq!(app.state, AppState::RepoBrowseFile);
    }

    #[tokio::test]
    async fn test_scenario_multiple_commit_pull_requests_require_selection_before_opening() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let mut app = browsing_app(&["a.rs"]);
        app.set_repository_availability(RepositoryAvailability::Available);
        attach_open_file(&mut app, "a.rs", "one\n", Vec::new());
        attach_blame(&mut app, "a.rs", COMMITTED);
        app.state = AppState::RepoBrowseFile;
        app.session_cache.put_commit_pr_resolution(
            SHA.to_string(),
            CommitPrResolution::Confirmed {
                pulls: vec![
                    CommitPullRequest {
                        number: 10,
                        title: "still open".to_string(),
                        state: CommitPullRequestState::Open,
                    },
                    CommitPullRequest {
                        number: 20,
                        title: "already merged".to_string(),
                        state: CommitPullRequestState::Merged,
                    },
                ],
            },
        );

        app.open_browse_blame_pr();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().pr_lookup,
            PrLookupState::Selecting {
                selected: 0,
                ref pulls,
                ..
            } if pulls.len() == 2
        ));

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut terminal)
            .unwrap();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().pr_lookup,
            PrLookupState::Selecting { selected: 1, .. }
        ));

        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut terminal)
            .unwrap();

        assert_eq!(app.state, AppState::FileList);
        assert_eq!(app.pr_number, Some(20));
        assert!(app.browse_state.is_none());
        assert_eq!(
            app.pr_open_source,
            PrOpenSource::ConfirmedCommit {
                sha: SHA.to_string()
            }
        );
    }

    #[test]
    fn test_scenario_escape_closes_pull_request_selection_without_opening_a_pr() {
        let mut app = browsing_app(&["a.rs"]);
        app.state = AppState::RepoBrowseFile;
        app.browse_state.as_mut().unwrap().pr_lookup = PrLookupState::Selecting {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            pulls: vec![
                CommitPullRequest {
                    number: 10,
                    title: "one".to_string(),
                    state: CommitPullRequestState::Open,
                },
                CommitPullRequest {
                    number: 20,
                    title: "two".to_string(),
                    state: CommitPullRequestState::Closed,
                },
            ],
            selected: 0,
        };

        app.handle_repo_browse_file_input(press(KeyCode::Esc), &mut test_terminal())
            .unwrap();

        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(app.pr_number, Some(1));
        assert!(matches!(
            app.browse_state.as_ref().unwrap().pr_lookup,
            PrLookupState::Idle
        ));
    }

    #[test]
    fn test_scenario_leaving_commit_diff_clears_a_half_typed_source_sequence() {
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let mut app = browsing_app(&["a.rs"]);
        attach_open_file(&mut app, "a.rs", "one\n", Vec::new());
        attach_blame(&mut app, "a.rs", COMMITTED);
        app.state = AppState::RepoBrowseFile;
        app.browse_push_jump();

        let annotation = match &app.browse_state.as_ref().unwrap().blame {
            BlameState::Ready { gutter, .. } => Arc::clone(gutter.annotation_at(0).unwrap()),
            _ => panic!("blame fixture must be ready"),
        };
        let cache = crate::ui::diff_view::build_plain_diff_cache("+changed\n", 4);
        let mut scroll = DiffScrollState::new(ScrollMode::Margin);
        scroll.set_line_count(cache.lines.len());
        app.browse_state.as_mut().unwrap().commit_diff = BrowseCommitDiffState::Ready {
            annotation,
            cache,
            scroll,
        };

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('q')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('b')), &mut terminal)
            .unwrap();

        assert!(app.pending_keys.is_empty());
        assert!(matches!(
            app.browse_state.as_ref().unwrap().commit_diff,
            BrowseCommitDiffState::Off
        ));
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Ready { .. }
        ));
    }

    #[tokio::test]
    async fn test_scenario_open_continuation_blame_commit_scroll_then_return_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Scenario Author",
                    "-c",
                    "user.email=scenario@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git command");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        git(&["init"]);
        std::fs::write(dir.path().join("main.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        git(&["add", "main.rs"]);
        git(&["commit", "-m", "baseline"]);
        let head = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();

        let mut app =
            browsing_app_on_disk(dir.path(), &[("main.rs", "fn one() {}\nfn two() {}\n")]);
        app.set_working_dir(Some(dir.path().to_string_lossy().into_owned()));
        app.set_local_mode(true);
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('b')), &mut terminal)
            .unwrap();
        settle_blame(&mut app).await;

        {
            let state = app.browse_state.as_mut().unwrap();
            state.cursor_line = 1;
            state.scroll_offset = 1;
        }
        press_open_blame_commit(&mut app, &mut terminal);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().commit_diff,
            BrowseCommitDiffState::Loading {
                ref annotation, ..
            } if annotation.sha() == head
        ));

        settle_commit_diff(&mut app).await;
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.commit_diff,
            BrowseCommitDiffState::Ready {
                ref annotation,
                ref cache,
                ..
            } if annotation.sha() == head && !cache.lines.is_empty()
        ));
        assert!(matches!(state.blame, BlameState::Ready { .. }));

        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut terminal)
            .unwrap();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().commit_diff,
            BrowseCommitDiffState::Ready { ref scroll, .. } if scroll.selected_line == 1
        ));

        app.handle_repo_browse_file_input(press(KeyCode::Char('q')), &mut terminal)
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.commit_diff, BrowseCommitDiffState::Off));
        assert_eq!(state.cursor_line, 1);
        assert_eq!(state.scroll_offset, 1);
        assert!(matches!(state.blame, BlameState::Ready { .. }));
        assert!(state.jump_stack.is_empty());
        assert_eq!(app.state, AppState::RepoBrowseFile);
    }

    #[tokio::test]
    async fn test_scenario_merge_blame_commit_renders_a_first_parent_diff_in_the_browser() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Scenario Author",
                    "-c",
                    "user.email=scenario@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git command");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };

        git(&["init"]);
        std::fs::write(dir.path().join("main.rs"), "base\n").unwrap();
        git(&["add", "main.rs"]);
        git(&["commit", "-m", "baseline"]);

        git(&["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("feature.rs"), "feature\n").unwrap();
        git(&["add", "feature.rs"]);
        git(&["commit", "-m", "feature"]);

        git(&["checkout", "main"]);
        std::fs::write(dir.path().join("main-only.rs"), "main\n").unwrap();
        git(&["add", "main-only.rs"]);
        git(&["commit", "-m", "main"]);

        git(&["merge", "--no-ff", "-m", "merge feature", "feature"]);
        let merge_sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();

        let mut app = browsing_app_on_disk(dir.path(), &[("main.rs", "base\n")]);
        app.set_working_dir(Some(dir.path().to_string_lossy().into_owned()));
        app.set_local_mode(true);
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('b')), &mut terminal)
            .unwrap();
        settle_blame(&mut app).await;
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Ready { .. }
        ));
        let merge_porcelain = format!(
            "{merge_sha} 1 1 1\n\
             author Scenario Author\n\
             author-time 1700000000\n\
             summary merge feature\n\
             \tbase\n"
        );
        attach_blame(&mut app, "main.rs", &merge_porcelain);

        press_open_blame_commit(&mut app, &mut terminal);
        settle_commit_diff(&mut app).await;

        let rendered = render_browser(&mut app, 120, 14);
        assert!(
            rendered.contains("+feature"),
            "merge commit diff must be visible in the browser:\n{rendered}"
        );
        assert!(!rendered.contains("This commit has no diff."));
    }

    #[tokio::test]
    async fn test_scenario_missing_blame_commit_surfaces_git_error_and_still_returns() {
        const MISSING_SHA: &str = "ffffffffffffffffffffffffffffffffffffffff";
        const PORCELAIN: &str = "ffffffffffffffffffffffffffffffffffffffff 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary vanished commit\n\
             \tone\n";

        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init"])
            .current_dir(dir.path())
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && std::env::var_os("CI").is_none() =>
            {
                return;
            }
            Ok(output) => panic!(
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => panic!("git init failed: {error}"),
        }

        let mut app = browsing_app_on_disk(dir.path(), &[("main.rs", "one\n")]);
        attach_open_file(&mut app, "main.rs", "one\n", Vec::new());
        attach_blame(&mut app, "main.rs", PORCELAIN);
        app.set_working_dir(Some(dir.path().to_string_lossy().into_owned()));
        app.set_local_mode(true);
        app.state = AppState::RepoBrowseFile;

        let mut terminal = test_terminal();
        press_open_blame_commit(&mut app, &mut terminal);
        settle_commit_diff(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.commit_diff,
            BrowseCommitDiffState::Failed {
                ref annotation,
                ref message,
            } if annotation.sha() == MISSING_SHA && message.contains("git show failed")
        ));
        assert_eq!(state.jump_stack.len(), 1);

        app.handle_repo_browse_file_input(press(KeyCode::Char('q')), &mut terminal)
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.commit_diff, BrowseCommitDiffState::Off));
        assert!(matches!(state.blame, BlameState::Ready { .. }));
        assert!(state.jump_stack.is_empty());
    }

    #[test]
    fn test_scenario_collapse_and_expand_a_directory() {
        let mut app = browsing_app(&["src/a.rs", "src/b.rs"]);
        // Row 0 is the `src/` directory row.
        assert_eq!(app.browse_state.as_ref().unwrap().tree.row_count(), 3);

        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().tree.row_count(), 1);
        assert_eq!(
            app.state,
            AppState::RepoBrowseTree,
            "collapsing is not opening"
        );

        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().tree.row_count(), 3);
    }

    // ===== scenario: filter the tree =====

    #[test]
    fn test_scenario_filter_then_cancel_restores_full_tree() {
        let mut app = browsing_app(&["src/app.rs", "src/ui.rs", "README.md"]);
        let full_rows = app.browse_state.as_ref().unwrap().tree.row_count();

        app.handle_repo_browse_tree_input(press(KeyCode::Char(' ')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Char('/')))
            .unwrap();
        for c in "ui".chars() {
            app.handle_repo_browse_tree_input(press(KeyCode::Char(c)))
                .unwrap();
        }
        assert_eq!(
            app.browse_state.as_ref().unwrap().tree.dump_tree(),
            "▼ src/\n  ui.rs"
        );

        app.handle_repo_browse_tree_input(press(KeyCode::Backspace))
            .unwrap();
        app.handle_repo_browse_tree_input(ctrl('u')).unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().tree.row_count(),
            full_rows
        );

        app.handle_repo_browse_tree_input(press(KeyCode::Esc))
            .unwrap();
        assert!(app.browse_state.as_ref().unwrap().filter.is_none());
        assert_eq!(
            app.browse_state.as_ref().unwrap().tree.row_count(),
            full_rows
        );
    }

    #[test]
    fn test_filter_input_swallows_navigation_keys() {
        let mut app = browsing_app(&["src/app.rs", "src/ui.rs"]);
        app.handle_repo_browse_tree_input(press(KeyCode::Char(' ')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Char('/')))
            .unwrap();
        // 'j' must type, not move the cursor, while the filter bar is active.
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        assert_eq!(
            app.browse_state
                .as_ref()
                .unwrap()
                .filter
                .as_ref()
                .unwrap()
                .query,
            "j"
        );
    }

    // ===== scenario: read a file =====

    #[test]
    fn test_scenario_scroll_a_file_and_return_to_the_tree() {
        let mut app = browsing_app(&["src/a.rs"]);
        let source: String = (0..40).map(|i| format!("line {i}\n")).collect();
        attach_open_file(&mut app, "src/a.rs", &source, Vec::new());
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 1);

        app.handle_repo_browse_file_input(press(KeyCode::Char('G')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 39);

        // `gg` returns to the top.
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 0);

        app.handle_repo_browse_file_input(press(KeyCode::Char('h')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.state, AppState::RepoBrowseTree);
    }

    // ===== scenario: outline =====

    fn app_with_outline() -> App {
        let mut app = browsing_app(&["src/a.rs"]);
        attach_open_file(
            &mut app,
            "src/a.rs",
            "struct App;\nimpl App {\n  fn run() {}\n}\n",
            Vec::new(),
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/a.rs".to_string(),
                symbols: vec![
                    Symbol {
                        name: "App".to_string(),
                        kind: SymbolKind::Class,
                        line: 1,
                        column: 7,
                        depth: 0,
                    },
                    Symbol {
                        name: "run".to_string(),
                        kind: SymbolKind::Method,
                        line: 3,
                        column: 5,
                        depth: 1,
                    },
                ],
            }],
        );
        app.state = AppState::RepoBrowseFile;
        app
    }

    #[test]
    fn test_scenario_outline_jumps_to_a_symbol() {
        let mut app = app_with_outline();

        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::Outline { selected: 0 }
        );

        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut test_terminal())
            .unwrap();

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(state.cursor_line, 2, "jumped to `fn run` on line 3");
        assert_eq!(state.jump_stack.len(), 1, "jump is undoable");
    }

    #[test]
    fn test_outline_opens_on_the_symbol_under_the_cursor() {
        let mut app = app_with_outline();
        // Walk the cursor onto `fn run` rather than assigning `cursor_line`,
        // so the pre-selection is reached the way a reader reaches it.
        for _ in 0..2 {
            app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
                .unwrap();
        }
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 2);

        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::Outline { selected: 1 }
        );
    }

    #[test]
    fn test_outline_escape_closes_without_moving() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Esc), &mut test_terminal())
            .unwrap();

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(state.cursor_line, 0);
        assert!(state.jump_stack.is_empty());
    }

    #[test]
    fn test_outline_without_symbols_reports_instead_of_opening_empty() {
        let mut app = browsing_app(&["notes.txt"]);
        attach_open_file(&mut app, "notes.txt", "plain text\n", Vec::new());
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "notes.txt".to_string(),
                symbols: Vec::new(),
            }],
        );
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(state.status.as_deref(), Some("No symbols in this file"));
    }

    // ===== scenario: symbol search =====

    #[test]
    fn test_scenario_symbol_search_typing_then_jump() {
        let mut app = app_with_outline();

        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        for c in "run".chars() {
            app.handle_repo_browse_file_input(press(KeyCode::Char(c)), &mut test_terminal())
                .unwrap();
        }
        assert_eq!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::SymbolSearch {
                query: "run".to_string(),
                selected: 0,
            }
        );

        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut test_terminal())
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(state.cursor_line, 2);
    }

    #[test]
    fn test_symbol_search_j_and_k_type_rather_than_navigate() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();

        let BrowseOverlay::SymbolSearch { ref query, .. } =
            app.browse_state.as_ref().unwrap().overlay
        else {
            panic!("overlay should still be symbol search");
        };
        assert_eq!(query, "j");
    }

    #[test]
    fn test_symbol_search_selection_is_clamped_to_results() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        // Two symbols exist, but only one matches "run".
        for _ in 0..5 {
            app.handle_repo_browse_file_input(press(KeyCode::Down), &mut test_terminal())
                .unwrap();
        }
        for c in "run".chars() {
            app.handle_repo_browse_file_input(press(KeyCode::Char(c)), &mut test_terminal())
                .unwrap();
        }
        assert_eq!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::SymbolSearch {
                query: "run".to_string(),
                selected: 0,
            }
        );
    }

    /// The clamp bounds the cursor by the same set the overlay draws.
    ///
    /// With fewer matches than the cap the two are indistinguishable, which is
    /// what the sibling test above exercises. Only a query that overflows the
    /// cap can tell them apart: a clamp with a cap of its own would stop the
    /// cursor short of rows the user can plainly see and never let them open
    /// one.
    #[test]
    fn test_the_selection_clamp_reaches_every_row_the_overlay_draws() {
        let mut app = app_with_outline();
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/a.rs".to_string(),
                symbols: (0..crate::app::browse::MAX_SYMBOL_SEARCH_RESULTS + 50)
                    .map(|n| Symbol {
                        name: format!("many_{n:04}"),
                        kind: SymbolKind::Function,
                        line: n + 1,
                        column: 0,
                        depth: 0,
                    })
                    .collect(),
            }],
        );

        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        for c in "many".chars() {
            app.handle_repo_browse_file_input(press(KeyCode::Char(c)), &mut test_terminal())
                .unwrap();
        }
        let drawn = app
            .browse_state
            .as_ref()
            .unwrap()
            .symbol_search_hits("many")
            .len();
        assert_eq!(drawn, crate::app::browse::MAX_SYMBOL_SEARCH_RESULTS);

        // One terminal for the whole run. `test_terminal()` opens a real handle
        // on stdout, and stdout is a pipe under `cargo test`; building one per
        // keypress meant hundreds alive at once and intermittently failed with
        // `WouldBlock`. The keypresses are what this test is about.
        let mut terminal = test_terminal();
        for _ in 0..drawn + 20 {
            app.handle_repo_browse_file_input(press(KeyCode::Down), &mut terminal)
                .unwrap();
        }

        let BrowseOverlay::SymbolSearch { selected, .. } =
            app.browse_state.as_ref().unwrap().overlay
        else {
            panic!("the overlay must still be symbol search");
        };
        assert_eq!(
            selected,
            drawn - 1,
            "the cursor stopped short of rows the overlay draws, so they can be \
             seen but never opened"
        );
    }

    #[test]
    fn test_symbol_search_refuses_to_open_before_the_index_is_ready() {
        let mut app = browsing_app(&["src/a.rs"]);
        app.handle_repo_browse_tree_input(press(KeyCode::Char('s')))
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(
            state.status.as_deref(),
            Some("Symbol index has not started")
        );
    }

    // ===== scenario: go to definition and jump back =====

    #[test]
    fn test_go_to_definition_reports_when_index_is_not_ready() {
        let mut app = browsing_app(&["src/a.rs"]);
        attach_open_file(&mut app, "src/a.rs", "let x = alpha();\n", Vec::new());
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut test_terminal())
            .unwrap();

        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Symbol index has not started")
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum Lifecycle {
        Idle,
        Building,
        Failed,
    }

    impl Lifecycle {
        fn install(self, app: &mut App) {
            let index = match self {
                Self::Idle => IndexState::Idle,
                Self::Building => IndexState::Building,
                Self::Failed => IndexState::Failed("parser crashed".to_string()),
            };
            let state = app.browse_state.as_mut().unwrap();
            state.index = index;
            state.status = None;
        }

        fn message(self) -> &'static str {
            match self {
                Self::Idle => "Symbol index has not started",
                Self::Building => "Symbol index is still building",
                Self::Failed => "Symbol index is unavailable: parser crashed",
            }
        }
    }

    #[test]
    fn test_symbol_actions_report_idle_building_and_failed_indexes() {
        for lifecycle in [Lifecycle::Idle, Lifecycle::Building, Lifecycle::Failed] {
            let expected = lifecycle.message();
            let mut terminal = test_terminal();

            let mut outline = browsing_app(&["src/a.rs"]);
            attach_open_file(&mut outline, "src/a.rs", "let x = alpha();\n", Vec::new());
            lifecycle.install(&mut outline);
            outline.state = AppState::RepoBrowseFile;
            outline
                .handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut terminal)
                .unwrap();
            let state = outline.browse_state.as_ref().unwrap();
            assert_eq!(
                state.overlay,
                BrowseOverlay::None,
                "{lifecycle:?} opened the outline"
            );
            assert_eq!(
                state.status.as_deref(),
                Some(expected),
                "{lifecycle:?} outline"
            );

            let mut search = browsing_app(&["src/a.rs"]);
            lifecycle.install(&mut search);
            search.state = AppState::RepoBrowseFile;
            search
                .handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut terminal)
                .unwrap();
            let state = search.browse_state.as_ref().unwrap();
            assert_eq!(
                state.overlay,
                BrowseOverlay::None,
                "{lifecycle:?} opened symbol search"
            );
            assert_eq!(
                state.status.as_deref(),
                Some(expected),
                "{lifecycle:?} symbol search"
            );

            let mut definition = browsing_app(&["src/a.rs"]);
            attach_open_file(
                &mut definition,
                "src/a.rs",
                "let x = alpha();\n",
                Vec::new(),
            );
            lifecycle.install(&mut definition);
            definition.state = AppState::RepoBrowseFile;
            definition
                .handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
                .unwrap();
            definition
                .handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut terminal)
                .unwrap();
            assert_eq!(
                definition.browse_state.as_ref().unwrap().status.as_deref(),
                Some(expected),
                "{lifecycle:?} go to definition"
            );
        }
    }

    /// A pending load leaves `open` holding a placeholder with no content and
    /// no symbols. Both `o` and `gd` answer questions about the open file, so
    /// both must say the file is still opening rather than reporting on the
    /// placeholder as if it were the file.
    #[test]
    fn test_outline_and_definition_do_not_report_on_a_file_still_being_read() {
        for key in ['o', 'd'] {
            let mut app = app_with_outline();
            attach_index(
                &mut app,
                vec![FileSymbols {
                    path: "src/a.rs".to_string(),
                    symbols: Vec::new(),
                }],
            );
            let state = app.browse_state.as_mut().unwrap();
            state.open_load = OpenLoad::Pending {
                path: "src/a.rs".to_string(),
                line: 0,
                scroll: None,
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            state.open = None;
            state.status = None;
            app.state = AppState::RepoBrowseFile;

            if key == 'd' {
                app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
                    .unwrap();
            }
            app.handle_repo_browse_file_input(press(KeyCode::Char(key)), &mut test_terminal())
                .unwrap();

            assert_eq!(
                app.browse_state.as_ref().unwrap().status.as_deref(),
                Some("Still opening this file"),
                "key {key} reported on a placeholder instead of the pending load"
            );
        }
    }

    /// The pending check must sit *above* the index check, not merely exist.
    ///
    /// With a ready index both orderings answer the same thing, so the sibling
    /// test above cannot see the order. Here both conditions hold at once —
    /// the file is still being read *and* the index is not ready — and only the
    /// documented order produces the pending message.
    #[test]
    fn test_pending_load_outranks_an_unbuilt_index_for_both_keys() {
        for key in ['o', 'd'] {
            let mut app = browsing_app(&["src/a.rs"]);
            let state = app.browse_state.as_mut().unwrap();
            assert!(
                state.index.ready().is_none(),
                "this test is only meaningful while the index is unbuilt"
            );
            state.open_load = OpenLoad::Pending {
                path: "src/a.rs".to_string(),
                line: 0,
                scroll: None,
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            state.open = None;
            state.status = None;
            app.state = AppState::RepoBrowseFile;

            if key == 'd' {
                app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
                    .unwrap();
            }
            app.handle_repo_browse_file_input(press(KeyCode::Char(key)), &mut test_terminal())
                .unwrap();

            assert_eq!(
                app.browse_state.as_ref().unwrap().status.as_deref(),
                Some("Still opening this file"),
                "key {key} let the index check answer for a file it has not read"
            );
        }
    }

    #[test]
    fn test_go_to_definition_reports_when_nothing_matches() {
        let mut app = app_with_outline();
        app.browse_state.as_mut().unwrap().cursor_line = 0;
        // Line 1 is `struct App;` — `App` resolves, so point at a line with no
        // indexed identifier instead.
        attach_open_file(
            &mut app,
            "src/a.rs",
            "let q = unknown_thing();\n",
            Vec::new(),
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/a.rs".to_string(),
                symbols: Vec::new(),
            }],
        );
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut test_terminal())
            .unwrap();

        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("No definition found")
        );
    }

    #[test]
    fn test_jump_back_returns_to_the_previous_position() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 2);

        app.handle_repo_browse_file_input(ctrl('o'), &mut test_terminal())
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.cursor_line, 0);
        assert!(state.jump_stack.is_empty());
    }

    #[tokio::test]
    async fn test_scenario_go_to_definition_crosses_files_and_ctrl_o_returns() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[
                ("src/lib.rs", "pub struct Config;\n\npub fn helper() {}\n"),
                ("src/main.rs", "fn main() {\n    helper();\n}\n"),
            ],
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/lib.rs".to_string(),
                symbols: vec![
                    symbol("Config", SymbolKind::Class, 1),
                    symbol("helper", SymbolKind::Function, 3),
                ],
            }],
        );

        // Row 0 is `src/`, row 1 `lib.rs`, row 2 `main.rs`.
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(open_path(&app), "src/main.rs");
        settle_browse(&mut app).await;

        // Put the cursor on `    helper();`.
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 1);

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        let open = state.open.as_ref().unwrap();
        assert_eq!(open.path, "src/lib.rs", "jumped into the other file");
        assert_eq!(
            open.lines,
            vec!["pub struct Config;", "", "pub fn helper() {}"],
            "the target was read from disk, not reused from the caller"
        );
        assert_eq!(state.cursor_line, 2, "`pub fn helper` is line 3, 0-based 2");
        assert_eq!(state.status, None, "a successful jump reports nothing");
        assert_eq!(state.jump_stack.len(), 1, "the jump is undoable");
        assert_eq!(state.tree.selected_row, 1, "the tree followed the jump");
        assert_eq!(app.state, AppState::RepoBrowseFile);

        app.handle_repo_browse_file_input(ctrl('o'), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(open_path(&app), "src/main.rs", "Ctrl-o reopened the caller");
        assert_eq!(state.cursor_line, 1, "back on the call site");
        assert!(state.jump_stack.is_empty());
    }

    /// Pins the granularity the README documents: `gd` is a *line* operation.
    ///
    /// `BrowseState` tracks `cursor_line` and no column, so a line naming two
    /// indexed symbols resolves in reading order — `Config` here, never
    /// `helper`. Changing that without changing the README is a regression.
    #[tokio::test]
    async fn test_go_to_definition_resolves_the_first_indexed_identifier_on_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[
                ("src/lib.rs", "pub struct Config;\n\npub fn helper() {}\n"),
                ("src/main.rs", "fn main() {\n    Config::helper();\n}\n"),
            ],
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/lib.rs".to_string(),
                symbols: vec![
                    symbol("Config", SymbolKind::Class, 1),
                    symbol("helper", SymbolKind::Function, 3),
                ],
            }],
        );

        // Row 0 is `src/`, row 1 `lib.rs`, row 2 `main.rs`.
        for _ in 0..2 {
            app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
                .unwrap();
        }
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;
        // Put the cursor on `    Config::helper();`.
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 1);

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;

        assert_eq!(open_path(&app), "src/lib.rs");
        assert_eq!(
            app.browse_state.as_ref().unwrap().cursor_line,
            0,
            "`Config` comes first on the line, so `helper` is never tried"
        );
    }

    #[tokio::test]
    async fn test_scenario_symbol_search_jumps_into_a_file_that_is_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[
                ("src/lib.rs", "pub struct Config;\n\npub fn helper() {}\n"),
                ("src/main.rs", "fn main() {\n    helper();\n}\n"),
            ],
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/lib.rs".to_string(),
                symbols: vec![
                    symbol("Config", SymbolKind::Class, 1),
                    symbol("helper", SymbolKind::Function, 3),
                ],
            }],
        );
        app.browse_open_path("src/main.rs", 0);
        settle_browse(&mut app).await;

        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        for c in "helper".chars() {
            app.handle_repo_browse_file_input(press(KeyCode::Char(c)), &mut test_terminal())
                .unwrap();
        }
        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(open_path(&app), "src/lib.rs", "left the file it started in");
        assert_eq!(state.cursor_line, 2);
        assert_eq!(state.jump_stack.len(), 1);
        assert_eq!(
            state.tree.selected_row, 1,
            "the tree follows the file the overlay opened"
        );
    }

    #[test]
    fn test_jump_back_with_empty_stack_reports() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(ctrl('o'), &mut test_terminal())
            .unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("No position to jump back to")
        );
    }

    // ===== opening from the tree =====

    #[tokio::test]
    async fn test_open_a_file_from_the_tree_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n").unwrap();

        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["src/a.rs".to_string()]);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;

        // Row 0 is `src/`; move onto the file before opening it.
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();

        assert_eq!(app.state, AppState::RepoBrowseFile);
        let placeholder = app.browse_state.as_ref().unwrap().open.as_ref().unwrap();
        assert_eq!(placeholder.path, "src/a.rs");
        assert!(!placeholder.viewable);
        assert!(placeholder
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Loading")));

        settle_browse(&mut app).await;

        let open = app.browse_state.as_ref().unwrap().open.as_ref().unwrap();
        assert!(open.viewable);
        assert_eq!(open.lines, vec!["pub fn alpha() {}"]);
    }

    #[tokio::test]
    async fn test_missing_file_stays_on_failure_pane_then_returns_to_a_usable_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("live.rs"), "pub fn live() {}\n").unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["gone.rs".to_string(), "live.rs".to_string()]);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;

        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;

        assert_eq!(app.state, AppState::RepoBrowseFile);
        let state = app.browse_state.as_ref().unwrap();
        let status = state.status.as_deref().unwrap();
        assert!(status.starts_with("gone.rs:"), "{status}");
        let open = state.open.as_ref().expect("failure pane");
        assert_eq!(open.path, "gone.rs");
        assert!(!open.viewable);
        let notice = open.notice.as_deref().expect("failure notice");
        assert!(notice.contains("gone.rs:"), "{notice}");

        app.handle_repo_browse_file_input(press(KeyCode::Esc), &mut test_terminal())
            .unwrap();
        assert_eq!(app.state, AppState::RepoBrowseTree);

        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(state.open.as_ref().unwrap().path, "live.rs");
        assert_eq!(
            state.open.as_ref().unwrap().lines,
            vec!["pub fn live() {}"],
            "the tree remained usable after leaving the failure pane"
        );
    }

    #[tokio::test]
    async fn test_module_graph_key_opens_and_focuses_a_right_side_pane() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "import { helper } from './helper';\n";
        let helper_source = "export const helper = 1;\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[("src/app.ts", app_source), ("src/helper.ts", helper_source)],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/helper.ts"]);
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('i')), &mut test_terminal())
            .unwrap();

        assert_eq!(app.state, AppState::RepoBrowseGraph);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            super::super::browse::ModuleGraphPaneState::Loading { .. }
        ));
        assert!(matches!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::None
        ));
    }

    #[test]
    fn test_escape_returns_to_code_without_hiding_the_module_graph_pane() {
        let mut app = browsing_app(&["src/app.ts"]);
        attach_open_file(&mut app, "src/app.ts", "export {};\n", Vec::new());
        app.state = AppState::RepoBrowseGraph;
        app.browse_state.as_mut().unwrap().module_graph_pane =
            super::super::browse::ModuleGraphPaneState::Loading {
                request_id: 7,
                path: "src/app.ts".to_string(),
            };

        app.handle_repo_browse_graph_input(press(KeyCode::Esc));

        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            super::super::browse::ModuleGraphPaneState::Loading { .. }
        ));
    }

    #[test]
    fn test_discussion_modal_handles_escape_before_graph_focus_controls() {
        let mut app = browsing_app(&["src/app.ts"]);
        attach_open_file(&mut app, "src/app.ts", "export {};\n", Vec::new());
        app.state = AppState::RepoBrowseGraph;
        let state = app.browse_state.as_mut().unwrap();
        state.module_graph_pane = ModuleGraphPaneState::Loading {
            request_id: 7,
            path: "src/app.ts".to_string(),
        };
        state.line_discussion = LineDiscussionState::Ready {
            path: "src/app.ts".to_string(),
            pr_numbers: vec![42],
            index: DiscussionIndex {
                comments: Vec::new(),
                threads: Vec::new(),
                line_threads: vec![Default::default()],
                file_thread_count: 0,
                comment_paths: Vec::new(),
                outcome: DiscussionOutcome::Complete,
            },
            view: DiscussionView::ThreadList {
                line: 0,
                selected: 0,
                scroll: 0,
            },
        };

        app.handle_repo_browse_graph_input(press(KeyCode::Esc));

        assert_eq!(app.state, AppState::RepoBrowseGraph);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().line_discussion,
            LineDiscussionState::Ready {
                view: DiscussionView::Closed,
                ..
            }
        ));
    }

    #[test]
    fn test_right_from_code_refocuses_a_visible_module_graph_pane() {
        let mut app = browsing_app(&["src/app.ts"]);
        attach_open_file(&mut app, "src/app.ts", "export {};\n", Vec::new());
        app.state = AppState::RepoBrowseFile;
        app.browse_state.as_mut().unwrap().module_graph_pane = ModuleGraphPaneState::Loading {
            request_id: 7,
            path: "src/app.ts".to_string(),
        };

        app.handle_repo_browse_file_input(press(KeyCode::Right), &mut test_terminal())
            .unwrap();

        assert_eq!(app.state, AppState::RepoBrowseGraph);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            ModuleGraphPaneState::Loading { .. }
        ));
    }

    #[test]
    fn test_graph_selection_and_direction_are_clamped_to_visible_nodes() {
        let mut app = browsing_app(&["src/app.ts"]);
        attach_open_file(&mut app, "src/app.ts", "export {};\n", Vec::new());
        app.state = AppState::RepoBrowseGraph;
        let row = |name: &str| super::super::browse::ModuleGraphRow {
            node: name.to_string(),
            edge: "[import] ./node  :1".to_string(),
            jump: None,
        };
        app.browse_state.as_mut().unwrap().module_graph_pane =
            ModuleGraphPaneState::Ready(super::super::browse::ModuleGraphPanel {
                path: "src/app.ts".to_string(),
                display_path: "src/app.ts".to_string(),
                direction: ModuleGraphDirection::Dependencies,
                selected: 0,
                dependencies: super::super::browse::ModuleGraphRows {
                    rows: vec![row("first"), row("second")],
                    total: 2,
                    guarantee: crate::module_graph::DependencyGuarantee::Exact,
                },
                dependents: super::super::browse::ModuleGraphRows {
                    rows: vec![row("importer")],
                    total: 1,
                    guarantee: crate::module_graph::DependencyGuarantee::Exact,
                },
            });

        app.config.keybindings.module_graph =
            KeySequence::double(KeyBinding::char('m'), KeyBinding::char('i'));
        app.handle_repo_browse_graph_input(press(KeyCode::Char('m')));
        app.handle_repo_browse_graph_input(press(KeyCode::Down));
        let ModuleGraphPaneState::Ready(panel) =
            &app.browse_state.as_ref().unwrap().module_graph_pane
        else {
            panic!("graph pane must stay ready");
        };
        assert_eq!(panel.selected, 1);

        app.handle_repo_browse_graph_input(press(KeyCode::Tab));
        let ModuleGraphPaneState::Ready(panel) =
            &app.browse_state.as_ref().unwrap().module_graph_pane
        else {
            panic!("graph pane must stay ready");
        };
        assert_eq!(panel.direction, ModuleGraphDirection::Dependents);
        assert_eq!(panel.selected, 0);
    }

    #[tokio::test]
    async fn test_visible_module_graph_refreshes_when_the_open_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "import './helper';\n";
        let helper_source = "export const helper = 1;\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[("src/app.ts", app_source), ("src/helper.ts", helper_source)],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/helper.ts"]);
        app.state = AppState::RepoBrowseFile;
        app.open_browse_module_graph();
        settle_module_graph(&mut app).await;

        app.browse_open_path("src/helper.ts", 0);

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            &state.module_graph_pane,
            super::super::browse::ModuleGraphPaneState::Waiting { path }
                if path == "src/helper.ts"
        ));
        assert!(state.module_graph_query_receiver.is_none());
    }

    #[tokio::test]
    async fn test_waiting_module_graph_closes_when_the_new_file_fails_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "export {};\n";
        let mut app = browsing_app_on_disk(dir.path(), &[("src/app.ts", app_source)]);
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        app.state = AppState::RepoBrowseFile;
        app.browse_state.as_mut().unwrap().module_graph_pane =
            ModuleGraphPaneState::Ready(super::super::browse::ModuleGraphPanel {
                path: "src/app.ts".to_string(),
                display_path: "src/app.ts".to_string(),
                direction: ModuleGraphDirection::Dependencies,
                selected: 0,
                dependencies: super::super::browse::ModuleGraphRows {
                    rows: Vec::new(),
                    total: 0,
                    guarantee: crate::module_graph::DependencyGuarantee::Exact,
                },
                dependents: super::super::browse::ModuleGraphRows {
                    rows: Vec::new(),
                    total: 0,
                    guarantee: crate::module_graph::DependencyGuarantee::Exact,
                },
            });

        app.browse_open_path("src/missing.ts", 0);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            ModuleGraphPaneState::Waiting { .. }
        ));
        app.state = AppState::RepoBrowseGraph;
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Closed
        ));
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|message| message.contains("missing.ts")),
            "file-load error must survive graph-pane cleanup"
        );
    }

    #[tokio::test]
    async fn test_module_graph_pane_loads_outgoing_and_switches_to_incoming() {
        let dir = tempfile::tempdir().unwrap();
        let app_source =
            "import { helper } from './helper';\nexport function app() { return helper(); }\n";
        let helper_source = "export function helper() { return 1; }\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[("src/app.ts", app_source), ("src/helper.ts", helper_source)],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/helper.ts"]);
        app.state = AppState::RepoBrowseFile;

        app.open_browse_module_graph();
        assert_eq!(app.state, AppState::RepoBrowseGraph);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            ModuleGraphPaneState::Loading { .. }
        ));
        settle_module_graph(&mut app).await;
        let state = app.browse_state.as_ref().unwrap();
        let ModuleGraphPaneState::Ready(panel) = &state.module_graph_pane else {
            panic!("module graph pane must become ready");
        };
        assert_eq!(panel.direction, ModuleGraphDirection::Dependencies);
        assert_eq!(panel.current_rows().len(), 1);
        assert!(panel.current_rows()[0].edge.contains("./helper"));
        assert_eq!(panel.current_rows()[0].node, "src/helper.ts");

        app.handle_repo_browse_graph_input(press(KeyCode::Tab));
        let state = app.browse_state.as_ref().unwrap();
        let ModuleGraphPaneState::Ready(panel) = &state.module_graph_pane else {
            panic!("module graph pane must stay open");
        };
        assert_eq!(panel.direction, ModuleGraphDirection::Dependents);
        assert!(panel.current_rows().is_empty());
    }

    #[tokio::test]
    async fn test_default_and_custom_sequence_module_graph_keys_open_the_pane() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "import { helper } from './helper';\n";
        let helper_source = "export const helper = 1;\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[("src/app.ts", app_source), ("src/helper.ts", helper_source)],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/helper.ts"]);
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('i')), &mut test_terminal())
            .unwrap();
        settle_module_graph(&mut app).await;
        assert_eq!(app.state, AppState::RepoBrowseGraph);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            ModuleGraphPaneState::Ready(_)
        ));

        app.handle_repo_browse_graph_input(press(KeyCode::Char('i')));
        assert_eq!(app.state, AppState::RepoBrowseFile);
        app.config.keybindings.module_graph =
            KeySequence::double(KeyBinding::char('m'), KeyBinding::char('i'));
        app.handle_repo_browse_file_input(press(KeyCode::Char('m')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('i')), &mut test_terminal())
            .unwrap();
        settle_module_graph(&mut app).await;
        assert_eq!(app.state, AppState::RepoBrowseGraph);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            ModuleGraphPaneState::Ready(_)
        ));
    }

    #[tokio::test]
    async fn test_loading_module_graph_can_be_cancelled_without_installing_a_stale_panel() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "import './helper';\n";
        let helper_source = "export const helper = 1;\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[("src/app.ts", app_source), ("src/helper.ts", helper_source)],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/helper.ts"]);
        app.state = AppState::RepoBrowseFile;

        app.open_browse_module_graph();
        let cancel = app
            .browse_state
            .as_ref()
            .unwrap()
            .module_graph_query_token()
            .expect("query token");
        assert!(!cancel.is_cancelled());
        app.handle_repo_browse_graph_input(press(KeyCode::Char('i')));
        assert!(cancel.is_cancelled());
        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Closed
        ));
        assert!(state.module_graph_query_receiver.is_none());
        assert_eq!(app.state, AppState::RepoBrowseFile);
    }

    #[tokio::test]
    async fn test_loading_module_graph_restarts_when_the_open_path_changes() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "import './helper';\n";
        let helper_source = "export const helper = 1;\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[("src/app.ts", app_source), ("src/helper.ts", helper_source)],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/helper.ts"]);
        app.state = AppState::RepoBrowseFile;

        app.open_browse_module_graph();
        let cancel = app
            .browse_state
            .as_ref()
            .unwrap()
            .module_graph_query_token()
            .expect("query token");
        attach_open_file(&mut app, "src/helper.ts", helper_source, Vec::new());
        app.poll_browse_updates();

        assert!(cancel.is_cancelled());
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            &state.module_graph_pane,
            ModuleGraphPaneState::Loading { path, .. } if path == "src/helper.ts"
        ));
        assert!(state.module_graph_query_receiver.is_some());
        assert!(state.status.is_none());
    }

    #[test]
    fn test_enter_on_an_empty_module_graph_direction_is_a_noop() {
        let mut app = browsing_app(&["src/empty.ts"]);
        attach_open_file(&mut app, "src/empty.ts", "export {};\n", Vec::new());
        app.state = AppState::RepoBrowseGraph;
        app.browse_state.as_mut().unwrap().module_graph_pane =
            ModuleGraphPaneState::Ready(super::super::browse::ModuleGraphPanel {
                path: "src/empty.ts".to_string(),
                display_path: "src/empty.ts".to_string(),
                direction: ModuleGraphDirection::Dependencies,
                selected: 0,
                dependencies: super::super::browse::ModuleGraphRows {
                    rows: Vec::new(),
                    total: 0,
                    guarantee: crate::module_graph::DependencyGuarantee::Exact,
                },
                dependents: super::super::browse::ModuleGraphRows {
                    rows: Vec::new(),
                    total: 0,
                    guarantee: crate::module_graph::DependencyGuarantee::Exact,
                },
            });

        app.handle_repo_browse_graph_input(press(KeyCode::Enter));

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Ready(_)
        ));
        assert!(state.status.is_none());
        assert!(state.jump_stack.is_empty());
    }

    #[test]
    fn test_non_navigable_module_graph_row_reports_instead_of_jumping() {
        let mut app = browsing_app(&["src/app.ts"]);
        attach_open_file(
            &mut app,
            "src/app.ts",
            "import react from 'react';\n",
            Vec::new(),
        );
        app.state = AppState::RepoBrowseGraph;
        app.browse_state.as_mut().unwrap().module_graph_pane =
            ModuleGraphPaneState::Ready(super::super::browse::ModuleGraphPanel {
                path: "src/app.ts".to_string(),
                display_path: "src/app.ts".to_string(),
                direction: ModuleGraphDirection::Dependencies,
                selected: 0,
                dependencies: super::super::browse::ModuleGraphRows {
                    rows: vec![super::super::browse::ModuleGraphRow {
                        node: "package react".to_string(),
                        edge: "[import] react  :1".to_string(),
                        jump: None,
                    }],
                    total: 1,
                    guarantee: crate::module_graph::DependencyGuarantee::Exact,
                },
                dependents: super::super::browse::ModuleGraphRows {
                    rows: Vec::new(),
                    total: 0,
                    guarantee: crate::module_graph::DependencyGuarantee::Exact,
                },
            });

        app.handle_repo_browse_graph_input(press(KeyCode::Enter));

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Ready(_)
        ));
        assert_eq!(
            state.status.as_deref(),
            Some("This dependency target is not a listed repository file")
        );
        assert!(state.jump_stack.is_empty());
    }

    #[tokio::test]
    async fn test_module_graph_opens_a_listed_cjk_json_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "import data from './データ.json';\nexport { data };\n";
        let json_source = "{\"value\":1}\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[("src/app.ts", app_source), ("src/データ.json", json_source)],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/データ.json"]);
        app.state = AppState::RepoBrowseFile;

        app.open_browse_module_graph();
        settle_module_graph(&mut app).await;
        app.handle_repo_browse_graph_input(press(KeyCode::Enter));
        settle_browse(&mut app).await;

        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(open_path(&app), "src/データ.json");
        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Import analysis is not supported for this file")
        );
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            ModuleGraphPaneState::Closed
        ));
    }

    #[tokio::test]
    async fn test_module_graph_pane_jumps_to_local_dependency_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let app_source =
            "import { helper } from './helper';\nexport function app() { return helper(); }\n";
        let helper_source = "export function helper() { return 1; }\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[("src/app.ts", app_source), ("src/helper.ts", helper_source)],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/helper.ts"]);
        app.state = AppState::RepoBrowseFile;
        app.browse_state.as_mut().unwrap().cursor_line = 1;

        app.open_browse_module_graph();
        settle_module_graph(&mut app).await;
        app.handle_repo_browse_graph_input(press(KeyCode::Enter));
        settle_browse(&mut app).await;
        settle_module_graph(&mut app).await;
        assert_eq!(open_path(&app), "src/helper.ts");
        assert!(matches!(
            app.browse_state.as_ref().unwrap().module_graph_pane,
            ModuleGraphPaneState::Ready(_)
        ));

        app.handle_repo_browse_file_input(ctrl('o'), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;
        settle_module_graph(&mut app).await;
        assert_eq!(open_path(&app), "src/app.ts");
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 1);
    }

    #[tokio::test]
    async fn test_browse_graph_mouse_helpers_follow_key_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "import { helper } from './helper';\nimport { util } from './util';\n";
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[
                ("src/app.ts", app_source),
                ("src/helper.ts", "export function helper() { return 1; }\n"),
                ("src/util.ts", "export function util() { return 2; }\n"),
            ],
        );
        attach_open_file(&mut app, "src/app.ts", app_source, Vec::new());
        attach_code_index(
            &mut app,
            dir.path(),
            &["src/app.ts", "src/helper.ts", "src/util.ts"],
        );
        app.state = AppState::RepoBrowseFile;

        app.open_browse_module_graph();
        settle_module_graph(&mut app).await;
        app.state = AppState::RepoBrowseGraph;

        // ホイール相当: 1 行下 → クランプ確認
        app.browse_graph_move(true);
        let selected = |app: &App| {
            let ModuleGraphPaneState::Ready(panel) =
                &app.browse_state.as_ref().unwrap().module_graph_pane
            else {
                panic!("panel not ready");
            };
            panel.selected
        };
        assert_eq!(selected(&app), 1);
        app.browse_graph_move(true);
        assert_eq!(selected(&app), 1, "末尾でクランプ(依存 2 件)");
        app.browse_graph_move(false);
        assert_eq!(selected(&app), 0);

        // クリック: 未選択行は選択のみ、選択済み行は true
        assert!(!app.browse_graph_click_select(1));
        assert_eq!(selected(&app), 1);
        assert!(app.browse_graph_click_select(1));

        // タイトルクリックで方向トグル
        let direction = |app: &App| {
            let ModuleGraphPaneState::Ready(panel) =
                &app.browse_state.as_ref().unwrap().module_graph_pane
            else {
                panic!("panel not ready");
            };
            panel.direction
        };
        let before = direction(&app);
        app.browse_graph_toggle_direction();
        assert_ne!(direction(&app), before);
    }

    #[test]
    fn test_browse_graph_mouse_helpers_are_noop_without_panel() {
        let mut app = App::new_cockpit("owner/repo", crate::config::Config::default(), true);
        app.browse_graph_move(true);
        assert!(!app.browse_graph_click_select(0));
        app.browse_graph_toggle_direction();
        app.browse_graph_open_selected();
    }

    #[test]
    fn test_failed_module_graph_action_reports_unavailable() {
        let mut app = browsing_app(&["src/app.ts"]);
        attach_open_file(&mut app, "src/app.ts", "export {};\n", Vec::new());
        app.state = AppState::RepoBrowseFile;
        app.browse_state.as_mut().unwrap().module_graph = ModuleGraphState::Failed;

        app.open_browse_module_graph();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Closed
        ));
        assert_eq!(state.status.as_deref(), Some("Module graph is unavailable"));
    }

    #[test]
    fn test_module_graph_reports_unavailable_for_a_listed_unanalyzed_stub() {
        let dir = tempfile::tempdir().unwrap();
        let app_source = "import './oversized';\n";
        let oversized = "x".repeat(crate::symbols::MAX_INDEXED_FILE_BYTES as usize + 1);
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[
                ("src/app.ts", app_source),
                ("src/oversized.ts", oversized.as_str()),
            ],
        );
        attach_open_file(&mut app, "src/oversized.ts", "", Vec::new());
        attach_code_index(&mut app, dir.path(), &["src/app.ts", "src/oversized.ts"]);
        app.state = AppState::RepoBrowseFile;

        app.open_browse_module_graph();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Closed
        ));
        assert_eq!(
            state.status.as_deref(),
            Some("Import analysis is unavailable for this file")
        );
    }

    #[test]
    fn test_module_graph_action_prioritizes_a_pending_file_load() {
        let mut app = browsing_app(&["src/app.ts"]);
        attach_open_file(&mut app, "src/app.ts", "import './helper';\n", Vec::new());
        let cancel = tokio_util::sync::CancellationToken::new();
        app.browse_state.as_mut().unwrap().open_load = OpenLoad::Pending {
            path: "src/app.ts".to_string(),
            line: 0,
            scroll: None,
            cancel,
        };

        app.open_browse_module_graph();

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.status.as_deref(), Some("Still opening this file"));
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Closed
        ));
    }

    /// A throwaway terminal for handlers that take one but do not draw.
    fn test_terminal() -> Terminal<CrosstermBackend<Stdout>> {
        Terminal::with_options(
            CrosstermBackend::new(std::io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 120, 40)),
            },
        )
        .expect("terminal")
    }
}
