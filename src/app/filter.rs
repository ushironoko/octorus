use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, DataState};

/// issue_state のフィルタに安全にアクセスするヘルパー
fn get_issue_filter_mut(app: &mut App) -> Option<&mut crate::filter::ListFilter> {
    app.issue_state
        .as_mut()
        .and_then(|s| s.issue_list_filter.as_mut())
}

fn get_issue_filter(app: &App) -> Option<&crate::filter::ListFilter> {
    app.issue_state
        .as_ref()
        .and_then(|s| s.issue_list_filter.as_ref())
}

impl App {
    pub(crate) fn handle_filter_input(&mut self, key: &KeyEvent, target: &str) -> bool {
        let filter = match target {
            "pr" => self.pr_list_filter.as_mut(),
            "file" => self.file_list_filter.as_mut(),
            "issue" => get_issue_filter_mut(self),
            _ => return false,
        };
        let Some(filter) = filter else {
            return false;
        };
        if !filter.input_active {
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                match target {
                    "pr" => self.pr_list_filter = None,
                    "file" => self.file_list_filter = None,
                    "issue" => {
                        if let Some(ref mut s) = self.issue_state {
                            s.issue_list_filter = None;
                        }
                    }
                    _ => {}
                }
                true
            }
            KeyCode::Enter => {
                let filter = match target {
                    "pr" => self.pr_list_filter.as_mut(),
                    "file" => self.file_list_filter.as_mut(),
                    "issue" => get_issue_filter_mut(self),
                    _ => return false,
                };
                if let Some(f) = filter {
                    if f.query.is_empty() {
                        match target {
                            "pr" => self.pr_list_filter = None,
                            "file" => self.file_list_filter = None,
                            "issue" => {
                                if let Some(ref mut s) = self.issue_state {
                                    s.issue_list_filter = None;
                                }
                            }
                            _ => {}
                        }
                    } else {
                        f.input_active = false;
                    }
                }
                true
            }
            KeyCode::Backspace => {
                let filter = match target {
                    "pr" => self.pr_list_filter.as_mut(),
                    "file" => self.file_list_filter.as_mut(),
                    "issue" => get_issue_filter_mut(self),
                    _ => return false,
                };
                if let Some(f) = filter {
                    f.delete_char();
                    self.reapply_filter(target);
                }
                true
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let filter = match target {
                    "pr" => self.pr_list_filter.as_mut(),
                    "file" => self.file_list_filter.as_mut(),
                    "issue" => get_issue_filter_mut(self),
                    _ => return false,
                };
                if let Some(f) = filter {
                    f.clear_query();
                    self.reapply_filter(target);
                }
                true
            }
            KeyCode::Up => {
                let filter = match target {
                    "pr" => self.pr_list_filter.as_mut(),
                    "file" => self.file_list_filter.as_mut(),
                    "issue" => get_issue_filter_mut(self),
                    _ => return false,
                };
                if let Some(f) = filter {
                    if let Some(idx) = f.navigate_up() {
                        match target {
                            "pr" => self.selected_pr = idx,
                            "file" => self.selected_file = idx,
                            "issue" => {
                                if let Some(ref mut s) = self.issue_state {
                                    s.selected_issue = idx;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                true
            }
            KeyCode::Down => {
                let filter = match target {
                    "pr" => self.pr_list_filter.as_mut(),
                    "file" => self.file_list_filter.as_mut(),
                    "issue" => get_issue_filter_mut(self),
                    _ => return false,
                };
                if let Some(f) = filter {
                    if let Some(idx) = f.navigate_down() {
                        match target {
                            "pr" => self.selected_pr = idx,
                            "file" => self.selected_file = idx,
                            "issue" => {
                                if let Some(ref mut s) = self.issue_state {
                                    s.selected_issue = idx;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                true
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return false;
                }
                let filter = match target {
                    "pr" => self.pr_list_filter.as_mut(),
                    "file" => self.file_list_filter.as_mut(),
                    "issue" => get_issue_filter_mut(self),
                    _ => return false,
                };
                if let Some(f) = filter {
                    f.insert_char(c);
                    self.reapply_filter(target);
                }
                true
            }
            _ => true,
        }
    }

    /// フィルタを再適用し、選択位置を同期する
    pub(crate) fn reapply_filter(&mut self, target: &str) {
        match target {
            "pr" => {
                let mut filter = match self.pr_list_filter.take() {
                    Some(f) => f,
                    None => return,
                };
                if let Some(prs) = self.pr_list.as_ref() {
                    filter.apply(prs, |pr, q| {
                        pr.title.to_lowercase().contains(q)
                            || pr.number.to_string().contains(q)
                            || pr.author.login.to_lowercase().contains(q)
                    });
                    if let Some(idx) = filter.sync_selection() {
                        self.selected_pr = idx;
                    }
                }
                self.pr_list_filter = Some(filter);
            }
            "file" => {
                let mut filter = match self.file_list_filter.take() {
                    Some(f) => f,
                    None => return,
                };
                let files = match &self.data_state {
                    DataState::Loaded { files, .. } => files.as_slice(),
                    _ => &[],
                };
                filter.apply(files, |file, q| file.filename.to_lowercase().contains(q));
                if let Some(idx) = filter.sync_selection() {
                    self.selected_file = idx;
                }
                self.file_list_filter = Some(filter);
            }
            "issue" => {
                let Some(ref mut state) = self.issue_state else {
                    return;
                };
                let mut filter = match state.issue_list_filter.take() {
                    Some(f) => f,
                    None => return,
                };
                if let Some(issues) = state.issues.as_ref() {
                    filter.apply(issues, |issue, q| {
                        issue.title.to_lowercase().contains(q)
                            || issue.number.to_string().contains(q)
                            || issue.author.login.to_lowercase().contains(q)
                    });
                    if let Some(idx) = filter.sync_selection() {
                        state.selected_issue = idx;
                    }
                }
                state.issue_list_filter = Some(filter);
            }
            _ => {}
        }
    }

    /// フィルタ適用中のナビゲーション（j/k/↑/↓）。処理した場合は true を返す。
    pub(crate) fn handle_filter_navigation(&mut self, target: &str, is_down: bool) -> bool {
        let filter = match target {
            "pr" => self.pr_list_filter.as_mut(),
            "file" => self.file_list_filter.as_mut(),
            "issue" => get_issue_filter_mut(self),
            _ => return false,
        };
        let Some(filter) = filter else {
            return false;
        };
        if filter.input_active {
            return false;
        }

        let idx = if is_down {
            filter.navigate_down()
        } else {
            filter.navigate_up()
        };
        if let Some(idx) = idx {
            match target {
                "pr" => self.selected_pr = idx,
                "file" => self.selected_file = idx,
                "issue" => {
                    if let Some(ref mut s) = self.issue_state {
                        s.selected_issue = idx;
                    }
                }
                _ => {}
            }
        }
        true
    }

    /// フィルタ適用中（非入力）の Esc 処理。処理した場合は true を返す。
    pub(crate) fn handle_filter_esc(&mut self, target: &str) -> bool {
        let has_filter = match target {
            "pr" => self.pr_list_filter.is_some(),
            "file" => self.file_list_filter.is_some(),
            "issue" => get_issue_filter(self).is_some(),
            _ => return false,
        };
        if has_filter {
            match target {
                "pr" => self.pr_list_filter = None,
                "file" => self.file_list_filter = None,
                "issue" => {
                    if let Some(ref mut s) = self.issue_state {
                        s.issue_list_filter = None;
                    }
                }
                _ => {}
            }
            true
        } else {
            false
        }
    }

    /// フィルタ適用中の Enter 処理。選択が None の場合は Enter を無視する。
    pub(crate) fn is_filter_selection_empty(&self, target: &str) -> bool {
        let filter = match target {
            "pr" => self.pr_list_filter.as_ref(),
            "file" => self.file_list_filter.as_ref(),
            "issue" => get_issue_filter(self),
            _ => return false,
        };
        match filter {
            Some(f) => f.selected.is_none(),
            None => false,
        }
    }
}
