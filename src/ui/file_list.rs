use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};

use super::common::{
    build_ci_status_span, build_pr_info, render_rally_status_bar, render_update_bar,
};
use crate::app::App;
use crate::app::TreeRow;
use crate::github::ChangedFile;
use std::collections::HashMap;

pub fn render(frame: &mut Frame, app: &mut App) {
    let has_rally = app.has_background_rally();
    let has_update = app.update_available.is_some();
    let has_filter_bar = app
        .file_list_filter
        .as_ref()
        .is_some_and(|f| f.input_active);

    let mut constraints = vec![Constraint::Length(3), Constraint::Min(0)];
    if has_filter_bar {
        constraints.push(Constraint::Length(3));
    }
    if has_update {
        constraints.push(Constraint::Length(1));
    }
    if has_rally {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(3));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let pr_info = build_pr_info(app);
    let ci_span = build_ci_status_span(app);

    let header = Paragraph::new(Line::from(vec![Span::raw(pr_info), ci_span]))
        .block(Block::default().borders(Borders::ALL).title("octorus"));
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
                        .title(format!("Changed Files (0/{})", total_files)),
                );
            frame.render_widget(empty, chunks[1]);
        } else {
            let filtered: Vec<&ChangedFile> =
                filter.matched_indices.iter().map(|&i| &files[i]).collect();
            let display_selected = filter.selected.unwrap_or(0);
            let display_count = filtered.len();

            let items = build_file_list_items_ref(
                &filtered,
                display_selected,
                &app.cmt.file_comment_counts,
            );

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Changed Files ({}/{})", display_count, total_files)),
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
        let max_count = files
            .iter()
            .filter_map(|f| app.cmt.file_comment_counts.get(&f.filename).copied())
            .max()
            .unwrap_or(0);
        let col_width = comment_col_width(max_count);
        let items: Vec<ListItem> = tree
            .visible_rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                build_tree_row_item(
                    files,
                    row,
                    i == tree.selected_row,
                    &app.cmt.file_comment_counts,
                    col_width,
                )
            })
            .collect();

        let title = format!("Changed Files ({}) [tree]", total_files);
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
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
        let items = build_file_list_items(files, app.selected_file, &app.cmt.file_comment_counts);

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Changed Files ({})", total_files)),
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
            super::common::render_filter_bar(frame, chunks[next_chunk], filter);
        }
        next_chunk += 1;
    }

    if has_update {
        render_update_bar(frame, chunks[next_chunk], app);
        next_chunk += 1;
    }

    if has_rally {
        render_rally_status_bar(frame, chunks[next_chunk], app);
        next_chunk += 1;
    }

    let help_text = super::footer::footer_hint_back(&app.config.keybindings);
    let footer_line = super::footer::build_footer_line(app, &help_text);
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block(app));
    frame.render_widget(footer, chunks[next_chunk]);
}

pub fn render_loading(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let header_text = if app.is_local_mode() {
        let af = if app.is_local_auto_focus() { " AF" } else { "" };
        format!("[LOCAL{}] Loading...", af)
    } else {
        match app.pr_number {
            Some(n) => format!("PR #{} - Loading...", n),
            None => "Loading...".to_string(),
        }
    };
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    let loading_msg = if app.is_local_mode() {
        format!("{} Loading local diff...", app.spinner_char())
    } else {
        format!("{} Loading PR data...", app.spinner_char())
    };
    let loading = Paragraph::new(loading_msg)
        .style(Style::default().fg(Color::Yellow))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Changed Files"),
        );
    frame.render_widget(loading, chunks[1]);

    let footer = Paragraph::new(format!("{} Please wait... (q: quit)", app.spinner_char()))
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[2]);
}

pub fn render_error(frame: &mut Frame, app: &App, error_msg: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let header_text = if app.is_local_mode() {
        "[LOCAL] Error".to_string()
    } else {
        match app.pr_number {
            Some(n) => format!("PR #{} - Error", n),
            None => "Error".to_string(),
        }
    };
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    let error = Paragraph::new(format!("Error: {}", error_msg))
        .style(Style::default().fg(Color::Red))
        .block(Block::default().borders(Borders::ALL).title("Error"));
    frame.render_widget(error, chunks[1]);

    let footer = Paragraph::new("r: retry | q: quit").block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[2]);
}

pub(crate) fn build_file_list_items<'a>(
    files: &'a [ChangedFile],
    selected_file: usize,
    comment_counts: &HashMap<String, usize>,
) -> Vec<ListItem<'a>> {
    let max_count = files
        .iter()
        .filter_map(|f| comment_counts.get(&f.filename).copied())
        .max()
        .unwrap_or(0);
    let col_width = comment_col_width(max_count);
    files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let count = comment_counts.get(&file.filename).copied().unwrap_or(0);
            build_file_list_item(file, i == selected_file, count, col_width)
        })
        .collect()
}

pub(crate) fn build_file_list_items_ref<'a>(
    files: &[&'a ChangedFile],
    selected: usize,
    comment_counts: &HashMap<String, usize>,
) -> Vec<ListItem<'a>> {
    let max_count = files
        .iter()
        .filter_map(|f| comment_counts.get(&f.filename).copied())
        .max()
        .unwrap_or(0);
    let col_width = comment_col_width(max_count);
    files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let count = comment_counts.get(&file.filename).copied().unwrap_or(0);
            build_file_list_item(file, i == selected, count, col_width)
        })
        .collect()
}

fn comment_label(count: usize) -> String {
    if count > 999 {
        "[1k+]".to_string()
    } else {
        format!("[{}]", count)
    }
}

pub(crate) fn comment_col_width(max_count: usize) -> usize {
    if max_count == 0 {
        1
    } else {
        comment_label(max_count).len() + 2
    }
}

fn build_comment_column(count: usize, col_width: usize) -> Span<'static> {
    if count == 0 {
        return Span::raw(" ".repeat(col_width));
    }
    let label = comment_label(count);
    let pad = col_width - label.len() - 1;
    Span::styled(
        format!("{}{} ", " ".repeat(pad), label),
        Style::default().fg(Color::Magenta),
    )
}

fn build_file_list_item<'a>(
    file: &'a ChangedFile,
    is_selected: bool,
    comment_count: usize,
    col_width: usize,
) -> ListItem<'a> {
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

    let comment_span = build_comment_column(comment_count, col_width);

    let spans = vec![
        Span::styled(
            format!("[{}]", status_char),
            Style::default().fg(status_color),
        ),
        comment_span,
        if file.viewed {
            Span::styled("✓ ", Style::default().fg(Color::Green))
        } else {
            Span::raw("  ")
        },
        Span::styled(&file.filename, style),
        Span::raw(format!(" +{} -{}", file.additions, file.deletions)),
    ];

    ListItem::new(Line::from(spans))
}

pub(crate) fn build_tree_row_item<'a>(
    files: &'a [ChangedFile],
    row: &TreeRow,
    is_selected: bool,
    comment_counts: &HashMap<String, usize>,
    col_width: usize,
) -> ListItem<'a> {
    match row {
        TreeRow::Dir {
            ref path,
            depth,
            expanded,
        } => {
            let indent = "  ".repeat(*depth);
            let icon = if *expanded { "▼" } else { "▶" };
            let dir_name = path.rsplit_once('/').map(|(_, name)| name).unwrap_or(path);

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
            let Some(file) = files.get(*index) else {
                return ListItem::new(Line::from(""));
            };
            let indent = "  ".repeat(*depth);
            let filename = file
                .filename
                .rsplit_once('/')
                .map(|(_, name)| name)
                .unwrap_or(&file.filename);

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

            let count = comment_counts.get(&file.filename).copied().unwrap_or(0);
            let comment_span = build_comment_column(count, col_width);

            let spans = vec![
                Span::raw(indent),
                Span::styled(
                    format!("[{}]", status_char),
                    Style::default().fg(status_color),
                ),
                comment_span,
                if file.viewed {
                    Span::styled("✓ ", Style::default().fg(Color::Green))
                } else {
                    Span::raw("  ")
                },
                Span::styled(filename, style),
                Span::raw(format!(" +{} -{}", file.additions, file.deletions)),
            ];
            ListItem::new(Line::from(spans))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    fn col_text(count: usize, col_width: usize) -> String {
        let span = build_comment_column(count, col_width);
        span.content.to_string()
    }

    #[test]
    fn comment_column_no_comments_collapses() {
        let col = comment_col_width(0);
        assert_eq!(col, 1);
        assert_snapshot!(col_text(0, col), @" ");
    }

    #[test]
    fn comment_column_single_digit() {
        let col = comment_col_width(9);
        assert_eq!(col, 5);
        assert_snapshot!(col_text(3, col), @" [3] ");
        assert_snapshot!(col_text(0, col), @"     ");
    }

    #[test]
    fn comment_column_double_digit() {
        let col = comment_col_width(42);
        assert_eq!(col, 6);
        assert_snapshot!(col_text(42, col), @" [42] ");
        assert_snapshot!(col_text(1, col), @"  [1] ");
        assert_snapshot!(col_text(0, col), @"      ");
    }

    #[test]
    fn comment_column_triple_digit() {
        let col = comment_col_width(123);
        assert_eq!(col, 7);
        assert_snapshot!(col_text(123, col), @" [123] ");
        assert_snapshot!(col_text(7, col), @"   [7] ");
        assert_snapshot!(col_text(0, col), @"       ");
    }

    #[test]
    fn comment_column_overflow_clamps_to_1k_plus() {
        let col = comment_col_width(2500);
        assert_eq!(col, 7);
        assert_snapshot!(col_text(2500, col), @" [1k+] ");
    }

    #[test]
    fn comment_col_width_values() {
        assert_eq!(comment_col_width(0), 1);
        assert_eq!(comment_col_width(1), 5);
        assert_eq!(comment_col_width(9), 5);
        assert_eq!(comment_col_width(10), 6);
        assert_eq!(comment_col_width(99), 6);
        assert_eq!(comment_col_width(100), 7);
        assert_eq!(comment_col_width(999), 7);
        assert_eq!(comment_col_width(1000), 7);
    }
}
