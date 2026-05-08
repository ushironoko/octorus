use anyhow::Result;
use crossterm::event;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;

use crate::keybinding::{event_to_keybinding, SequenceMatch};

use super::types::*;
use super::App;

impl App {
    pub(crate) fn push_jump_location(&mut self) {
        let loc = JumpLocation {
            file_index: self.selected_file,
            line_index: self.diff_scroll.selected_line,
            scroll_offset: self.diff_scroll.scroll_offset,
        };
        self.jump_stack.push(loc);
        // 上限 100 件
        if self.jump_stack.len() > 100 {
            self.jump_stack.remove(0);
        }
    }

    /// ジャンプスタックから復元
    pub(crate) fn jump_back(&mut self) {
        let Some(loc) = self.jump_stack.pop() else {
            return;
        };

        let file_changed = self.selected_file != loc.file_index;
        self.selected_file = loc.file_index;
        self.diff_scroll.selected_line = loc.line_index;
        self.diff_scroll.scroll_offset = loc.scroll_offset;

        if file_changed {
            self.update_diff_line_count();
            self.update_file_comment_positions();
            self.ensure_diff_cache();
        }
    }

    /// シンボル選択ポップアップを開く
    pub(crate) async fn open_symbol_popup(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let file = match self.files().get(self.selected_file) {
            Some(f) => f,
            None => return Ok(()),
        };
        let patch = match file.patch.as_ref() {
            Some(p) => p,
            None => return Ok(()),
        };
        let info = match crate::diff::get_line_info(patch, self.diff_scroll.selected_line) {
            Some(i) => i,
            None => return Ok(()),
        };

        let symbols = crate::symbol::extract_all_identifiers(&info.line_content);
        if symbols.is_empty() {
            return Ok(());
        }

        // 候補が1つだけの場合は直接ジャンプ（ポップアップ不要）
        if symbols.len() == 1 {
            let symbol_name = symbols[0].0.clone();
            self.jump_to_symbol_definition_async(&symbol_name, terminal)
                .await?;
            return Ok(());
        }

        self.symbol_popup = Some(SymbolPopupState {
            symbols,
            selected: 0,
        });
        Ok(())
    }

    /// ポップアップ内のキーハンドリング
    pub(crate) async fn handle_symbol_popup_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        if self.symbol_popup.is_none() {
            return Ok(());
        }

        let kb = self.config.keybindings.clone();
        if self.matches_single_key(&key, &kb.move_down) {
            let popup = self.symbol_popup.as_mut().unwrap();
            popup.selected = (popup.selected + 1).min(popup.symbols.len().saturating_sub(1));
        } else if self.matches_single_key(&key, &kb.move_up) {
            let popup = self.symbol_popup.as_mut().unwrap();
            popup.selected = popup.selected.saturating_sub(1);
        } else if self.matches_single_key(&key, &kb.open_panel) {
            let symbol_name = self.symbol_popup.as_ref().unwrap().symbols
                [self.symbol_popup.as_ref().unwrap().selected]
                .0
                .clone();
            self.symbol_popup = None;
            self.jump_to_symbol_definition_async(&symbol_name, terminal)
                .await?;
        } else if self.matches_single_key(&key, &kb.quit) {
            self.symbol_popup = None;
        }
        Ok(())
    }

    /// シンボルの定義元へジャンプ（diff パッチ内 → リポジトリ全体、非同期）
    pub(crate) async fn jump_to_symbol_definition_async(
        &mut self,
        symbol: &str,
        _terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        // Phase 1: diff パッチ内を検索
        let files: Vec<crate::github::ChangedFile> = self.files().to_vec();
        if let Some((file_idx, line_idx)) =
            crate::symbol::find_definition_in_patches(symbol, &files, self.selected_file)
        {
            self.push_jump_location();
            let file_changed = self.selected_file != file_idx;
            self.selected_file = file_idx;
            self.diff_scroll.selected_line = line_idx;
            self.diff_scroll.scroll_offset = line_idx;

            if file_changed {
                self.update_diff_line_count();
                self.update_file_comment_positions();
                self.ensure_diff_cache();
            }
            return Ok(());
        }

        // Phase 2: ローカルリポジトリ全体を非同期検索
        let repo_root = match &self.working_dir {
            Some(dir) => {
                let output = tokio::process::Command::new("git")
                    .args(["rev-parse", "--show-toplevel"])
                    .current_dir(dir)
                    .output()
                    .await;
                match output {
                    Ok(o) if o.status.success() => {
                        String::from_utf8_lossy(&o.stdout).trim().to_string()
                    }
                    _ => return Ok(()),
                }
            }
            None => return Ok(()),
        };

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let symbol_owned = symbol.to_string();
        let repo_root_owned = repo_root.clone();
        let origin_file_index = self.selected_file;

        tokio::spawn(async move {
            let result = crate::symbol::find_definition_in_repo(
                &symbol_owned,
                std::path::Path::new(&repo_root_owned),
            )
            .await;

            let update = match result {
                Ok(Some((file_path, line_number))) => {
                    super::SymbolSearchUpdate::Found(super::RepoSymbolSearchResult {
                        file_path,
                        line_number,
                        repo_root: repo_root_owned,
                    })
                }
                Ok(None) => super::SymbolSearchUpdate::NotFound,
                Err(e) => super::SymbolSearchUpdate::Failed(e.to_string()),
            };

            let _ = tx.send(update).await;
        });

        self.cmt.submission_result = None;
        self.cmt.submission_result_time = None;
        self.symbol_search = super::SymbolSearchState::Searching {
            receiver: rx,
            origin_file_index,
        };

        Ok(())
    }

    /// 現在のファイルを外部エディタで開く（gf キー）
    pub(crate) async fn open_current_file_in_editor(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let file = match self.files().get(self.selected_file) {
            Some(f) => f.clone(),
            None => return Ok(()),
        };

        // 行番号: new_line_number があれば使用、なければ 1
        let line_number = file.patch.as_ref().and_then(|patch| {
            crate::diff::get_line_info(patch, self.diff_scroll.selected_line)
                .and_then(|info| info.new_line_number)
        });

        // リポジトリルート取得 → フルパス構築
        let full_path = match &self.working_dir {
            Some(dir) => {
                let output = tokio::process::Command::new("git")
                    .args(["rev-parse", "--show-toplevel"])
                    .current_dir(dir)
                    .output()
                    .await;
                match output {
                    Ok(o) if o.status.success() => {
                        let root = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        std::path::Path::new(&root)
                            .join(&file.filename)
                            .to_string_lossy()
                            .to_string()
                    }
                    _ => return Ok(()),
                }
            }
            None => return Ok(()),
        };

        // TUI 一時停止 → エディタ → TUI 復帰
        crate::ui::restore_terminal(terminal)?;
        let _ = crate::editor::open_file_at_line(
            self.config.editor.as_deref(),
            &full_path,
            line_number.unwrap_or(1) as usize,
        );
        *terminal = crate::ui::setup_terminal()?;

        Ok(())
    }

    pub(crate) fn handle_pr_description_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let terminal_height = terminal.size()?.height;
        // Viewport overhead: header (3) + body borders (2) + footer (1) = 6
        let visible_lines = terminal_height.saturating_sub(6) as usize;
        let half_page = (visible_lines / 2).max(1);

        let kb = self.config.keybindings.clone();

        if self.matches_single_key(&key, &kb.help) {
            self.open_help(AppState::PrDescription);
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            self.state = self.previous_state;
            return Ok(());
        }

        if !self.local_mode && self.matches_single_key(&key, &kb.open_in_browser) {
            if let Some(pr_number) = self.pr_number {
                self.open_pr_in_browser(pr_number);
            }
            return Ok(());
        }

        // IMPORTANT: ここでは toggle_markdown_rich() を呼ばず、フラグ反転と PR description
        // キャッシュの再構築のみ行う。toggle_markdown_rich() は prefetch_receiver の破棄や
        // highlighted_cache_store のmarkdownエントリ削除など DiffView 向けの副作用を持つため、
        // ファイル diff に戻った際は ensure_diff_cache() が markdown_rich の不整合を検出し、
        // markdownファイルのみ自動再構築する。
        if self.matches_single_key(&key, &kb.toggle_markdown_rich) {
            self.markdown_rich = !self.markdown_rich;
            self.pr_description_cache = None;
            self.rebuild_pr_description_cache();
            return Ok(());
        }

        if let Some(kb_event) = event_to_keybinding(&key) {
            self.check_sequence_timeout();
            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);
                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    self.pr_description_scroll_offset = 0;
                    return Ok(());
                }
                self.clear_pending_keys();
            } else if self.key_could_match_sequence(&key, &kb.jump_to_first) {
                self.push_pending_key(kb_event);
                return Ok(());
            }
        }

        if Self::is_shift_char_shortcut(&key, 'j') {
            self.pr_description_scroll_offset = self
                .pr_description_scroll_offset
                .saturating_add(visible_lines.max(1));
        } else if Self::is_shift_char_shortcut(&key, 'k') {
            self.pr_description_scroll_offset = self
                .pr_description_scroll_offset
                .saturating_sub(visible_lines.max(1));
        } else if Self::is_shift_char_shortcut(&key, 'g') {
            self.pr_description_scroll_offset = usize::MAX;
        } else if self.matches_single_key(&key, &kb.move_down) {
            self.pr_description_scroll_offset = self.pr_description_scroll_offset.saturating_add(1);
        } else if self.matches_single_key(&key, &kb.move_up) {
            self.pr_description_scroll_offset = self.pr_description_scroll_offset.saturating_sub(1);
        } else if self.matches_single_key(&key, &kb.page_down) {
            self.pr_description_scroll_offset =
                self.pr_description_scroll_offset.saturating_add(half_page);
        } else if self.matches_single_key(&key, &kb.page_up) {
            self.pr_description_scroll_offset =
                self.pr_description_scroll_offset.saturating_sub(half_page);
        }

        Ok(())
    }

    pub(crate) fn handle_help_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let terminal_height = terminal.size()?.height;
        self.apply_help_scroll(key, terminal_height);
        Ok(())
    }

    /// Help viewport height: tab header (3) + content borders (2) + footer (1)
    pub(crate) const HELP_VIEWPORT_OVERHEAD: u16 = 6;

    pub(crate) fn apply_help_scroll(&mut self, key: event::KeyEvent, terminal_height: u16) {
        let kb = self.config.keybindings.clone();
        if self.matches_single_key(&key, &kb.tab_prev)
            || self.matches_single_key(&key, &kb.tab_next)
        {
            self.help_tab = match self.help_tab {
                HelpTab::Keybindings => HelpTab::Config,
                HelpTab::Config => HelpTab::Keybindings,
            };
            return;
        }

        if let Some(kb_event) = event_to_keybinding(&key) {
            self.check_sequence_timeout();
            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);
                if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                    self.clear_pending_keys();
                    match self.help_tab {
                        HelpTab::Keybindings => self.help_scroll_offset = 0,
                        HelpTab::Config => self.config_scroll_offset = 0,
                    };
                    return;
                }
                self.clear_pending_keys();
            } else if self.key_could_match_sequence(&key, &kb.jump_to_first) {
                self.push_pending_key(kb_event);
                return;
            }
        }

        let visible_lines = terminal_height.saturating_sub(Self::HELP_VIEWPORT_OVERHEAD) as usize;
        let half_page = (visible_lines / 2).max(1);

        let mut offset = match self.help_tab {
            HelpTab::Keybindings => self.help_scroll_offset,
            HelpTab::Config => self.config_scroll_offset,
        };

        if self.matches_single_key(&key, &kb.quit) || self.matches_single_key(&key, &kb.help) {
            self.state = self.previous_state;
            return;
        } else if Self::is_shift_char_shortcut(&key, 'j') {
            offset = offset.saturating_add(visible_lines.max(1));
        } else if Self::is_shift_char_shortcut(&key, 'k') {
            offset = offset.saturating_sub(visible_lines.max(1));
        } else if Self::is_shift_char_shortcut(&key, 'g') {
            offset = usize::MAX;
        } else if self.matches_single_key(&key, &kb.move_down) {
            offset = offset.saturating_add(1);
        } else if self.matches_single_key(&key, &kb.move_up) {
            offset = offset.saturating_sub(1);
        } else if self.matches_single_key(&key, &kb.page_down) {
            offset = offset.saturating_add(half_page);
        } else if self.matches_single_key(&key, &kb.page_up) {
            offset = offset.saturating_sub(half_page);
        }

        match self.help_tab {
            HelpTab::Keybindings => self.help_scroll_offset = offset,
            HelpTab::Config => self.config_scroll_offset = offset,
        };
    }
}
