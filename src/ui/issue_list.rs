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

use unicode_width::UnicodeWidthStr;

use super::common::truncate_with_width;
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
        .split(frame.area());

    let filter_str = state.issue_list_state_filter.display_name();
    let header_text = format!("Issue List: {} ({})", app.repo, filter_str);
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    if matches!(state.issues, crate::app::LoadState::Loading) {
        let loading = Paragraph::new(format!("{} Loading issues...", app.spinner_char()))
            .block(Block::default().borders(Borders::ALL).title("Issues"));
        frame.render_widget(loading, chunks[1]);
    } else if let Some(issues) = state.issues.as_loaded() {
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
                            super::common::render_filter_bar(frame, chunks[next_chunk], filter);
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
            } else if state.issues.is_loading() {
                format!("Issues ({}) {}", total_issues, app.spinner_char())
            } else if state.issue_list_has_more {
                format!("Issues ({}+)", total_issues)
            } else {
                format!("Issues ({})", total_issues)
            };

            let inner_width = chunks[1].width.saturating_sub(3) as usize;
            let items = build_issue_list_items(&display_issues, display_selected, inner_width);

            let mut list_state = ListState::default()
                .with_offset(state.issue_list_scroll_offset)
                .with_selected(Some(display_selected));

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().bg(Color::DarkGray));
            frame.render_stateful_widget(list, chunks[1], &mut list_state);

            if let Some(ref mut state) = app.issue_state {
                state.issue_list_scroll_offset = list_state.offset();
            }

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

    let mut next_chunk = 2;
    if has_filter_bar {
        if let Some(ref state) = app.issue_state {
            if let Some(ref filter) = state.issue_list_filter {
                super::common::render_filter_bar(frame, chunks[next_chunk], filter);
            }
        }
        next_chunk += 1;
    }

    render_footer(frame, chunks[next_chunk], app);
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
    let help_text = format!(
        "j/k/↑↓: move | Enter: view | {}O: browser | o: open | c: closed | a: all | r: refresh | q: back | ?: help",
        filter_hint
    );
    let line = super::footer::build_footer_line(app, &help_text);
    let footer = Paragraph::new(line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

fn build_issue_list_items(
    issues: &[&IssueSummary],
    selected: usize,
    area_width: usize,
) -> Vec<ListItem<'static>> {
    issues
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let is_selected = i == selected;

            let state_icon = if issue.state.to_lowercase() == "open" {
                Span::styled("● ", Style::default().fg(Color::Green))
            } else {
                Span::styled("● ", Style::default().fg(Color::Magenta))
            };

            let number_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let number_span = Span::styled(format!("#{:<5}", issue.number), number_style);

            let author_width = 4 + issue.author.login.width();
            let fixed_width = 2 + 6 + 2 + 2 + author_width;
            let title_width = area_width.saturating_sub(fixed_width).max(20);
            let title = truncate_with_width(&issue.title, title_width);
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

            let author_span = Span::styled(
                format!("by @{}", issue.author.login),
                Style::default().fg(Color::Cyan),
            );

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppState, IssueState};
    use crate::github::{IssueSummary, Label, User};
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_full(app: &mut App) -> String {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut lines = Vec::new();
        for y in 0..24u16 {
            let mut line = String::new();
            for x in 0..100u16 {
                let cell = &buf[(x, y)];
                line.push_str(cell.symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    fn make_issue(number: u32, title: &str, state: &str, author: &str) -> IssueSummary {
        IssueSummary {
            number,
            title: title.to_string(),
            state: state.to_string(),
            author: User { login: author.to_string() },
            labels: vec![],
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            comments: vec![],
        }
    }

    #[test]
    fn test_empty_issue_list() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueList;
        let mut issue_state = IssueState::new();
        issue_state.issues = crate::app::LoadState::Loaded(vec![]);
        app.issue_state = Some(issue_state);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │Issue List: test/repo (open)                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Issues────────────────────────────────────────────────────────────────────────────────────────────┐
        │No issues found                                                                                   │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │j/k/↑↓: move | Enter: view | Space /: filter | O: browser | o: open | c: closed | a: all | r: refr│
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_issue_list_with_items() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueList;
        let mut issue_state = IssueState::new();
        issue_state.issues = crate::app::LoadState::Loaded(vec![
            make_issue(10, "Fix login bug", "open", "alice"),
            make_issue(9, "Add dark mode", "closed", "bob"),
        ]);
        app.issue_state = Some(issue_state);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │Issue List: test/repo (open)                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Issues (2)────────────────────────────────────────────────────────────────────────────────────────┐
        │● #10     Fix login bug                                                                 by @alice ▲
        │● #9      Add dark mode                                                                   by @bob █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  ▼
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │j/k/↑↓: move | Enter: view | Space /: filter | O: browser | o: open | c: closed | a: all | r: refr│
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_issue_list_with_labels() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueList;
        let mut issue_state = IssueState::new();
        let mut issue = make_issue(5, "Bug report", "open", "carol");
        issue.labels = vec![
            Label { name: "bug".to_string() },
            Label { name: "priority".to_string() },
        ];
        issue_state.issues = crate::app::LoadState::Loaded(vec![issue]);
        app.issue_state = Some(issue_state);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │Issue List: test/repo (open)                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Issues (1)────────────────────────────────────────────────────────────────────────────────────────┐
        │● #5      Bug report                                                                    by @carol │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │j/k/↑↓: move | Enter: view | Space /: filter | O: browser | o: open | c: closed | a: all | r: refr│
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_issue_list_no_state() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueList;
        app.issue_state = None;

        let output = render_full(&mut app);
        assert!(!output.contains("Issues"), "no issue panel when state is None");
    }
}

