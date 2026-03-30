use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};

use super::common::render_rally_status_bar;
use super::diff_view;
use super::file_list::{build_file_list_items, build_tree_row_item};
use crate::app::{App, AppState, DataState};
use crate::github::ChangedFile;

pub fn render(frame: &mut Frame, app: &mut App) {
    let has_rally = app.has_background_rally();

    let outer_constraints = if has_rally {
        vec![
            Constraint::Min(0),
            Constraint::Length(1),
        ]
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

    let is_file_focused = app.state == AppState::SplitViewFileList;
    let is_diff_focused = app.state == AppState::SplitViewDiff;

    render_file_list_pane(frame, app, h_chunks[0], is_file_focused);
    render_diff_pane(frame, app, h_chunks[1], is_diff_focused);

    if has_rally {
        render_rally_status_bar(frame, outer_chunks[1], app);
    }
}

fn render_file_list_pane(
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

    let has_filter_bar = app
        .file_list_filter
        .as_ref()
        .is_some_and(|f| f.input_active);

    let mut constraints = vec![
        Constraint::Length(3),
        Constraint::Min(0),
    ];
    if has_filter_bar {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Length(3));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let pr_info = match &app.data_state {
        DataState::Loaded { pr, .. } => {
            format!("PR #{}: {}", pr.number, pr.title)
        }
        _ => match app.pr_number {
            Some(n) => format!("PR #{}", n),
            None => "PR".to_string(),
        },
    };

    let header = Paragraph::new(pr_info).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title("octorus"),
    );
    frame.render_widget(header, chunks[0]);

    let files = app.files();
    let total_files = files.len();

    if let Some(ref filter) = app.file_list_filter {
        if filter.matched_indices.is_empty() {
            let empty_msg = format!("No matches for '{}'", filter.query);
            let empty = Paragraph::new(empty_msg)
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color))
                        .title(format!("Files (0/{})", total_files)),
                );
            frame.render_widget(empty, chunks[1]);
        } else {
            let filtered: Vec<&ChangedFile> =
                filter.matched_indices.iter().map(|&i| &files[i]).collect();
            let display_selected = filter.selected.unwrap_or(0);
            let display_count = filtered.len();

            let items = build_file_list_items_ref(&filtered, display_selected);

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color))
                        .title(format!("Files ({}/{})", display_count, total_files)),
                )
                .highlight_style(Style::default().bg(Color::DarkGray));

            let mut list_state = ListState::default()
                .with_offset(app.file_list_scroll_offset)
                .with_selected(Some(display_selected));

            frame.render_stateful_widget(list, chunks[1], &mut list_state);

            app.file_list_scroll_offset = list_state.offset();

            if display_count > 1 {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("▲"))
                    .end_symbol(Some("▼"));

                let mut scrollbar_state =
                    ScrollbarState::new(display_count.saturating_sub(1)).position(display_selected);

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
    } else if app.is_file_tree_active() {
        let tree = app.file_tree_state.as_ref().unwrap();
        let row_count = tree.row_count();
        let items: Vec<ListItem> = tree
            .visible_rows
            .iter()
            .enumerate()
            .map(|(i, row)| build_tree_row_item(files, row, i == tree.selected_row))
            .collect();

        let title = format!("Files ({}) [tree]", total_files);
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(title),
            )
            .highlight_style(Style::default().bg(Color::DarkGray));

        let mut list_state = ListState::default()
            .with_offset(tree.scroll_offset)
            .with_selected(Some(tree.selected_row));

        frame.render_stateful_widget(list, chunks[1], &mut list_state);

        if let Some(ref mut tree) = app.file_tree_state {
            tree.scroll_offset = list_state.offset();
        }

        if row_count > 1 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));
            let selected = app.file_tree_state.as_ref().map_or(0, |t| t.selected_row);
            let mut scrollbar_state =
                ScrollbarState::new(row_count.saturating_sub(1)).position(selected);
            frame.render_stateful_widget(
                scrollbar,
                chunks[1].inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }
    } else {
        let items = build_file_list_items(files, app.selected_file);

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(format!("Files ({})", total_files)),
            )
            .highlight_style(Style::default().bg(Color::DarkGray));

        let mut list_state = ListState::default()
            .with_offset(app.file_list_scroll_offset)
            .with_selected(Some(app.selected_file));

        frame.render_stateful_widget(list, chunks[1], &mut list_state);

        app.file_list_scroll_offset = list_state.offset();
        if let Some(sel) = list_state.selected() {
            app.selected_file = sel;
        }

        if total_files > 1 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            let mut scrollbar_state =
                ScrollbarState::new(total_files.saturating_sub(1)).position(app.selected_file);

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

    let mut next_chunk = 2;

    if has_filter_bar {
        if let Some(ref filter) = app.file_list_filter {
            let cursor_display = format!("/{}", filter.query);
            let filter_bar = Paragraph::new(Line::from(vec![
                Span::styled("Filter: ", Style::default().fg(Color::Cyan)),
                Span::styled(cursor_display, Style::default().fg(Color::White)),
                Span::styled("│", Style::default().fg(Color::DarkGray)),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
            frame.render_widget(filter_bar, chunks[next_chunk]);
        }
        next_chunk += 1;
    }

    let help_text_owned;
    let help_text = if is_focused {
        help_text_owned = super::footer::footer_hint_back(&app.config.keybindings);
        help_text_owned.as_str()
    } else {
        "←/h: focus files"
    };
    let footer_line = super::footer::build_footer_line(app, help_text);
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block_with_border(
        app,
        Style::default().fg(border_color),
    ));
    frame.render_widget(footer, chunks[next_chunk]);
}

fn build_file_list_items_ref<'a>(files: &[&'a ChangedFile], selected: usize) -> Vec<ListItem<'a>> {
    use ratatui::style::Modifier;

    files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let is_selected = i == selected;
            let style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let status_color = match file.status.as_str() {
                "added" => Color::Green,
                "removed" => Color::Red,
                "modified" => Color::Yellow,
                "copied" => Color::Cyan,
                _ => Color::White,
            };

            let status_char = match file.status.as_str() {
                "added" => 'A',
                "removed" => 'D',
                "modified" => 'M',
                "renamed" => 'R',
                "copied" => 'C',
                _ => '?',
            };

            let line = Line::from(vec![
                Span::styled(
                    format!("[{}] ", status_char),
                    Style::default().fg(status_color),
                ),
                if file.viewed {
                    Span::styled("✓ ", Style::default().fg(Color::Green))
                } else {
                    Span::raw("  ")
                },
                Span::styled(&file.filename, style),
                Span::raw(format!(" +{} -{}", file.additions, file.deletions)),
            ]);

            ListItem::new(line)
        })
        .collect()
}

fn render_diff_pane(frame: &mut Frame, app: &App, area: ratatui::layout::Rect, is_focused: bool) {
    let border_color = if is_focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let has_inline_comment = is_focused && app.cmt.comment_panel_open;

    if has_inline_comment {
        render_diff_pane_with_comments(frame, app, area, border_color);
    } else {
        render_diff_pane_normal(frame, app, area, border_color, is_focused);
    }
}

fn render_diff_pane_normal(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    border_color: Color,
    is_focused: bool,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    render_diff_header(frame, app, chunks[0], border_color);
    render_diff_body(frame, app, chunks[1], border_color);

    let footer_text_owned;
    let footer_text = if is_focused {
        footer_text_owned = super::footer::footer_hint_back(&app.config.keybindings);
        footer_text_owned.as_str()
    } else {
        "Enter/→: focus diff"
    };

    render_diff_footer(frame, app, chunks[2], footer_text, border_color);
}

fn render_diff_footer(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    help_text: &str,
    border_color: Color,
) {
    let footer_line = super::footer::build_footer_line(app, help_text);
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block_with_border(
        app,
        Style::default().fg(border_color),
    ));
    frame.render_widget(footer, area);
}

fn render_diff_pane_with_comments(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    border_color: Color,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(50),
            Constraint::Percentage(40),
            Constraint::Length(3),
        ])
        .split(area);

    render_diff_header(frame, app, chunks[0], border_color);
    render_diff_body(frame, app, chunks[1], border_color);

    let indices = app.get_comment_indices_at_current_line();
    let mut lines: Vec<Line> = vec![];

    if indices.is_empty() {
        lines.push(Line::from(Span::styled(
            "No comments. c: comment, s: suggestion",
            Style::default().fg(Color::DarkGray),
        )));
    } else if let Some(ref comments) = app.cmt.review_comments {
        for (i, &idx) in indices.iter().enumerate() {
            let Some(comment) = comments.get(idx) else {
                continue;
            };

            if i > 0 {
                lines.push(Line::from(Span::styled(
                    "───────────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
            }

            lines.push(Line::from(vec![
                Span::styled(
                    format!("@{}", comment.user.login),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!(" (line {})", comment.line.unwrap_or(0)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));

            for line in comment.body.lines() {
                lines.push(Line::from(line.to_string()));
            }
            lines.push(Line::from(""));
        }
    }

    let title = "Comments (j/k/↑↓: scroll, c: comment, s: suggest, r: reply)";
    let total_lines = lines.len();

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(title),
        )
        .wrap(Wrap { trim: true })
        .scroll((app.cmt.comment_panel_scroll, 0));
    frame.render_widget(paragraph, chunks[2]);

    if total_lines > 1 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let max_scroll = total_lines.saturating_sub(1);
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll).position(app.cmt.comment_panel_scroll as usize);

        frame.render_stateful_widget(
            scrollbar,
            chunks[2].inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }

    let footer_text = "j/k/↑↓: scroll | n/N: jump | Tab: switch | r: reply | c: comment | s: suggest | →/l: fullscreen | ←/h/q: close";
    render_diff_footer(frame, app, chunks[3], footer_text, border_color);
}

fn render_diff_header(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    border_color: Color,
) {
    let header_text = app
        .files()
        .get(app.selected_file)
        .map(|file| {
            format!(
                "{} (+{} -{})",
                file.filename, file.additions, file.deletions
            )
        })
        .unwrap_or_else(|| "No file selected".to_string());

    let header = Paragraph::new(header_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title("Diff Preview"),
    );
    frame.render_widget(header, area);
}

fn render_diff_body(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    border_color: Color,
) {
    let visible_height = area.height.saturating_sub(2) as usize;
    let (lines, scroll_row) = if let Some(ref cache) = app.diff_store.current {
        let line_count = cache.lines.len();
        // Slice from scroll_offset, bounded to visible viewport + buffer for wrap handling.
        let max_scroll = line_count.saturating_sub(visible_height);
        let start = app.diff_scroll.scroll_offset.min(max_scroll);
        let end = (start + visible_height + 10).min(line_count);
        let multiline_range = app
            .multiline_selection
            .as_ref()
            .map(|s| (s.start(), s.end()));
        let rendered = diff_view::render_cached_lines(
            cache,
            start..end,
            app.diff_scroll.selected_line,
            &app.cmt.file_comment_lines,
            app.config.diff.bg_color,
            multiline_range,
        );
        (rendered, 0u16)
    } else {
        let file = app.files().get(app.selected_file);
        let rendered = match file {
            Some(f) => match f.patch.as_ref() {
                Some(_) => vec![Line::from("Loading diff...")],
                None => {
                    if app.is_lazy_diff_loading() {
                        vec![Line::from("Loading diff...")]
                    } else {
                        vec![Line::from("No diff available")]
                    }
                }
            },
            None => vec![Line::from("No file selected")],
        };
        (rendered, app.diff_scroll.scroll_offset as u16)
    };

    let diff_block = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll_row, 0));

    frame.render_widget(diff_block, area);

    if let Some(ref cache) = app.diff_store.current {
        let total_lines = cache.lines.len();
        let visible_height = area.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible_height);
        if max_scroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            let clamped_position = app.diff_scroll.scroll_offset.min(max_scroll);
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
