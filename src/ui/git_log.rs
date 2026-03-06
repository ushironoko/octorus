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
use crate::app::{App, AppState, GitLogState};
use crate::github::{format_relative_time, PrCommit};

/// Split View を描画（GitLogSplitCommitList / GitLogSplitDiff）
pub fn render_split(frame: &mut Frame, app: &mut App) {
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

    let is_commit_focused = app.state == AppState::GitLogSplitCommitList;
    let is_diff_focused = app.state == AppState::GitLogSplitDiff;

    render_commit_list_pane(frame, app, h_chunks[0], is_commit_focused);
    render_diff_pane(frame, app, h_chunks[1], is_diff_focused);

    if has_rally {
        render_rally_status_bar(frame, outer_chunks[1], app);
    }
}

/// フルスクリーン diff を描画（GitLogDiffView）
pub fn render_diff_view(frame: &mut Frame, app: &mut App) {
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Diff content
            Constraint::Length(3), // Footer
        ])
        .split(outer_chunks[0]);

    let Some(ref gl) = app.git_log_state else {
        return;
    };

    // Header
    let header_text = gl
        .commits
        .get(gl.selected_commit)
        .map(|c| format!("{} {}", c.short_sha(), c.message))
        .unwrap_or_else(|| "No commit selected".to_string());

    let header = Paragraph::new(header_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title("Commit Diff"),
    );
    frame.render_widget(header, chunks[0]);

    // Diff body
    render_diff_body(frame, gl, chunks[1], Color::Yellow, app.config.diff.bg_color);

    // Footer
    let footer_text = "j/k: scroll | g/G: top/bottom | Ctrl-d/u: page | q/Esc/h: back";
    let footer_line = super::footer::build_footer_line(app, footer_text);
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block_with_border(
        app,
        Style::default().fg(Color::Yellow),
    ));
    frame.render_widget(footer, chunks[2]);

    if has_rally {
        render_rally_status_bar(frame, outer_chunks[1], app);
    }
}

fn render_commit_list_pane(
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
            Constraint::Min(0),    // Commit list
            Constraint::Length(3), // Footer
        ])
        .split(area);

    // Header
    let header_info = if app.is_local_mode() {
        "Local branch".to_string()
    } else {
        format!("PR #{}", app.pr_number.unwrap_or(0))
    };
    let header = Paragraph::new(header_info).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title("Git Log"),
    );
    frame.render_widget(header, chunks[0]);

    // Commit list content — use &mut borrow throughout to allow scroll_offset update
    if let Some(ref mut gl) = app.git_log_state {
        if gl.commits_loading {
            let loading = Paragraph::new(Line::from(Span::styled(
                format!("{} Loading commits...", app.spinner_char()),
                Style::default().fg(Color::Yellow),
            )))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title("Commits"),
            );
            frame.render_widget(loading, chunks[1]);
        } else if let Some(ref error) = gl.commits_error {
            let err_msg = Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("Error: {}", error),
                    Style::default().fg(Color::Red),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Press r to retry",
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title("Commits"),
            );
            frame.render_widget(err_msg, chunks[1]);
        } else if gl.commits.is_empty() {
            let empty = Paragraph::new("No commits")
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color))
                        .title("Commits (0)"),
                );
            frame.render_widget(empty, chunks[1]);
        } else {
            let total = gl.commits.len();
            let selected_commit = gl.selected_commit;
            let items: Vec<ListItem> = gl
                .commits
                .iter()
                .enumerate()
                .map(|(i, commit)| build_commit_list_item(commit, i == selected_commit))
                .collect();

            let title = if total >= 250 {
                format!("Commits ({} - API limit)", total)
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
                .with_offset(gl.commit_list_scroll_offset)
                .with_selected(Some(selected_commit));

            frame.render_stateful_widget(list, chunks[1], &mut list_state);
            gl.commit_list_scroll_offset = list_state.offset();

            if total > 1 {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("▲"))
                    .end_symbol(Some("▼"));

                let mut scrollbar_state =
                    ScrollbarState::new(total.saturating_sub(1)).position(selected_commit);

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
    }

    // Footer
    let help_text = if is_focused {
        "j/k: move | g/G: top/bottom | Enter/Tab/l: diff | q/Esc: back"
    } else {
        "h/Left: focus commits"
    };
    let footer_line = super::footer::build_footer_line(app, help_text);
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block_with_border(
        app,
        Style::default().fg(border_color),
    ));
    frame.render_widget(footer, chunks[2]);
}

fn build_commit_list_item<'a>(commit: &PrCommit, is_selected: bool) -> ListItem<'a> {
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

fn render_diff_pane(
    frame: &mut Frame,
    app: &App,
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
            Constraint::Min(0),    // Diff content
            Constraint::Length(3), // Footer
        ])
        .split(area);

    let Some(ref gl) = app.git_log_state else {
        return;
    };

    // Header
    let header_text = gl
        .commits
        .get(gl.selected_commit)
        .map(|c| format!("{} {}", c.short_sha(), c.message))
        .unwrap_or_else(|| "No commit selected".to_string());

    let header = Paragraph::new(header_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title("Diff Preview"),
    );
    frame.render_widget(header, chunks[0]);

    // Diff body
    render_diff_body(frame, gl, chunks[1], border_color, app.config.diff.bg_color);

    // Footer
    let footer_text = if is_focused {
        "j/k: scroll | g/G: top/bottom | Enter/l: fullscreen | h/Left: commits | q: back"
    } else {
        "Enter/Tab/l: focus diff"
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
    gl: &GitLogState,
    area: ratatui::layout::Rect,
    border_color: Color,
    bg_color: bool,
) {
    if gl.diff_loading {
        let loading = Paragraph::new(Line::from(Span::styled(
            "Loading diff...",
            Style::default().fg(Color::Yellow),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        );
        frame.render_widget(loading, area);
        return;
    }

    if let Some(ref error) = gl.diff_error {
        let err = Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Error: {}", error),
                Style::default().fg(Color::Red),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Press r to retry",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        );
        frame.render_widget(err, area);
        return;
    }

    let lines: Vec<Line> = if let Some(ref cache) = gl.diff_cache {
        let visible_height = area.height.saturating_sub(2) as usize;
        let line_count = cache.lines.len();
        let visible_start = gl.scroll_offset.saturating_sub(2).min(line_count);
        let visible_end = (gl.scroll_offset + visible_height + 5).min(line_count);

        let empty_comments = HashSet::new();
        diff_view::render_cached_lines(
            cache,
            visible_start..visible_end,
            gl.selected_line,
            &empty_comments,
            bg_color,
            None,
        )
    } else {
        vec![Line::from(Span::styled(
            "Select a commit to view diff",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let adjusted_scroll = if gl.diff_cache.is_some() {
        let visible_start = gl.scroll_offset.saturating_sub(2);
        (gl.scroll_offset - visible_start) as u16
    } else {
        gl.scroll_offset as u16
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
    if let Some(ref cache) = gl.diff_cache {
        let total_lines = cache.lines.len();
        let visible_height = area.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible_height);
        if max_scroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            let clamped_position = gl.scroll_offset.min(max_scroll);
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
