use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};

use crate::app::App;
use crate::github::IssueSummary;

pub fn render(frame: &mut Frame, app: &mut App) {
    let Some(ref state) = app.issue_state else {
        return;
    };

    let has_filter_bar = state
        .issue_list_filter
        .as_ref()
        .is_some_and(|f| f.input_active);

    let mut constraints = vec![
        Constraint::Length(3), // Header
        Constraint::Min(0),    // Issue list
    ];
    if has_filter_bar {
        constraints.push(Constraint::Length(3)); // Filter bar
    }
    constraints.push(Constraint::Length(3)); // Footer

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    // Header
    let filter_str = state.issue_list_state_filter.display_name();
    let header_text = format!("Issue List: {} ({})", app.repo, filter_str);
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    // Issue list
    if state.issue_list_loading && state.issues.is_none() {
        let loading = Paragraph::new(format!("{} Loading issues...", app.spinner_char()))
            .block(Block::default().borders(Borders::ALL).title("Issues"));
        frame.render_widget(loading, chunks[1]);
    } else if let Some(ref issues) = state.issues {
        if issues.is_empty() {
            let empty = Paragraph::new("No issues found")
                .block(Block::default().borders(Borders::ALL).title("Issues"));
            frame.render_widget(empty, chunks[1]);
        } else {
            let (display_issues, display_selected, total_display) =
                if let Some(ref filter) = state.issue_list_filter {
                    if filter.matched_indices.is_empty() {
                        let empty_msg = format!("No matches for '{}'", filter.query);
                        let empty = Paragraph::new(empty_msg)
                            .style(Style::default().fg(Color::DarkGray))
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .title(format!("Issues (0/{})", issues.len())),
                            );
                        frame.render_widget(empty, chunks[1]);

                        let mut next_chunk = 2;
                        if has_filter_bar {
                            render_filter_bar(frame, chunks[next_chunk], filter);
                            next_chunk += 1;
                        }
                        render_footer(frame, chunks[next_chunk], app);
                        return;
                    }
                    let filtered: Vec<&IssueSummary> =
                        filter.matched_indices.iter().map(|&i| &issues[i]).collect();
                    let sel = filter.selected.unwrap_or(0);
                    let total = filtered.len();
                    (filtered, sel, total)
                } else {
                    let all: Vec<&IssueSummary> = issues.iter().collect();
                    let sel = state.selected_issue;
                    let total = all.len();
                    (all, sel, total)
                };

            let total_issues = issues.len();
            let title = if let Some(ref filter) = state.issue_list_filter {
                format!("Issues ({}/{})", filter.matched_indices.len(), total_issues)
            } else if state.issue_list_loading {
                format!("Issues ({}) {}", total_issues, app.spinner_char())
            } else if state.issue_list_has_more {
                format!("Issues ({}+)", total_issues)
            } else {
                format!("Issues ({})", total_issues)
            };

            let items = build_issue_list_items(&display_issues, display_selected);

            let mut list_state = ListState::default()
                .with_offset(state.issue_list_scroll_offset)
                .with_selected(Some(display_selected));

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().bg(Color::DarkGray));
            frame.render_stateful_widget(list, chunks[1], &mut list_state);

            // Update scroll offset
            if let Some(ref mut state) = app.issue_state {
                state.issue_list_scroll_offset = list_state.offset();
            }

            // Scrollbar
            if total_display > 1 {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("▲"))
                    .end_symbol(Some("▼"));

                let mut scrollbar_state =
                    ScrollbarState::new(total_display.saturating_sub(1)).position(display_selected);

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
    } else {
        let empty = Paragraph::new("Failed to load issues")
            .block(Block::default().borders(Borders::ALL).title("Issues"));
        frame.render_widget(empty, chunks[1]);
    }

    // Filter bar
    let mut next_chunk = 2;
    if has_filter_bar {
        if let Some(ref state) = app.issue_state {
            if let Some(ref filter) = state.issue_list_filter {
                render_filter_bar(frame, chunks[next_chunk], filter);
            }
        }
        next_chunk += 1;
    }

    // Footer
    render_footer(frame, chunks[next_chunk], app);
}

fn render_filter_bar(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    filter: &crate::filter::ListFilter,
) {
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
    frame.render_widget(filter_bar, area);
}

fn render_footer(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let filter_hint = if app
        .issue_state
        .as_ref()
        .is_some_and(|s| s.issue_list_filter.is_some())
    {
        "Esc: clear filter | "
    } else {
        "Space /: filter | "
    };
    let footer_text = format!(
        "j/k/↑↓: move | Enter: view | {}O: browser | o: open | c: closed | a: all | r: refresh | q: back | ?: help",
        filter_hint
    );
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

fn build_issue_list_items(issues: &[&IssueSummary], selected: usize) -> Vec<ListItem<'static>> {
    issues
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let is_selected = i == selected;

            // State icon
            let state_icon = if issue.state.to_lowercase() == "open" {
                Span::styled("● ", Style::default().fg(Color::Green))
            } else {
                Span::styled("● ", Style::default().fg(Color::Magenta))
            };

            // Issue number
            let number_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let number_span = Span::styled(format!("#{:<5}", issue.number), number_style);

            // Title (truncate)
            let title_width = 50;
            let title = truncate_string(&issue.title, title_width);
            let title_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let title_span = Span::styled(
                format!("{:<width$}", title, width = title_width),
                title_style,
            );

            // Author
            let author_span = Span::styled(
                format!("by @{}", issue.author.login),
                Style::default().fg(Color::Cyan),
            );

            // Labels (show first 2)
            let labels_str = if !issue.labels.is_empty() {
                let label_names: Vec<&str> = issue
                    .labels
                    .iter()
                    .take(2)
                    .map(|l| l.name.as_str())
                    .collect();
                if issue.labels.len() > 2 {
                    format!(" [{}+{}]", label_names.join(", "), issue.labels.len() - 2)
                } else {
                    format!(" [{}]", label_names.join(", "))
                }
            } else {
                String::new()
            };
            let labels_span = Span::styled(labels_str, Style::default().fg(Color::Blue));

            // Comment count
            let comment_count = issue.comments.len();
            let comment_span = if comment_count > 0 {
                Span::styled(
                    format!("  💬 {}", comment_count),
                    Style::default().fg(Color::DarkGray),
                )
            } else {
                Span::raw("")
            };

            let line = Line::from(vec![
                state_icon,
                number_span,
                Span::raw("  "),
                title_span,
                Span::raw("  "),
                author_span,
                labels_span,
                comment_span,
            ]);

            ListItem::new(line)
        })
        .collect()
}

fn truncate_string(s: &str, max_width: usize) -> String {
    if s.chars().count() <= max_width {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_width.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}
