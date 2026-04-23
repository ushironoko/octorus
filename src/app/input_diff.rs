use anyhow::Result;
use crossterm::event::{self, KeyCode, KeyModifiers};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;

use crate::filter::ListFilter;
use crate::keybinding::{event_to_keybinding, SequenceMatch};

use super::types::*;
use super::{App, AppState};

impl App {
    pub(crate) async fn handle_split_view_file_list_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        if self.handle_filter_input(&key, "file") {
            self.sync_diff_to_selected_file();
            return Ok(());
        }

        let kb = self.config.keybindings.clone();
        let has_filter = self.file_list_filter.is_some();
        let tree_active = self.is_file_tree_active();

        if self.matches_single_key(&key, &kb.tree_toggle) && !has_filter {
            self.toggle_file_tree();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_down) {
            if has_filter {
                self.handle_filter_navigation("file", true);
            } else if tree_active {
                self.file_tree_move_down();
            } else if !self.files().is_empty() {
                self.selected_file =
                    (self.selected_file + 1).min(self.files().len().saturating_sub(1));
            }
            // File 行移動時のみ diff 同期（Dir 行ではスキップ → 直前ファイルの diff を維持）
            if !tree_active || self.file_tree_state.as_ref().is_none_or(|t| t.selected_file_index().is_some()) {
                self.sync_diff_to_selected_file();
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_up) {
            if has_filter {
                self.handle_filter_navigation("file", false);
            } else if tree_active {
                self.file_tree_move_up();
            } else if self.selected_file > 0 {
                self.selected_file = self.selected_file.saturating_sub(1);
            }
            if !tree_active || self.file_tree_state.as_ref().is_none_or(|t| t.selected_file_index().is_some()) {
                self.sync_diff_to_selected_file();
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_down) || Self::is_shift_char_shortcut(&key, 'j') {
            if !has_filter {
                let page_step = terminal.size()?.height.saturating_sub(8) as usize;
                let step = page_step.max(1);
                if tree_active {
                    self.file_tree_page_down(step);
                } else if !self.files().is_empty() {
                    self.selected_file =
                        (self.selected_file + step).min(self.files().len().saturating_sub(1));
                }
                if !tree_active || self.file_tree_state.as_ref().is_none_or(|t| t.selected_file_index().is_some()) {
                    self.sync_diff_to_selected_file();
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_up) || Self::is_shift_char_shortcut(&key, 'k') {
            if !has_filter {
                let page_step = terminal.size()?.height.saturating_sub(8) as usize;
                let step = page_step.max(1);
                if tree_active {
                    self.file_tree_page_up(step);
                } else {
                    self.selected_file = self.selected_file.saturating_sub(step);
                }
                if !tree_active || self.file_tree_state.as_ref().is_none_or(|t| t.selected_file_index().is_some()) {
                    self.sync_diff_to_selected_file();
                }
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            if self.handle_filter_esc("file") {
                return Ok(());
            }
            self.state = AppState::FileList;
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.git_ops) {
            self.open_git_ops();
            return Ok(());
        }

        if let Some(kb_event) = event_to_keybinding(&key) {
            self.check_sequence_timeout();

            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);

                if self.try_match_sequence(&kb.filter) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    if let Some(ref mut filter) = self.file_list_filter {
                        filter.input_active = true;
                    } else {
                        let mut filter = ListFilter::new();
                        let files = self.files();
                        filter.apply(files, |_file, _q| true);
                        if let Some(idx) = filter.sync_selection() {
                            self.selected_file = idx;
                        }
                        self.file_list_filter = Some(filter);
                    }
                    return Ok(());
                }

                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    if tree_active {
                        self.file_tree_jump_to_first();
                    } else {
                        self.selected_file = 0;
                    }
                    self.sync_diff_to_selected_file();
                    return Ok(());
                }

                self.clear_pending_keys();
            } else {
                let could_start_filter = self.key_could_match_sequence(&key, &kb.filter);
                let could_start_gg = self.key_could_match_sequence(&key, &kb.jump_to_first);
                if could_start_filter || could_start_gg {
                    self.push_pending_key(kb_event);
                    return Ok(());
                }
            }
        }

        // G: Jump to last
        if self.matches_single_key(&key, &kb.jump_to_last) {
            if tree_active {
                self.file_tree_jump_to_last();
            } else if !self.files().is_empty() {
                self.selected_file = self.files().len().saturating_sub(1);
            }
            self.sync_diff_to_selected_file();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.open_panel)
            || self.matches_single_key(&key, &kb.move_right)
                   {
            if self.is_filter_selection_empty("file") {
                return Ok(());
            }
            if tree_active && self.file_tree_enter() {
                return Ok(());
            }
            if !self.files().is_empty() {
                self.state = AppState::SplitViewDiff;
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_left) {
            self.state = AppState::FileList;
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.comment_list) {
            self.previous_state = AppState::SplitViewFileList;
            self.open_comment_list();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.help) {
            self.open_help(AppState::SplitViewFileList);
            return Ok(());
        }

        self.handle_common_file_list_keys(key, terminal).await?;

        Ok(())
    }
    pub(super) async fn handle_diff_input_common(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        variant: DiffViewVariant,
    ) -> Result<()> {
        if self.symbol_popup.is_some() {
            self.handle_symbol_popup_input(key, terminal).await?;
            return Ok(());
        }

        if self.matches_single_key(&key, &self.config.keybindings.help) {
            let from = match variant {
                DiffViewVariant::SplitPane => AppState::SplitViewDiff,
                DiffViewVariant::Fullscreen => AppState::DiffView,
            };
            self.open_help(from);
            return Ok(());
        }

        let term_size = terminal.size()?;
        let term_h = term_size.height as usize;
        let term_w = term_size.width as usize;

        // Calculate visible_lines matching the actual rendered diff body height.
        // When the comment panel is closed, the layout is:
        //   Header(3) + Diff(remaining) + Footer(3) + border(2) = term_h - 8
        // When the comment panel is open, the diff body only gets a percentage of
        // the available space (after subtracting fixed-height regions), so we must
        // replicate that proportional split here to keep adjust_scroll() accurate.
        let visible_lines = if self.cmt.comment_panel_open {
            let has_rally = self.has_background_rally();
            // Fixed-height rows consumed by Header + Footer (+ optional Rally bar)
            let fixed = if has_rally { 7usize } else { 6usize };
            let remaining = term_h.saturating_sub(fixed);
            // Diff percentage / total percentage — must match the rendering layout:
            //   Fullscreen without rally: 55% diff + 40% comments = 95%
            //   Fullscreen with rally:    50% diff + 40% comments = 90%
            //   SplitPane (both):         50% diff + 40% comments = 90%
            let (diff_pct, total_pct) = match variant {
                DiffViewVariant::Fullscreen if !has_rally => (55usize, 95usize),
                _ => (50, 90),
            };
            let diff_area_h = remaining * diff_pct / total_pct;
            // Subtract 2 for the Block borders around the diff body
            diff_area_h.saturating_sub(2)
        } else {
            // Header(3) + Footer(3) + border(2) = 8
            term_h.saturating_sub(8)
        };
        let panel_inner_width = self.comment_panel_inner_width(term_w);

        let kb = self.config.keybindings.clone();

        if self.multiline_selection.is_some() {
            if self.matches_single_key(&key, &kb.move_down) {
                if self.diff_scroll.line_count > 0 {
                    let new_cursor =
                        (self.diff_scroll.selected_line + 1).min(self.diff_scroll.line_count.saturating_sub(1));
                    self.diff_scroll.selected_line = new_cursor;
                    if let Some(ref mut sel) = self.multiline_selection {
                        sel.cursor_line = new_cursor;
                    }
                    self.adjust_scroll(visible_lines);
                }
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.move_up) {
                let new_cursor = self.diff_scroll.selected_line.saturating_sub(1);
                self.diff_scroll.selected_line = new_cursor;
                if let Some(ref mut sel) = self.multiline_selection {
                    sel.cursor_line = new_cursor;
                }
                self.adjust_scroll(visible_lines);
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.comment) {
                self.enter_multiline_comment_input();
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.suggestion) {
                self.enter_multiline_suggestion_input();
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.quit) {
                self.multiline_selection = None;
                return Ok(());
            }

            return Ok(());
        }

        if self.cmt.comment_panel_open {
            if self.matches_single_key(&key, &kb.move_down) {
                let max_scroll = self.max_comment_panel_scroll(term_h, term_w);
                self.cmt.comment_panel_scroll =
                    self.cmt.comment_panel_scroll.saturating_add(1).min(max_scroll);
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.move_up) {
                self.cmt.comment_panel_scroll = self.cmt.comment_panel_scroll.saturating_sub(1);
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.next_comment) {
                let prev_line = self.diff_scroll.selected_line;
                self.jump_to_next_comment();
                if self.diff_scroll.selected_line != prev_line {
                    self.cmt.comment_panel_scroll = 0;
                    self.cmt.selected_inline_comment = 0;
                    self.adjust_scroll(visible_lines);
                }
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.prev_comment) {
                let prev_line = self.diff_scroll.selected_line;
                self.jump_to_prev_comment();
                if self.diff_scroll.selected_line != prev_line {
                    self.cmt.comment_panel_scroll = 0;
                    self.cmt.selected_inline_comment = 0;
                    self.adjust_scroll(visible_lines);
                }
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.comment) {
                self.enter_comment_input();
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.suggestion) {
                self.enter_suggestion_input();
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.reply) {
                if self.has_comment_at_current_line() {
                    self.enter_reply_input();
                }
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.tab_switch) {
                if self.has_comment_at_current_line() {
                    let count = self.get_comment_indices_at_current_line().len();
                    if count > 1 && self.cmt.selected_inline_comment + 1 < count {
                        self.cmt.selected_inline_comment += 1;
                        self.cmt.comment_panel_scroll = self.comment_panel_offset_for(
                            self.cmt.selected_inline_comment,
                            panel_inner_width,
                        );
                    }
                }
                return Ok(());
            }

            if key.code == KeyCode::BackTab {
                if self.has_comment_at_current_line() {
                    let count = self.get_comment_indices_at_current_line().len();
                    if count > 1 && self.cmt.selected_inline_comment > 0 {
                        self.cmt.selected_inline_comment -= 1;
                        self.cmt.comment_panel_scroll = self.comment_panel_offset_for(
                            self.cmt.selected_inline_comment,
                            panel_inner_width,
                        );
                    }
                }
                return Ok(());
            }

            match variant {
                DiffViewVariant::SplitPane => {
                    if self.matches_single_key(&key, &kb.move_right) {
                        self.diff_view_return_state = AppState::SplitViewDiff;
                        self.preview_return_state = AppState::DiffView;
                        self.state = AppState::DiffView;
                        return Ok(());
                    }

                    if self.matches_single_key(&key, &kb.quit)
                        || self.matches_single_key(&key, &kb.move_left)
                                                                  {
                        self.cmt.comment_panel_open = false;
                        self.cmt.comment_panel_scroll = 0;
                        return Ok(());
                    }
                }
                DiffViewVariant::Fullscreen => {
                    if self.matches_single_key(&key, &kb.move_left) {
                        self.state = self.diff_view_return_state;
                        return Ok(());
                    }

                    if self.matches_single_key(&key, &kb.quit) {
                        self.cmt.comment_panel_open = false;
                        self.cmt.comment_panel_scroll = 0;
                        return Ok(());
                    }
                }
            }

            return Ok(());
        }

        self.check_sequence_timeout();

        let current_kb = event_to_keybinding(&key);

        if let Some(kb_event) = current_kb {
            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);

                if self.try_match_sequence(&kb.go_to_definition) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    self.open_symbol_popup(terminal).await?;
                    return Ok(());
                }

                if self.try_match_sequence(&kb.go_to_file) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    self.open_current_file_in_editor(terminal).await?;
                    return Ok(());
                }

                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    self.diff_scroll.selected_line = 0;
                    self.diff_scroll.scroll_offset = 0;
                    return Ok(());
                }

                self.clear_pending_keys();
            } else {
                let could_start_gd = self.key_could_match_sequence(&key, &kb.go_to_definition);
                let could_start_gf = self.key_could_match_sequence(&key, &kb.go_to_file);
                let could_start_gg = self.key_could_match_sequence(&key, &kb.jump_to_first);

                if could_start_gd || could_start_gf || could_start_gg {
                    self.push_pending_key(kb_event);
                    return Ok(());
                }
            }
        }

        match variant {
            DiffViewVariant::SplitPane => {
                if self.matches_single_key(&key, &kb.move_right) {
                    self.diff_view_return_state = AppState::SplitViewDiff;
                    self.preview_return_state = AppState::DiffView;
                    self.state = AppState::DiffView;
                    return Ok(());
                }

                if self.matches_single_key(&key, &kb.move_left) {
                    self.state = AppState::SplitViewFileList;
                    return Ok(());
                }

                if self.matches_single_key(&key, &kb.quit) {
                    self.state = AppState::FileList;
                    return Ok(());
                }

                if self.matches_single_key(&key, &kb.comment) {
                    self.enter_comment_input();
                    return Ok(());
                }

                if self.matches_single_key(&key, &kb.suggestion) {
                    self.enter_suggestion_input();
                    return Ok(());
                }
            }
            DiffViewVariant::Fullscreen => {
                if self.matches_single_key(&key, &kb.quit) {
                    self.handle_fullscreen_diff_quit();
                    return Ok(());
                }

                if self.matches_single_key(&key, &kb.move_left) {
                    self.state = self.diff_view_return_state;
                    return Ok(());
                }
            }
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.pr_description) {
            self.open_pr_description();
            return Ok(());
        }

        if (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT))
            || self.matches_single_key(&key, &kb.multiline_select)
        {
            self.enter_multiline_selection();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_down) {
            if self.diff_scroll.line_count > 0 {
                self.diff_scroll.selected_line =
                    (self.diff_scroll.selected_line + 1).min(self.diff_scroll.line_count.saturating_sub(1));
                self.adjust_scroll(visible_lines);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_up) {
            self.diff_scroll.selected_line = self.diff_scroll.selected_line.saturating_sub(1);
            self.adjust_scroll(visible_lines);
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.jump_to_last) {
            if self.diff_scroll.line_count > 0 {
                self.diff_scroll.selected_line = self.diff_scroll.line_count.saturating_sub(1);
                self.adjust_scroll(visible_lines);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.jump_back) {
            self.jump_back();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_down) || Self::is_shift_char_shortcut(&key, 'j') {
            if self.diff_scroll.line_count > 0 {
                self.diff_scroll.selected_line =
                    (self.diff_scroll.selected_line + 20).min(self.diff_scroll.line_count.saturating_sub(1));
                self.adjust_scroll(visible_lines);
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.page_up) || Self::is_shift_char_shortcut(&key, 'k') {
            self.diff_scroll.selected_line = self.diff_scroll.selected_line.saturating_sub(20);
            self.adjust_scroll(visible_lines);
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.next_comment) {
            self.jump_to_next_comment();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.prev_comment) {
            self.jump_to_prev_comment();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.toggle_markdown_rich) {
            self.toggle_markdown_rich();
            self.ensure_diff_cache();
            return Ok(());
        }

        // Open panel (local mode ではコメント対象の PR がないため無効)
        if !self.local_mode && self.matches_single_key(&key, &kb.open_panel) {
            self.cmt.comment_panel_open = true;
            self.cmt.comment_panel_scroll = 0;
            self.cmt.selected_inline_comment = 0;
            return Ok(());
        }

        if variant == DiffViewVariant::Fullscreen {
            if self.matches_single_key(&key, &kb.comment) {
                self.enter_comment_input();
                return Ok(());
            }

            if self.matches_single_key(&key, &kb.suggestion) {
                self.enter_suggestion_input();
                return Ok(());
            }
        }

        Ok(())
    }
    pub(crate) fn handle_fullscreen_diff_quit(&mut self) {
        if self.started_from_pr_list
            && self.diff_view_return_state == AppState::FileList
            && !self.zen_mode
        {
            self.back_to_pr_list();
        } else {
            self.state = self.diff_view_return_state;
        }
    }

    pub(crate) async fn handle_split_view_diff_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        self.handle_diff_input_common(key, terminal, DiffViewVariant::SplitPane)
            .await
    }
    pub(crate) async fn handle_diff_view_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        self.handle_diff_input_common(key, terminal, DiffViewVariant::Fullscreen)
            .await
    }
    pub(crate) fn adjust_scroll(&mut self, visible_lines: usize) {
        if visible_lines == 0 {
            return;
        }
        // When the entire diff fits within the viewport, no scrolling is needed.
        // Reset scroll_offset to prevent stale state from hiding lines after refresh.
        if self.diff_scroll.line_count <= visible_lines {
            self.diff_scroll.scroll_offset = 0;
            return;
        }

        // Scroll margin: keep cursor at least this many lines from viewport edges.
        // Uses half the viewport so scrolling begins when cursor passes the center.
        let margin = visible_lines / 2;

        // Cursor above the top margin
        if self.diff_scroll.selected_line < self.diff_scroll.scroll_offset + margin {
            self.diff_scroll.scroll_offset = self.diff_scroll.selected_line.saturating_sub(margin);
        }
        // Cursor below the bottom margin
        if self.diff_scroll.selected_line + margin >= self.diff_scroll.scroll_offset + visible_lines {
            self.diff_scroll.scroll_offset = self
                .diff_scroll
                .selected_line
                .saturating_sub(visible_lines.saturating_sub(margin + 1));
        }
    }
}
