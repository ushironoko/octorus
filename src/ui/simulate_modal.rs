use std::collections::BTreeMap;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, PendingGitOpsConfirm, SimulationResult};
use crate::gitfilm::GitfilmAreaSnapshot;

/// ファイルがどのエリアにどのステータスで存在するかの要約
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileState {
    working: Option<String>,
    staging: Option<String>,
}

impl FileState {
    fn label(&self) -> &str {
        if let Some(ref s) = self.staging {
            match s.as_str() {
                s if s.contains("new file") => "staged (new)",
                s if s.contains("modified") => "staged",
                s if s.contains("deleted") => "staged (deleted)",
                _ => "staged",
            }
        } else if let Some(ref w) = self.working {
            match w.as_str() {
                "clean" => "committed",
                "modified" => "modified",
                "untracked" => "untracked",
                s if s.contains("deleted") => "deleted",
                _ => w.as_str(),
            }
        } else {
            "unknown"
        }
    }
}

fn collect_file_states(snapshot: &GitfilmAreaSnapshot) -> BTreeMap<String, FileState> {
    let mut map = BTreeMap::new();
    for entry in &snapshot.working_tree {
        map.entry(entry.path.clone())
            .or_insert_with(|| FileState {
                working: None,
                staging: None,
            })
            .working = Some(entry.status.clone());
    }
    for entry in &snapshot.staging_area {
        map.entry(entry.path.clone())
            .or_insert_with(|| FileState {
                working: None,
                staging: None,
            })
            .staging = Some(entry.status.clone());
    }
    map
}

struct FileDiff {
    path: String,
    before_label: String,
    after_label: String,
}

fn compute_diffs(
    before: &GitfilmAreaSnapshot,
    after: &GitfilmAreaSnapshot,
) -> Vec<FileDiff> {
    let before_states = collect_file_states(before);
    let after_states = collect_file_states(after);

    let mut all_paths: Vec<String> = before_states
        .keys()
        .chain(after_states.keys())
        .cloned()
        .collect();
    all_paths.sort();
    all_paths.dedup();

    let empty = FileState {
        working: None,
        staging: None,
    };

    all_paths
        .into_iter()
        .filter_map(|path| {
            let b = before_states.get(&path).unwrap_or(&empty);
            let a = after_states.get(&path).unwrap_or(&empty);
            if b == a {
                return None;
            }
            Some(FileDiff {
                path,
                before_label: b.label().to_string(),
                after_label: a.label().to_string(),
            })
        })
        .collect()
}

fn transition_color(label: &str) -> Color {
    match label {
        "committed" => Color::Green,
        "staged" | "staged (new)" => Color::Cyan,
        "staged (deleted)" => Color::Red,
        "modified" => Color::Yellow,
        "untracked" => Color::DarkGray,
        "deleted" => Color::Red,
        _ => Color::White,
    }
}

pub fn render_simulating(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let width = 40_u16.min(area.width.saturating_sub(4));
    let height = 3_u16.min(area.height.saturating_sub(4));
    let modal_area = centered_rect(width, height, area);

    frame.render_widget(Clear, modal_area);

    let spinner = app.spinner_char();
    let text = Line::from(vec![Span::styled(
        format!(" {} Simulating... (Esc: cancel)", spinner),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, modal_area);
}

pub fn render_preview(frame: &mut Frame, app: &App) {
    let Some(ref ops) = app.git_ops_state else {
        return;
    };
    let Some(PendingGitOpsConfirm::Previewing {
        ref op,
        ref result,
        scroll_offset,
    }) = ops.pending_confirm
    else {
        return;
    };

    let area = frame.area();
    let modal_width = (area.width as f32 * 0.7) as u16;
    let modal_height = (area.height as f32 * 0.6) as u16;
    let modal_area = centered_rect(
        modal_width.max(40).min(area.width.saturating_sub(4)),
        modal_height.max(10).min(area.height.saturating_sub(4)),
        area,
    );

    frame.render_widget(Clear, modal_area);

    let mut lines: Vec<Line> = Vec::new();

    match result {
        SimulationResult::Success(ref preview) => {
            let diffs = compute_diffs(&preview.before, &preview.after);
            if diffs.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  (no changes)",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                for diff in diffs {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", diff.path),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )));
                    let before_color = transition_color(&diff.before_label);
                    let after_color = transition_color(&diff.after_label);
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(diff.before_label, Style::default().fg(before_color)),
                        Span::styled(" → ", Style::default().fg(Color::DarkGray)),
                        Span::styled(diff.after_label, Style::default().fg(after_color)),
                    ]));
                }
            }
        }
        SimulationResult::Message(ref msg) => {
            for line in msg.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", line),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }
    }

    let title = format!(" Confirm: {} ", op.display_command());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(" Y: confirm | n: cancel | j/k: scroll ")
        .border_style(Style::default().fg(Color::Yellow));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset as u16, 0));

    frame.render_widget(paragraph, modal_area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        DestructiveOp, GitOpsState, PendingGitOpsConfirm, SimulationPreview, SimulationResult,
    };
    use crate::config::Config;
    use crate::gitfilm::{GitfilmAreaSnapshot, GitfilmFileEntry, GitfilmRepoState};
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn make_app() -> (
        crate::app::App,
        tokio::sync::mpsc::Sender<crate::loader::DataLoadResult>,
    ) {
        let config = Config::default();
        crate::app::App::new_loading("owner/repo", 1, config)
    }

    fn empty_snapshot() -> GitfilmAreaSnapshot {
        GitfilmAreaSnapshot {
            working_tree: vec![],
            staging_area: vec![],
            repository: GitfilmRepoState { commits: vec![] },
        }
    }

    fn fe(path: &str, status: &str) -> GitfilmFileEntry {
        GitfilmFileEntry {
            path: path.to_string(),
            status: status.to_string(),
        }
    }

    fn render_modal_text(app: &mut crate::app::App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_preview(frame, app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut lines = Vec::new();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                let cell = &buf[(x, y)];
                line.push_str(cell.symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        // 空行をトリムして意味のある部分だけ返す
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        while lines.first().is_some_and(|l| l.is_empty()) {
            lines.remove(0);
        }
        lines.join("\n")
    }

    fn set_previewing(
        app: &mut crate::app::App,
        op: DestructiveOp,
        before: GitfilmAreaSnapshot,
        after: GitfilmAreaSnapshot,
    ) {
        let mut ops = GitOpsState::new(vec![]);
        ops.pending_confirm = Some(PendingGitOpsConfirm::Previewing {
            op,
            result: SimulationResult::Success(SimulationPreview { before, after }),
            scroll_offset: 0,
        });
        app.git_ops_state = Some(ops);
    }

    #[test]
    fn test_compute_diffs_committed_to_staged() {
        let before = GitfilmAreaSnapshot {
            working_tree: vec![
                fe("src/main.rs", "clean"),
                fe("src/lib.rs", "clean"),
            ],
            staging_area: vec![],
            repository: GitfilmRepoState { commits: vec![] },
        };
        let after = GitfilmAreaSnapshot {
            working_tree: vec![
                fe("src/main.rs", "clean"),
                fe("src/lib.rs", "clean"),
            ],
            staging_area: vec![
                fe("src/main.rs", "staged (modified)"),
                fe("src/lib.rs", "staged (modified)"),
            ],
            repository: GitfilmRepoState { commits: vec![] },
        };

        let diffs = compute_diffs(&before, &after);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].path, "src/lib.rs");
        assert_eq!(diffs[0].before_label, "committed");
        assert_eq!(diffs[0].after_label, "staged");
        assert_eq!(diffs[1].path, "src/main.rs");
    }

    #[test]
    fn test_compute_diffs_staged_to_modified() {
        let before = GitfilmAreaSnapshot {
            working_tree: vec![],
            staging_area: vec![fe("a.rs", "staged (modified)")],
            repository: GitfilmRepoState { commits: vec![] },
        };
        let after = GitfilmAreaSnapshot {
            working_tree: vec![fe("a.rs", "modified")],
            staging_area: vec![],
            repository: GitfilmRepoState { commits: vec![] },
        };

        let diffs = compute_diffs(&before, &after);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].before_label, "staged");
        assert_eq!(diffs[0].after_label, "modified");
    }

    #[test]
    fn test_compute_diffs_no_changes() {
        let snapshot = GitfilmAreaSnapshot {
            working_tree: vec![fe("a.rs", "clean")],
            staging_area: vec![],
            repository: GitfilmRepoState { commits: vec![] },
        };
        let diffs = compute_diffs(&snapshot, &snapshot);
        assert!(diffs.is_empty());
    }

    #[test]
    fn test_compute_diffs_new_file_staged() {
        let before = empty_snapshot();
        let after = GitfilmAreaSnapshot {
            working_tree: vec![],
            staging_area: vec![fe("new.rs", "staged (new file)")],
            repository: GitfilmRepoState { commits: vec![] },
        };

        let diffs = compute_diffs(&before, &after);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].before_label, "unknown");
        assert_eq!(diffs[0].after_label, "staged (new)");
    }

    #[test]
    fn test_compute_diffs_file_deleted() {
        let before = GitfilmAreaSnapshot {
            working_tree: vec![fe("old.rs", "clean")],
            staging_area: vec![],
            repository: GitfilmRepoState { commits: vec![] },
        };
        let after = GitfilmAreaSnapshot {
            working_tree: vec![],
            staging_area: vec![fe("old.rs", "staged (deleted)")],
            repository: GitfilmRepoState { commits: vec![] },
        };

        let diffs = compute_diffs(&before, &after);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].before_label, "committed");
        assert_eq!(diffs[0].after_label, "staged (deleted)");
    }

    #[test]
    fn test_modal_commit_undo_snapshot() {
        let (mut app, _tx) = make_app();
        set_previewing(
            &mut app,
            DestructiveOp::UndoCommit,
            GitfilmAreaSnapshot {
                working_tree: vec![
                    fe("src/app/types.rs", "clean"),
                    fe("src/ui/git_ops.rs", "clean"),
                ],
                staging_area: vec![],
                repository: GitfilmRepoState { commits: vec![] },
            },
            GitfilmAreaSnapshot {
                working_tree: vec![
                    fe("src/app/types.rs", "clean"),
                    fe("src/ui/git_ops.rs", "clean"),
                ],
                staging_area: vec![
                    fe("src/app/types.rs", "staged (modified)"),
                    fe("src/ui/git_ops.rs", "staged (modified)"),
                ],
                repository: GitfilmRepoState { commits: vec![] },
            },
        );

        assert_snapshot!(render_modal_text(&mut app, 60, 15), @"
        ┌ Confirm: git reset --soft HEAD~1 ──────┐
        │  src/app/types.rs                      │
        │    committed → staged                  │
        │  src/ui/git_ops.rs                     │
        │    committed → staged                  │
        │                                        │
        │                                        │
        │                                        │
        │                                        │
        └ Y: confirm | n: cancel | j/k: scroll ──┘
        ");
    }

    #[test]
    fn test_modal_discard_snapshot() {
        let (mut app, _tx) = make_app();
        set_previewing(
            &mut app,
            DestructiveOp::Discard {
                path: "src/main.rs".to_string(),
            },
            GitfilmAreaSnapshot {
                working_tree: vec![fe("src/main.rs", "modified")],
                staging_area: vec![],
                repository: GitfilmRepoState { commits: vec![] },
            },
            GitfilmAreaSnapshot {
                working_tree: vec![fe("src/main.rs", "clean")],
                staging_area: vec![],
                repository: GitfilmRepoState { commits: vec![] },
            },
        );

        assert_snapshot!(render_modal_text(&mut app, 60, 12), @"
        ┌ Confirm: git restore -- src/main.rs ───┐
        │  src/main.rs                           │
        │    modified → committed                │
        │                                        │
        │                                        │
        │                                        │
        │                                        │
        └ Y: confirm | n: cancel | j/k: scroll ──┘
        ");
    }

    #[test]
    fn test_modal_no_changes_snapshot() {
        let (mut app, _tx) = make_app();
        let snapshot = GitfilmAreaSnapshot {
            working_tree: vec![fe("a.rs", "clean")],
            staging_area: vec![],
            repository: GitfilmRepoState { commits: vec![] },
        };
        set_previewing(
            &mut app,
            DestructiveOp::Discard {
                path: "a.rs".to_string(),
            },
            snapshot.clone(),
            snapshot,
        );

        assert_snapshot!(render_modal_text(&mut app, 60, 10), @"
        ┌ Confirm: git restore -- a.rs ──────────┐
        │  (no changes)                          │
        │                                        │
        │                                        │
        │                                        │
        └ Y: confirm | n: cancel | j/k: scroll ──┘
        ");
    }

    #[test]
    fn test_modal_undo_stage_snapshot() {
        let (mut app, _tx) = make_app();
        set_previewing(
            &mut app,
            DestructiveOp::UndoStage {
                paths: vec!["src/lib.rs".to_string()],
            },
            GitfilmAreaSnapshot {
                working_tree: vec![],
                staging_area: vec![fe("src/lib.rs", "staged (modified)")],
                repository: GitfilmRepoState { commits: vec![] },
            },
            GitfilmAreaSnapshot {
                working_tree: vec![fe("src/lib.rs", "modified")],
                staging_area: vec![],
                repository: GitfilmRepoState { commits: vec![] },
            },
        );

        assert_snapshot!(render_modal_text(&mut app, 60, 10), @"
        ┌ Confirm: git reset -- src/lib.rs ──────┐
        │  src/lib.rs                            │
        │    staged → modified                   │
        │                                        │
        │                                        │
        └ Y: confirm | n: cancel | j/k: scroll ──┘
        ");
    }

    #[test]
    fn test_modal_force_push_snapshot() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(vec![]);
        ops.pending_confirm = Some(PendingGitOpsConfirm::Previewing {
            op: DestructiveOp::ForcePush {
                branch: "feat/my-branch".to_string(),
            },
            result: SimulationResult::Message(
                "Push was rejected (non-fast-forward).\nForce push will overwrite remote history."
                    .to_string(),
            ),
            scroll_offset: 0,
        });
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_modal_text(&mut app, 70, 10), @"
        ┌ Confirm: git push --force-with-lease origin fe┐
        │  Push was rejected (non-fast-forward).        │
        │  Force push will overwrite remote history.    │
        │                                               │
        │                                               │
        └ Y: confirm | n: cancel | j/k: scroll ─────────┘
        ");
    }
}
