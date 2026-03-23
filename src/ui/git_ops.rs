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

/// GitOps 分割ビューを描画
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

    // 横並び: 左35% / 右65%
    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(outer_chunks[0]);

    let is_diff_focused = app.state == AppState::GitOpsSplitDiff;
    let left_focus = app
        .git_ops_state
        .as_ref()
        .map(|ops| ops.left_focus)
        .unwrap_or(LeftPaneFocus::Tree);
    let is_tree_focused = !is_diff_focused && left_focus == LeftPaneFocus::Tree;
    let is_commits_focused = !is_diff_focused && left_focus == LeftPaneFocus::Commits;

    // 左ペインを縦分割: 70% Tree / 30% Commits
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(h_chunks[0]);

    // diff ペインの visible_lines を事前計算してセット
    // 右ペインレイアウト: Header(3) + Diff(Min) + Footer(3) → border含めて -8
    let diff_visible_lines = h_chunks[1].height.saturating_sub(8) as usize;
    if let Some(ref mut ops) = app.git_ops_state {
        ops.diff_scroll.set_visible_lines(diff_visible_lines);
        ops.commit_log.diff_scroll.set_visible_lines(diff_visible_lines);
    }

    // &mut app が必要なペインを先に描画（scroll_offset 更新のため）
    render_tree_pane(frame, app, left_chunks[0], is_tree_focused);
    render_commits_pane(frame, app, left_chunks[1], is_commits_focused);
    // &app で十分なペインを後に描画
    render_diff_pane(frame, &*app, h_chunks[1], is_diff_focused);

    if has_rally {
        render_rally_status_bar(frame, outer_chunks[1], app);
    }
}

/// 左ペイン: ツリービュー
fn render_tree_pane(
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Tree list
            Constraint::Length(3), // Footer
        ])
        .split(area);

    let Some(ref ops) = app.git_ops_state else {
        return;
    };

    // Header: "Git Status" with counts
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

    // Tree list
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

        // Update scroll offset from list state
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

    // Footer
    let help_text = if is_focused {
        let Some(ref ops) = app.git_ops_state else {
            return;
        };
        let base = "j/k: move | Space: stage | s: all | d: discard | c: commit | u: undo | R: refresh | P: push | Tab: commits";
        if let Some((ref msg, _)) = ops.op_message {
            // op_message が表示中の場合はそちらを優先
            msg.as_str()
        } else {
            base
        }
    } else {
        "Tab/h: focus tree"
    };
    let footer_line = super::footer::build_footer_line(app, help_text);
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block_with_border(
        app,
        Style::default().fg(border_color),
    ));
    frame.render_widget(footer, chunks[2]);
}

/// 右ペイン: Diff プレビュー
fn render_diff_pane(frame: &mut Frame, app: &App, area: ratatui::layout::Rect, is_focused: bool) {
    let border_color = if is_focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Diff content
            Constraint::Length(3), // Footer
        ])
        .split(area);

    let bg_color = app.config.diff.bg_color;

    let Some(ref ops) = app.git_ops_state else {
        return;
    };

    let is_commit_diff = ops.left_return_focus == LeftPaneFocus::Commits;

    // Header
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

    // Diff body — left_return_focus に応じて diff_store / scroll を切り替え
    if is_commit_diff {
        render_commit_diff_body(frame, &ops.commit_log, chunks[1], border_color, bg_color);
    } else {
        render_diff_body(frame, ops, chunks[1], border_color, bg_color);
    }

    // Footer
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

/// Diff 本文を描画
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

    // Scrollbar
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

/// コミット diff 本文を描画
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

    // Scrollbar
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

/// 左下ペイン: コミット一覧
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

    let title = if cl.has_more {
        format!("Commits ({}+)", total)
    } else {
        format!("Commits ({})", total)
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

/// コミットアイテムを構築
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

/// ツリー行をリストアイテムに変換
fn build_tree_row_item<'a>(
    ops: &GitOpsState,
    row: &TreeRow,
    is_selected: bool,
) -> ListItem<'a> {
    match row {
        TreeRow::Dir(path, depth, expanded) => {
            let indent = "  ".repeat(*depth);
            let icon = if *expanded { "▼" } else { "▶" };
            // ディレクトリ名はパスの最後のコンポーネント
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
        TreeRow::File(idx, depth) => {
            let Some(entry) = ops.entries.get(*idx) else {
                return ListItem::new(Line::from(""));
            };

            let indent = "  ".repeat(*depth);

            // 変更種別ラベル: ファイルの性質を表す固定テキスト（stage/unstageで不変）
            let label = entry.change_type_label();

            // 色だけでstaged/unstagedを区別
            let status_color = status_color_for_entry(entry);

            // ファイル名はパスの最後のコンポーネント
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

            // 行数情報（あれば）
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
        // staged のみ
        (_, FileStatus::Unmodified) if entry.is_staged() => Color::Green,
        // untracked
        (FileStatus::Untracked, FileStatus::Untracked) => Color::Magenta,
        // worktree 変更あり
        (_, wt) if wt != FileStatus::Unmodified && wt != FileStatus::Ignored => Color::Red,
        // staged + worktree 変更あり（MM 等）
        _ if entry.is_staged() && entry.has_worktree_changes() => Color::Yellow,
        _ => Color::White,
    }
}

/// ステータスのカウントを集計
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
