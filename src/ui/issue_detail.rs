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

use crate::app::{App, IssueDetailFocus};
use crate::diff::LineType;

pub fn render(frame: &mut Frame, app: &mut App) {
    {
        let Some(ref state) = app.issue_state else {
            return;
        };
        if state.issue_detail.is_loading() {
            let spinner = app.spinner_char();
            let loading = Paragraph::new(format!("{} Loading issue...", spinner))
                .block(Block::default().borders(Borders::ALL).title("Issue Detail"));
            frame.render_widget(loading, frame.area());
            return;
        }
        if state.issue_detail.as_loaded().is_none() {
            let empty = Paragraph::new("No issue data")
                .block(Block::default().borders(Borders::ALL).title("Issue Detail"));
            frame.render_widget(empty, frame.area());
            return;
        }
    }

    let (
        linked_prs_count,
        linked_prs_loading,
        detail_focus,
        detail_number,
        detail_title,
        detail_state,
        detail_author_login,
        selected_linked_pr,
    ) = {
        let state = app.issue_state.as_ref().unwrap();
        let detail = state.issue_detail.as_loaded().unwrap();
        let lp_count = state.linked_prs.as_loaded().map(|p| p.len()).unwrap_or(0);
        (
            lp_count,
            state.linked_prs.is_loading(),
            state.detail_focus,
            detail.number,
            detail.title.clone(),
            detail.state.clone(),
            detail.author.login.clone(),
            state.selected_linked_pr,
        )
    };

    let linked_prs_height = if linked_prs_loading {
        3u16
    } else if linked_prs_count == 0 {
        0
    } else {
        (linked_prs_count as u16 + 2).min(10)
    };

    let mut constraints = vec![Constraint::Length(3)];
    if linked_prs_height > 0 || linked_prs_loading {
        constraints.push(Constraint::Min(4));
        constraints.push(Constraint::Length(linked_prs_height));
    } else {
        constraints.push(Constraint::Min(4));
    }
    constraints.push(Constraint::Length(1));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let state_icon = "●";
    let state_color = if detail_state.to_lowercase() == "open" {
        Color::Green
    } else {
        Color::Magenta
    };

    let header_line = Line::from(vec![
        Span::styled(format!("{} ", state_icon), Style::default().fg(state_color)),
        Span::styled(
            format!("#{} ", detail_number),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(detail_title, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  by @{}", detail_author_login),
            Style::default().fg(Color::Cyan),
        ),
    ]);

    let header = Paragraph::new(header_line)
        .block(Block::default().borders(Borders::ALL).title("Issue Detail"));
    frame.render_widget(header, chunks[0]);

    let body_border_style = if detail_focus == IssueDetailFocus::Body {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    render_body(frame, app, chunks[1], body_border_style);

    let linked_prs_chunk_idx = if linked_prs_height > 0 || linked_prs_loading {
        Some(2)
    } else {
        None
    };

    if let Some(idx) = linked_prs_chunk_idx {
        let prs_focus = detail_focus == IssueDetailFocus::LinkedPrs;
        let prs_border_style = if prs_focus {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let state = app.issue_state.as_ref().unwrap();
        if state.linked_prs.is_loading() {
            let spinner = app.spinner_char();
            let loading = Paragraph::new(format!("{} Loading linked PRs...", spinner)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(prs_border_style)
                    .title("Linked PRs"),
            );
            frame.render_widget(loading, chunks[idx]);
        } else if let Some(prs) = state.linked_prs.as_loaded() {
            let title = format!("Linked PRs ({})", prs.len());

            let items: Vec<ListItem> = prs
                .iter()
                .enumerate()
                .map(|(i, pr)| {
                    let is_selected = prs_focus && i == selected_linked_pr;

                    let pr_state_icon = if pr.state.to_lowercase() == "open"
                        || pr.state.to_lowercase() == "merged"
                    {
                        let color = if pr.state.to_lowercase() == "merged" {
                            Color::Magenta
                        } else {
                            Color::Green
                        };
                        Span::styled("● ", Style::default().fg(color))
                    } else {
                        Span::styled("● ", Style::default().fg(Color::Red))
                    };

                    let number_style = if is_selected {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Yellow)
                    };

                    let title_style = if is_selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };

                    let mut spans = vec![
                        Span::raw("  "),
                        pr_state_icon,
                        Span::styled(format!("#{} ", pr.number), number_style),
                        Span::styled(pr.title.clone(), title_style),
                        Span::styled(
                            format!("  {}", pr.state),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ];
                    if let Some(ref repo) = pr.repo {
                        spans.push(Span::styled(
                            format!("  ({})", repo),
                            Style::default().fg(Color::Blue),
                        ));
                    }
                    let line = Line::from(spans);

                    ListItem::new(line)
                })
                .collect();

            let mut list_state = ListState::default().with_selected(if prs_focus {
                Some(selected_linked_pr)
            } else {
                None
            });

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(prs_border_style)
                        .title(title),
                )
                .highlight_style(Style::default().bg(Color::DarkGray));
            frame.render_stateful_widget(list, chunks[idx], &mut list_state);
        }
    }

    let footer_idx = chunks.len() - 1;
    let help_text = super::footer::footer_hint_back(&app.config.keybindings);
    let footer_line = super::footer::build_footer_line(app, &help_text);
    let footer = Paragraph::new(footer_line);
    frame.render_widget(footer, chunks[footer_idx]);
}

fn render_body(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect, border_style: Style) {
    let has_cache = app
        .issue_state
        .as_ref()
        .is_some_and(|s| s.issue_detail_cache.is_some());

    if has_cache {
        let (body_lines, total_lines, scroll_offset) = {
            let state = app.issue_state.as_ref().unwrap();
            let cache = state.issue_detail_cache.as_ref().unwrap();

            let lines: Vec<Line> = cache
                .lines
                .iter()
                .filter(|line| line.line_type != LineType::Header)
                .map(|cached_line| {
                    let spans: Vec<Span> = cached_line
                        .spans
                        .iter()
                        .enumerate()
                        .map(|(span_idx, interned)| {
                            let text = cache.resolve(interned.content).to_string();
                            if span_idx == 0 && text.starts_with(' ') {
                                Span::styled(text[1..].to_string(), interned.style)
                            } else {
                                Span::styled(text, interned.style)
                            }
                        })
                        .collect();
                    Line::from(spans)
                })
                .collect();

            let total = lines.len();
            let content_height = area.height.saturating_sub(2) as usize;
            let max_scroll = total.saturating_sub(content_height);
            let offset = state.issue_detail_scroll_offset.min(max_scroll);

            (lines, total, offset)
        };

        if let Some(ref mut state) = app.issue_state {
            state.issue_detail_scroll_offset = scroll_offset;
        }

        let content_height = area.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(content_height);

        let body = Paragraph::new(body_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .title("Body"),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll_offset as u16, 0));
        frame.render_widget(body, area);

        if total_lines > content_height {
            let mut scrollbar_state = ScrollbarState::new(max_scroll + 1).position(scroll_offset);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            frame.render_stateful_widget(
                scrollbar,
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }
    } else {
        let body_text = app
            .issue_state
            .as_ref()
            .and_then(|s| s.issue_detail.as_loaded())
            .and_then(|d| d.body.as_deref())
            .unwrap_or("(no description)")
            .to_string();

        let body = Paragraph::new(body_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .title("Body"),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(body, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppState, IssueState};
    use crate::github::{IssueDetail, User};
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

    fn make_detail(number: u32, title: &str, body: Option<&str>) -> IssueDetail {
        IssueDetail {
            number,
            title: title.to_string(),
            body: body.map(|s| s.to_string()),
            state: "open".to_string(),
            author: User {
                login: "testuser".to_string(),
            },
            labels: vec![],
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-02T00:00:00Z".to_string(),
            comments: vec![],
        }
    }

    #[test]
    fn test_no_issue_data() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueDetail;
        let issue_state = IssueState::new();
        app.issue_state = Some(issue_state);

        assert_snapshot!(render_full(&mut app), @"
        ┌Issue Detail──────────────────────────────────────────────────────────────────────────────────────┐
        │No issue data                                                                                     │
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
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_with_issue_detail() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueDetail;
        let mut issue_state = IssueState::new();
        issue_state.issue_detail = crate::app::LoadState::Loaded(make_detail(
            42,
            "Fix the widget",
            Some("This is the body text."),
        ));
        app.issue_state = Some(issue_state);

        assert_snapshot!(render_full(&mut app), @"
        ┌Issue Detail──────────────────────────────────────────────────────────────────────────────────────┐
        │● #42 Fix the widget  by @testuser                                                                │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Body──────────────────────────────────────────────────────────────────────────────────────────────┐
        │This is the body text.                                                                            │
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
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ? Help | ! Shell | q/Esc Back
        ");
    }

    #[test]
    fn test_with_issue_detail_no_body() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueDetail;
        let mut issue_state = IssueState::new();
        issue_state.issue_detail =
            crate::app::LoadState::Loaded(make_detail(7, "Empty body issue", None));
        app.issue_state = Some(issue_state);

        let output = render_full(&mut app);
        assert!(
            output.contains("(no description)"),
            "should show fallback text"
        );
    }

    #[test]
    fn test_no_issue_state() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueDetail;
        app.issue_state = None;

        let output = render_full(&mut app);
        assert!(!output.contains("Issue Detail") || output.trim().is_empty());
    }
}
