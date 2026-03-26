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

            let items = build_file_list_items_ref(&filtered, display_selected);

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
        let items: Vec<ListItem> = tree
            .visible_rows
            .iter()
            .enumerate()
            .map(|(i, row)| build_tree_row_item(files, row, i == tree.selected_row))
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
        let items = build_file_list_items(files, app.selected_file);

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

    let ai_rally_text = if app.has_background_rally() {
        "A: Resume Rally"
    } else {
        "A: AI Rally"
    };
    let filter_hint = if app.file_list_filter.is_some() {
        "Esc: clear filter"
    } else {
        "Space /: filter"
    };
    let help_text = if app.is_local_mode() {
        format!(
            "j/k/↑↓: move | Enter/→/l: split view | {} | {} | R: refresh | q: quit | ?: help",
            filter_hint, ai_rally_text
        )
    } else {
        format!(
            "j/k/↑↓: move | Enter/→/l: split view | {} | v: viewed | V: viewed dir | O: browser | {}: description | {}: CI checks | a: approve | r: request changes | c: comment | C: comments | {} | R: refresh | q: quit | ?: help",
            filter_hint, app.config.keybindings.pr_description.display(), app.config.keybindings.ci_checks.display(), ai_rally_text
        )
    };
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
) -> Vec<ListItem<'a>> {
    files
        .iter()
        .enumerate()
        .map(|(i, file)| build_file_list_item(file, i == selected_file))
        .collect()
}

fn build_file_list_items_ref<'a>(files: &[&'a ChangedFile], selected: usize) -> Vec<ListItem<'a>> {
    files
        .iter()
        .enumerate()
        .map(|(i, file)| build_file_list_item(file, i == selected))
        .collect()
}

fn build_file_list_item<'a>(file: &'a ChangedFile, is_selected: bool) -> ListItem<'a> {
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
}

pub(crate) fn build_tree_row_item<'a>(
    files: &'a [ChangedFile],
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

            let line = Line::from(vec![
                Span::raw(indent),
                Span::styled(
                    format!("[{}] ", status_char),
                    Style::default().fg(status_color),
                ),
                if file.viewed {
                    Span::styled("✓ ", Style::default().fg(Color::Green))
                } else {
                    Span::raw("  ")
                },
                Span::styled(filename, style),
                Span::raw(format!(" +{} -{}", file.additions, file.deletions)),
            ]);
            ListItem::new(line)
        }
    }
}
