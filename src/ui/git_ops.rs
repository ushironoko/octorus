use std::collections::HashSet;

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};

use super::common::render_rally_status_bar;
use super::diff_view;
use crate::app::{App, AppState, CommitLogState, FileStatus, GitOpsState, LeftPaneFocus, TreeRow};
use crate::github::{format_relative_time, PrCommit};

pub fn render(frame: &mut Frame, app: &mut App) {
    let has_rally = app.has_background_rally();

    let outer_constraints = if has_rally {
        vec![Constraint::Min(0), Constraint::Length(1)]
    } else {
        vec![Constraint::Min(0)]
    };

    let outer_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(outer_constraints)
        .split(frame.area());

    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(app.config.layout.left_panel_percent()),
            Constraint::Percentage(app.config.layout.right_panel_percent()),
        ])
        .split(outer_chunks[0]);

    let is_diff_focused = app.state == AppState::GitOpsSplitDiff;
    let left_focus = app
        .git_ops_state
        .as_ref()
        .map(|ops| ops.left_focus)
        .unwrap_or(LeftPaneFocus::Tree);
    let is_tree_focused = !is_diff_focused && left_focus == LeftPaneFocus::Tree;
    let is_commits_focused = !is_diff_focused && left_focus == LeftPaneFocus::Commits;

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(h_chunks[0]);

    let diff_visible_lines = h_chunks[1].height.saturating_sub(8) as usize;
    if let Some(ref mut ops) = app.git_ops_state {
        ops.diff_scroll.set_visible_lines(diff_visible_lines);
        ops.commit_log.diff_scroll.set_visible_lines(diff_visible_lines);
    }

    render_tree_pane(frame, app, left_chunks[0], is_tree_focused);
    render_commits_pane(frame, app, left_chunks[1], is_commits_focused);
    render_diff_pane(frame, &*app, h_chunks[1], is_diff_focused);

    if has_rally {
        render_rally_status_bar(frame, outer_chunks[1], app);
    }
}

fn render_tree_pane(
    frame: &mut Frame,
    app: &mut App,
    area: ratatui::layout::Rect,
    is_focused: bool,
) {
    let has_pending_confirm = app
        .git_ops_state
        .as_ref()
        .is_some_and(|ops| ops.pending_confirm.is_some());

    let border_color = if has_pending_confirm {
        Color::Red
    } else if is_focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let Some(ref ops) = app.git_ops_state else {
        return;
    };

    let (staged_count, unstaged_count, untracked_count) = count_statuses(ops);
    let header_text = format!(
        "staged:{} unstaged:{} untracked:{}",
        staged_count, unstaged_count, untracked_count,
    );
    let header = Paragraph::new(header_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title("Git Status"),
    );
    frame.render_widget(header, chunks[0]);

    if ops.entries.is_empty() {
        let empty = Paragraph::new("No changes")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title("Files (0)"),
            );
        frame.render_widget(empty, chunks[1]);
    } else {
        let total = ops.tree.visible_rows.len();
        let selected = ops.tree.selected_row;
        let items: Vec<ListItem> = ops
            .tree
            .visible_rows
            .iter()
            .enumerate()
            .map(|(i, row)| build_tree_row_item(ops, row, i == selected))
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(format!("Files ({})", ops.entries.len())),
            )
            .highlight_style(Style::default().bg(Color::DarkGray));

        let mut list_state = ListState::default()
            .with_offset(ops.tree.scroll_offset)
            .with_selected(Some(selected));

        frame.render_stateful_widget(list, chunks[1], &mut list_state);

        if let Some(ref mut ops) = app.git_ops_state {
            ops.tree.scroll_offset = list_state.offset();
        }

        if total > 1 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            let mut scrollbar_state =
                ScrollbarState::new(total.saturating_sub(1)).position(selected);

            frame.render_stateful_widget(
                scrollbar,
                chunks[1].inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }
    }

    let confirm_text;
    let pending = app
        .git_ops_state
        .as_ref()
        .and_then(|ops| ops.pending_confirm.as_ref());
    let help_text = if let Some(confirm) = pending {
        match confirm {
            crate::app::PendingGitOpsConfirm::Discard { ref path, ref command } => {
                let name = path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path);
                confirm_text = format!("Discard {}? [{}] (Y/n)", name, command);
                confirm_text.as_str()
            }
            crate::app::PendingGitOpsConfirm::Undo { ref command } => {
                confirm_text = format!("Undo? [{}] (Y/n)", command);
                confirm_text.as_str()
            }
        }
    } else {
        let Some(ref ops) = app.git_ops_state else {
            return;
        };
        if ops.pushing {
            let spinner = app.spinner_char();
            confirm_text = format!("{} Pushing...", spinner);
            confirm_text.as_str()
        } else if let Some((ref msg, _)) = ops.op_message {
            msg.as_str()
        } else if is_focused {
            "j/k: move | Space: stage | s: all | d: discard | c: commit | u: undo | R: refresh | P: push | Tab: commits"
        } else {
            "Tab/h: focus tree"
        }
    };
    if has_pending_confirm {
        let line = Line::from(Span::styled(
            help_text,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        let footer = Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        );
        frame.render_widget(footer, chunks[2]);
    } else {
        let footer_line = super::footer::build_footer_line(app, help_text);
        let footer =
            Paragraph::new(footer_line).block(super::footer::build_footer_block_with_border(
                app,
                Style::default().fg(border_color),
            ));
        frame.render_widget(footer, chunks[2]);
    }
}

fn render_diff_pane(frame: &mut Frame, app: &App, area: ratatui::layout::Rect, is_focused: bool) {
    let border_color = if is_focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let bg_color = app.config.diff.bg_color;

    let Some(ref ops) = app.git_ops_state else {
        return;
    };

    let is_commit_diff = ops.left_return_focus == LeftPaneFocus::Commits;

    let header_text = if is_commit_diff {
        ops.commit_log
            .commits
            .get(ops.commit_log.selected)
            .map(|c| format!("{} {}", c.short_sha(), c.message))
            .unwrap_or_else(|| "No commit selected".to_string())
    } else {
        ops.selected_path()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "No file selected".to_string())
    };

    let header = Paragraph::new(header_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title("Diff Preview"),
    );
    frame.render_widget(header, chunks[0]);

    if is_commit_diff {
        render_commit_diff_body(frame, &ops.commit_log, chunks[1], border_color, bg_color);
    } else {
        render_diff_body(frame, ops, chunks[1], border_color, bg_color);
    }

    let footer_text = if is_focused {
        "j/k: scroll | J/K: page | gg/G: top/bottom | Ctrl-d/u: page | Tab: tree | h/Esc: back"
    } else {
        "Enter/l: focus diff"
    };
    let footer_line = super::footer::build_footer_line(app, footer_text);
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block_with_border(
        app,
        Style::default().fg(border_color),
    ));
    frame.render_widget(footer, chunks[2]);
}

fn render_diff_body(
    frame: &mut Frame,
    ops: &GitOpsState,
    area: ratatui::layout::Rect,
    border_color: Color,
    bg_color: bool,
) {
    let lines: Vec<Line> = if let Some(ref cache) = ops.diff_store.current {
        let visible_height = area.height.saturating_sub(2) as usize;
        let line_count = cache.lines.len();
        let visible_start = ops.diff_scroll.scroll_offset.saturating_sub(2).min(line_count);
        let visible_end = (ops.diff_scroll.scroll_offset + visible_height + 5).min(line_count);

        let empty_comments = HashSet::new();
        diff_view::render_cached_lines(
            cache,
            visible_start..visible_end,
            ops.diff_scroll.selected_line,
            &empty_comments,
            bg_color,
            None,
        )
    } else {
        vec![Line::from(Span::styled(
            "Select a file to preview diff",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let adjusted_scroll = if ops.diff_store.current.is_some() {
        let visible_start = ops.diff_scroll.scroll_offset.saturating_sub(2);
        (ops.diff_scroll.scroll_offset - visible_start) as u16
    } else {
        ops.diff_scroll.scroll_offset as u16
    };

    let diff_block = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .wrap(Wrap { trim: false })
        .scroll((adjusted_scroll, 0));
    frame.render_widget(diff_block, area);

    if let Some(ref cache) = ops.diff_store.current {
        let total_lines = cache.lines.len();
        let visible_height = area.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible_height);
        if max_scroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            let clamped_position = ops.diff_scroll.scroll_offset.min(max_scroll);
            let mut scrollbar_state = ScrollbarState::new(max_scroll).position(clamped_position);

            frame.render_stateful_widget(
                scrollbar,
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }
    }
}

fn render_commit_diff_body(
    frame: &mut Frame,
    cl: &CommitLogState,
    area: ratatui::layout::Rect,
    border_color: Color,
    bg_color: bool,
) {
    let lines: Vec<Line> = if cl.diff_loading {
        vec![Line::from(Span::styled(
            "Loading diff...",
            Style::default().fg(Color::Yellow),
        ))]
    } else if let Some(ref error) = cl.diff_error {
        vec![Line::from(Span::styled(
            format!("Error: {}", error),
            Style::default().fg(Color::Red),
        ))]
    } else if let Some(ref cache) = cl.diff_store.current {
        let visible_height = area.height.saturating_sub(2) as usize;
        let line_count = cache.lines.len();
        let visible_start = cl.diff_scroll.scroll_offset.saturating_sub(2).min(line_count);
        let visible_end = (cl.diff_scroll.scroll_offset + visible_height + 5).min(line_count);

        let empty_comments = HashSet::new();
        diff_view::render_cached_lines(
            cache,
            visible_start..visible_end,
            cl.diff_scroll.selected_line,
            &empty_comments,
            bg_color,
            None,
        )
    } else {
        vec![Line::from(Span::styled(
            "Select a commit to preview diff",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let adjusted_scroll = if cl.diff_store.current.is_some() {
        let visible_start = cl.diff_scroll.scroll_offset.saturating_sub(2);
        (cl.diff_scroll.scroll_offset - visible_start) as u16
    } else {
        cl.diff_scroll.scroll_offset as u16
    };

    let diff_block = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .wrap(Wrap { trim: false })
        .scroll((adjusted_scroll, 0));
    frame.render_widget(diff_block, area);

    if let Some(ref cache) = cl.diff_store.current {
        let total_lines = cache.lines.len();
        let visible_height = area.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible_height);
        if max_scroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            let clamped_position = cl.diff_scroll.scroll_offset.min(max_scroll);
            let mut scrollbar_state = ScrollbarState::new(max_scroll).position(clamped_position);

            frame.render_stateful_widget(
                scrollbar,
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }
    }
}

fn render_commits_pane(
    frame: &mut Frame,
    app: &mut App,
    area: ratatui::layout::Rect,
    is_focused: bool,
) {
    let border_color = if is_focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let spinner = app.spinner_char().to_string();
    let Some(ref mut ops) = app.git_ops_state else {
        return;
    };
    let ahead_count = ops.ahead_count;
    let cl = &mut ops.commit_log;

    if cl.loading && cl.commits.is_empty() {
        let loading = Paragraph::new(Line::from(Span::styled(
            format!("{} Loading commits...", spinner),
            Style::default().fg(Color::Yellow),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title("Commits"),
        );
        frame.render_widget(loading, area);
        return;
    }

    if let Some(ref error) = cl.error {
        let err_msg = Paragraph::new(Span::styled(
            format!("Error: {}", error),
            Style::default().fg(Color::Red),
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title("Commits"),
        );
        frame.render_widget(err_msg, area);
        return;
    }

    if cl.commits.is_empty() {
        let empty = Paragraph::new("No commits")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title("Commits (0)"),
            );
        frame.render_widget(empty, area);
        return;
    }

    let total = cl.commits.len();
    let selected = cl.selected;
    let mut items: Vec<ListItem> = cl
        .commits
        .iter()
        .enumerate()
        .map(|(i, commit)| build_commit_item(commit, i == selected))
        .collect();

    if cl.loading && !cl.commits.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("{} Loading more...", spinner),
            Style::default().fg(Color::Yellow),
        ))));
    }

    let count_str = if cl.has_more {
        format!("{}+", total)
    } else {
        format!("{}", total)
    };
    let title = if ahead_count > 0 {
        format!("Commits ({}) \u{2191}{}", count_str, ahead_count)
    } else {
        format!("Commits ({})", count_str)
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(title),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let mut list_state = ListState::default()
        .with_offset(cl.scroll_offset)
        .with_selected(Some(selected));

    frame.render_stateful_widget(list, area, &mut list_state);
    cl.scroll_offset = list_state.offset();

    if total > 1 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state = ScrollbarState::new(total.saturating_sub(1)).position(selected);

        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn build_commit_item<'a>(commit: &PrCommit, is_selected: bool) -> ListItem<'a> {
    let style = if is_selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let relative_time = format_relative_time(&commit.date);
    let author = commit
        .author_login
        .as_deref()
        .unwrap_or(&commit.author_name);

    let line = Line::from(vec![
        Span::styled(
            format!("{} ", commit.short_sha()),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(commit.message.clone(), style),
        Span::styled(
            format!("  ({}, {})", author, relative_time),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    ListItem::new(line)
}

fn build_tree_row_item<'a>(
    ops: &GitOpsState,
    row: &TreeRow,
    is_selected: bool,
) -> ListItem<'a> {
    match row {
        TreeRow::Dir { ref path, depth, expanded } => {
            let indent = "  ".repeat(*depth);
            let icon = if *expanded { "▼" } else { "▶" };
            let dir_name = path
                .rsplit_once('/')
                .map(|(_, name)| name)
                .unwrap_or(path);

            let style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };

            let line = Line::from(vec![
                Span::raw(indent),
                Span::styled(format!("{} {}/", icon, dir_name), style),
            ]);
            ListItem::new(line)
        }
        TreeRow::File { index, depth } => {
            let Some(entry) = ops.entries.get(*index) else {
                return ListItem::new(Line::from(""));
            };

            let indent = "  ".repeat(*depth);

            let label = entry.change_type_label();
            let status_color = status_color_for_entry(entry);
            let filename = entry
                .path
                .rsplit_once('/')
                .map(|(_, name)| name)
                .unwrap_or(&entry.path);

            let file_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let mut spans = vec![
                Span::raw(indent),
                Span::styled(label, Style::default().fg(status_color)),
                Span::raw(" "),
                Span::styled(filename.to_string(), file_style),
            ];

            let total_add = entry.additions + entry.staged_additions;
            let total_del = entry.deletions + entry.staged_deletions;
            if total_add > 0 || total_del > 0 {
                spans.push(Span::raw(format!(" +{} -{}", total_add, total_del)));
            }

            ListItem::new(Line::from(spans))
        }
    }
}

fn status_color_for_entry(entry: &crate::app::GitStatusEntry) -> Color {
    if entry.unmerged {
        return Color::Red;
    }

    match (entry.index_status, entry.worktree_status) {
        (_, FileStatus::Unmodified) if entry.is_staged() => Color::Green,
        (FileStatus::Untracked, FileStatus::Untracked) => Color::Magenta,
        (_, wt) if wt != FileStatus::Unmodified && wt != FileStatus::Ignored => Color::Red,
        _ if entry.is_staged() && entry.has_worktree_changes() => Color::Yellow,
        _ => Color::White,
    }
}

fn count_statuses(ops: &GitOpsState) -> (usize, usize, usize) {
    let mut staged = 0;
    let mut unstaged = 0;
    let mut untracked = 0;

    for entry in &ops.entries {
        if entry.index_status == FileStatus::Untracked
            && entry.worktree_status == FileStatus::Untracked
        {
            untracked += 1;
        } else {
            if entry.is_staged() {
                staged += 1;
            }
            if entry.has_worktree_changes() {
                unstaged += 1;
            }
        }
    }

    (staged, unstaged, untracked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{FileStatus, GitOpsState, GitStatusEntry, PendingGitOpsConfirm};
    use crate::config::Config;
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    fn make_app() -> (App, tokio::sync::mpsc::Sender<crate::loader::DataLoadResult>) {
        let config = Config::default();
        App::new_loading("owner/repo", 1, config)
    }

    fn entry(path: &str, index: FileStatus, worktree: FileStatus) -> GitStatusEntry {
        GitStatusEntry {
            path: path.to_string(),
            index_status: index,
            worktree_status: worktree,
            additions: 0,
            deletions: 0,
            staged_additions: 0,
            staged_deletions: 0,
            orig_path: None,
            unmerged: false,
        }
    }

    fn rebuild_tree(ops: &mut GitOpsState) {
        let paths: Vec<(usize, &str)> = ops
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (i, e.path.as_str()))
            .collect();
        ops.tree.rebuild(&paths);
    }

    /// render_tree_pane を TestBackend に描画し、フッター行（下3行）のテキストを返す
    fn render_tree_pane_footer(app: &mut App, is_focused: bool) -> String {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, 100, 20);
        terminal
            .draw(|frame| {
                render_tree_pane(frame, app, area, is_focused);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let footer_start = area.height.saturating_sub(3) as usize;
        let mut lines = Vec::new();
        for y in footer_start..area.height as usize {
            let mut line = String::new();
            for x in 0..area.width as usize {
                let cell = &buf[(x as u16, y as u16)];
                line.push_str(cell.symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    fn render_tree_pane_border_color(app: &mut App, is_focused: bool) -> Color {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, 80, 20);
        terminal
            .draw(|frame| {
                render_tree_pane(frame, app, area, is_focused);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        buf[(0u16, 0u16)].fg
    }

    #[test]
    fn test_footer_discard_confirm_focused() {
        let (mut app, _tx) = make_app();
        let entries = vec![entry("src/main.rs", FileStatus::Unmodified, FileStatus::Modified)];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);
        ops.pending_confirm = Some(PendingGitOpsConfirm::Discard {
            path: "src/main.rs".to_string(),
            command: "git restore -- src/main.rs".to_string(),
        });
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_tree_pane_footer(&mut app, true), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Discard main.rs? [git restore -- src/main.rs] (Y/n)                                               │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_footer_discard_confirm_unfocused() {
        let (mut app, _tx) = make_app();
        let entries = vec![entry("a.rs", FileStatus::Unmodified, FileStatus::Modified)];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);
        ops.pending_confirm = Some(PendingGitOpsConfirm::Discard {
            path: "a.rs".to_string(),
            command: "git restore -- a.rs".to_string(),
        });
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_tree_pane_footer(&mut app, false), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Discard a.rs? [git restore -- a.rs] (Y/n)                                                         │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_footer_undo_confirm_focused() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ]);
        rebuild_tree(&mut ops);
        ops.pending_confirm = Some(PendingGitOpsConfirm::Undo {
            command: "git update-index (restore 1 file(s))".to_string(),
        });
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_tree_pane_footer(&mut app, true), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Undo? [git update-index (restore 1 file(s))] (Y/n)                                                │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_footer_undo_confirm_unfocused() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ]);
        rebuild_tree(&mut ops);
        ops.pending_confirm = Some(PendingGitOpsConfirm::Undo {
            command: "git reset --soft abc1234".to_string(),
        });
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_tree_pane_footer(&mut app, false), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Undo? [git reset --soft abc1234] (Y/n)                                                            │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_footer_no_confirm_focused() {
        let (mut app, _tx) = make_app();
        let entries = vec![entry("a.rs", FileStatus::Unmodified, FileStatus::Modified)];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_tree_pane_footer(&mut app, true), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │j/k: move | Space: stage | s: all | d: discard | c: commit | u: undo | R: refresh | P: push | Tab:│
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_footer_no_confirm_unfocused() {
        let (mut app, _tx) = make_app();
        let entries = vec![entry("a.rs", FileStatus::Unmodified, FileStatus::Modified)];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_tree_pane_footer(&mut app, false), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Tab/h: focus tree                                                                                 │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_border_red_on_pending_confirm() {
        let (mut app, _tx) = make_app();
        let entries = vec![entry("a.rs", FileStatus::Unmodified, FileStatus::Modified)];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);
        ops.pending_confirm = Some(PendingGitOpsConfirm::Undo {
            command: "git reset".to_string(),
        });
        app.git_ops_state = Some(ops);

        assert_eq!(render_tree_pane_border_color(&mut app, false), Color::Red);
    }

    #[test]
    fn test_border_not_red_without_confirm() {
        let (mut app, _tx) = make_app();
        let entries = vec![entry("a.rs", FileStatus::Unmodified, FileStatus::Modified)];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);
        app.git_ops_state = Some(ops);

        assert_eq!(
            render_tree_pane_border_color(&mut app, true),
            Color::Yellow
        );
        assert_eq!(
            render_tree_pane_border_color(&mut app, false),
            Color::DarkGray
        );
    }

    #[test]
    fn test_footer_op_message_focused() {
        let (mut app, _tx) = make_app();
        let entries = vec![entry("a.rs", FileStatus::Unmodified, FileStatus::Modified)];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);
        ops.op_message = Some(("Pushed to origin/main".to_string(), std::time::Instant::now()));
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_tree_pane_footer(&mut app, true), @r"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Pushed to origin/main                                                                             │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_footer_op_message_unfocused() {
        let (mut app, _tx) = make_app();
        let entries = vec![entry("a.rs", FileStatus::Unmodified, FileStatus::Modified)];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);
        ops.op_message = Some(("Pushed to origin/main".to_string(), std::time::Instant::now()));
        app.git_ops_state = Some(ops);

        // unfocused 時に op_message が見えるかどうか
        assert_snapshot!(render_tree_pane_footer(&mut app, false), @r"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Pushed to origin/main                                                                             │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_footer_op_message_empty_entries_focused() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.op_message = Some(("Pushed to origin/main".to_string(), std::time::Instant::now()));
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_tree_pane_footer(&mut app, true), @r"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Pushed to origin/main                                                                             │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    // =================================================================
    // コミット diff ペイン スナップショット
    // =================================================================

    fn make_commit(sha: &str, message: &str) -> crate::github::PrCommit {
        crate::github::PrCommit {
            sha: sha.to_string(),
            message: message.to_string(),
            author_name: "tester".to_string(),
            author_login: None,
            date: "2025-01-01T00:00:00Z".to_string(),
        }
    }

    /// render_diff_pane を描画し、diff body 部分のテキストを返す
    fn render_diff_pane_body(app: &App) -> String {
        let backend = TestBackend::new(80, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, 80, 15);
        terminal
            .draw(|frame| {
                render_diff_pane(frame, app, area, false);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // Header: row 0-2, Body: row 3-11, Footer: row 12-14
        let body_start = 3usize;
        let body_end = 12usize;
        let mut lines = Vec::new();
        for y in body_start..body_end {
            let mut line = String::new();
            for x in 0..area.width as usize {
                let cell = &buf[(x as u16, y as u16)];
                line.push_str(cell.symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    #[test]
    fn test_diff_pane_commit_loading() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.commit_log.commits.push(make_commit("abc1234", "test commit"));
        ops.commit_log.selected = 0;
        ops.commit_log.diff_loading = true;
        ops.left_return_focus = LeftPaneFocus::Commits;
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_diff_pane_body(&app), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Loading diff...                                                               │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        └──────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_diff_pane_commit_error() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.commit_log.commits.push(make_commit("abc1234", "test commit"));
        ops.commit_log.selected = 0;
        ops.commit_log.diff_error = Some("gh: Not Found (HTTP 404)".to_string());
        ops.left_return_focus = LeftPaneFocus::Commits;
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_diff_pane_body(&app), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Error: gh: Not Found (HTTP 404)                                               │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        └──────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_diff_pane_no_commit_selected() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.left_return_focus = LeftPaneFocus::Commits;
        app.git_ops_state = Some(ops);

        assert_snapshot!(render_diff_pane_body(&app), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Select a commit to preview diff                                               │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        └──────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    /// poll 後に diff_loading=true になることを UI で検証
    #[tokio::test]
    async fn test_diff_pane_after_commit_list_arrives_without_pr() {
        let (mut app, _tx) = make_app();
        app.pr_number = None;

        let mut ops = GitOpsState::new(Vec::new());
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        ops.commit_log.list_receiver = Some(rx);
        ops.commit_log.loading = true;
        ops.left_return_focus = LeftPaneFocus::Commits;
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        let page = crate::github::CommitListPage {
            items: vec![make_commit("local_sha", "local commit")],
            has_more: false,
        };
        tx.send(Ok(page)).await.unwrap();

        app.poll_git_ops_updates();

        // poll 後: diff 取得が開始されている → "Loading diff..." が表示される
        let cl = &app.git_ops_state.as_ref().unwrap().commit_log;
        assert!(cl.diff_loading);
        assert!(cl.diff_receiver.is_some());

        assert_snapshot!(render_diff_pane_body(&app), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Loading diff...                                                               │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        │                                                                              │
        └──────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    // =================================================================
    // Push ローディング / ahead count スナップショット
    // =================================================================

    #[test]
    fn test_footer_pushing_spinner() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ]);
        rebuild_tree(&mut ops);
        ops.pushing = true;
        app.git_ops_state = Some(ops);

        let footer = render_tree_pane_footer(&mut app, true);
        assert!(
            footer.contains("Pushing..."),
            "should show pushing spinner, got: {}",
            footer
        );
    }

    #[test]
    fn test_footer_pushing_spinner_unfocused() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ]);
        rebuild_tree(&mut ops);
        ops.pushing = true;
        app.git_ops_state = Some(ops);

        let footer = render_tree_pane_footer(&mut app, false);
        assert!(
            footer.contains("Pushing..."),
            "pushing spinner should show even when unfocused, got: {}",
            footer
        );
    }

    /// commits pane のタイトルに ahead count を表示
    fn render_commits_pane_text(app: &mut App, is_focused: bool) -> String {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, 60, 10);
        terminal
            .draw(|frame| {
                render_commits_pane(frame, app, area, is_focused);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut lines = Vec::new();
        for y in 0..area.height as usize {
            let mut line = String::new();
            for x in 0..area.width as usize {
                let cell = &buf[(x as u16, y as u16)];
                line.push_str(cell.symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    #[test]
    fn test_commits_pane_title_with_ahead_count() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.commit_log.commits.push(make_commit("abc123", "test commit"));
        ops.commit_log.initialized = true;
        ops.ahead_count = 3;
        app.git_ops_state = Some(ops);

        let output = render_commits_pane_text(&mut app, false);
        assert!(
            output.contains("\u{2191}3"),
            "should show ↑3 in title, got: {}",
            output
        );
    }

    #[test]
    fn test_commits_pane_title_without_ahead() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.commit_log.commits.push(make_commit("abc123", "test commit"));
        ops.commit_log.initialized = true;
        ops.ahead_count = 0;
        app.git_ops_state = Some(ops);

        let output = render_commits_pane_text(&mut app, false);
        assert!(
            !output.contains("\u{2191}"),
            "should not show ↑ when ahead=0, got: {}",
            output
        );
        assert!(output.contains("Commits (1)"), "got: {}", output);
    }

    /// コミット作成後→ahead_count受信→タイトルに↑Nが反映される
    #[tokio::test]
    async fn test_ahead_count_updates_after_commit() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.commit_log.commits.push(make_commit("sha1", "initial"));
        ops.commit_log.initialized = true;
        ops.ahead_count = 0;

        // コミット成功後の状態: ahead_receiver にカウントが送られてくる
        let (ahead_tx, ahead_rx) = tokio::sync::mpsc::channel(1);
        ops.ahead_receiver = Some(ahead_rx);
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        // ahead=0 の時点ではタイトルに ↑ がない
        let before = render_commits_pane_text(&mut app, false);
        assert!(!before.contains("\u{2191}"), "before poll: no arrow, got: {}", before);

        // ahead_count が到着
        ahead_tx.send(2).await.unwrap();
        app.poll_git_ops_updates();

        // poll 後: タイトルに ↑2 が表示される
        let after = render_commits_pane_text(&mut app, false);
        assert!(
            after.contains("\u{2191}2"),
            "after poll: should show ↑2, got: {}",
            after
        );
    }

    /// Push 完了後→ahead_count=0にリセット→↑表示が消える
    #[tokio::test]
    async fn test_ahead_count_resets_after_push() {
        let (mut app, _tx) = make_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.commit_log.commits.push(make_commit("sha1", "initial"));
        ops.commit_log.initialized = true;
        ops.ahead_count = 3;
        ops.pushing = true;

        // push 結果が op_receiver に到着
        let (op_tx, op_rx) = tokio::sync::mpsc::channel(1);
        ops.op_receiver = Some(op_rx);
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        // push 前: ↑3 が表示されている
        let before = render_commits_pane_text(&mut app, false);
        assert!(before.contains("\u{2191}3"), "before push: got: {}", before);

        // push 成功
        op_tx.send(Ok("Pushed to origin/main".to_string())).await.unwrap();
        app.poll_git_ops_updates();

        // push 後: ahead_count=0 にリセット → ↑ 消える
        let after = render_commits_pane_text(&mut app, false);
        assert!(
            !after.contains("\u{2191}"),
            "after push: arrow should disappear, got: {}",
            after
        );
    }

    /// status refresh 後に diff pane が空にならない（フラッシュ防止）
    #[tokio::test]
    async fn test_status_refresh_does_not_flash_diff_pane() {
        let (mut app, _tx) = make_app();
        let entries = vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ];
        let mut ops = GitOpsState::new(entries);
        rebuild_tree(&mut ops);

        // current diff cache をセット（表示中の状態を再現）
        let cache = crate::app::DiffCache {
            file_index: 0,
            patch_hash: 42,
            lines: vec![],
            interner: lasso::Rodeo::new(),
            highlighted: false,
            markdown_rich: false,
        };
        ops.diff_store.set_current("a.rs".to_string(), cache);

        // status_receiver に新しい entries を送る
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        ops.status_receiver = Some(rx);
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        let new_entries = vec![
            entry("a.rs", FileStatus::Modified, FileStatus::Unmodified),
        ];
        tx.send(Ok(new_entries)).await.unwrap();

        app.poll_git_ops_updates();

        // current cache は維持されている（フラッシュなし）
        let ops = app.git_ops_state.as_ref().unwrap();
        assert!(
            ops.diff_store.current.is_some(),
            "current diff cache should be preserved after status refresh"
        );
        // store は空（prefetch で再構築される）
        assert_eq!(
            ops.diff_store.store_len(),
            0,
            "store should be cleared for re-prefetch"
        );
    }
}
