use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame,
};

use crate::app::App;
use crate::diff::LineType;
use crate::ui::common::{build_ci_status_span, build_pr_info};

pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);

    render_body(frame, app, chunks[1]);

    render_footer(frame, app, chunks[2]);
}

fn render_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let pr_info = build_pr_info(app);
    let ci_span = build_ci_status_span(app);
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "PR Description",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" - "),
        Span::styled(pr_info, Style::default().fg(Color::Cyan)),
        ci_span,
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

fn render_body(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let content_height = area.height.saturating_sub(2) as usize;

    let Some(ref cache) = app.pr_description_cache else {
        let no_desc = Paragraph::new(Line::from(Span::styled(
            "  No description provided.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().borders(Borders::ALL).title("Description"));
        frame.render_widget(no_desc, area);
        return;
    };

    let lines: Vec<Line<'_>> = cache
        .lines
        .iter()
        .filter(|cached| cached.line_type != LineType::Header)
        .map(|cached| {
            let spans: Vec<Span<'_>> = cached
                .spans
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let text = cache.resolve(s.content);
                    if i == 0 && text.starts_with(' ') {
                        Span::styled(text[1..].to_string(), s.style)
                    } else {
                        Span::styled(text.to_string(), s.style)
                    }
                })
                .collect();
            Line::from(spans)
        })
        .collect();

    // Scroll offset is measured in *wrapped* (visual) rows because
    // `Paragraph::scroll` applies after wrapping. Count wrapped rows at the inner
    // content width so the clamp reaches the true bottom; using the logical line
    // count would leave the wrapped tail unreachable.
    let inner_width = area.width.saturating_sub(2);
    let body = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total_visual = body.line_count(inner_width);
    let max_scroll = total_visual.saturating_sub(content_height);
    if app.pr_description_scroll_offset > max_scroll {
        app.pr_description_scroll_offset = max_scroll;
    }
    let offset = app.pr_description_scroll_offset;

    let scroll_info = if total_visual > content_height {
        format!(" ({}/{})", offset + 1, max_scroll + 1)
    } else {
        String::new()
    };

    let body = body
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Description{}", scroll_info)),
        )
        .scroll((offset as u16, 0));
    frame.render_widget(body, area);

    if total_visual > content_height {
        let mut scrollbar_state = ScrollbarState::new(max_scroll + 1).position(offset);
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
}

fn render_footer(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let help_text = super::footer::footer_hint_back(&app.config.keybindings);
    let footer = Paragraph::new(Line::from(Span::styled(
        format!(" {}", help_text),
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(footer, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_full(app: &mut App) -> String {
        render_at(app, 100, 24)
    }

    fn render_at(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, app);
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
        lines.join("\n")
    }

    fn app_with_pr_body(body: &str) -> App {
        use crate::app::DataState;
        use crate::github::{Branch, PullRequest, User};

        let (mut app, _) = App::new_loading("owner/repo", 1, crate::config::Config::default());
        let pr = Box::new(PullRequest {
            number: 1,
            node_id: None,
            title: "Test PR".to_string(),
            body: Some(body.to_string()),
            state: "open".to_string(),
            head: Branch {
                ref_name: "feature".to_string(),
                sha: "abc".to_string(),
            },
            base: Branch {
                ref_name: "main".to_string(),
                sha: "def".to_string(),
            },
            user: User {
                login: "user".to_string(),
            },
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        });
        app.data_state = DataState::Loaded { pr, files: vec![] };
        app.open_pr_description();
        app
    }

    /// Regression: a long description whose logical lines wrap across many visual
    /// rows must remain fully scrollable. Jumping to the bottom (offset = MAX) has
    /// to reveal the final line — the scroll clamp must be based on wrapped visual
    /// lines, not logical line count.
    #[test]
    fn test_long_wrapping_description_tail_reachable() {
        let mut body = String::new();
        for i in 0..8 {
            body.push_str(&format!(
                "Paragraph {i} is intentionally long so that it wraps across several visual rows when rendered inside a narrow terminal viewport.\n"
            ));
        }
        body.push_str("UNIQUE_TAIL_MARKER");

        let mut app = app_with_pr_body(&body);
        // Emulate "jump to bottom" (Shift+G sets offset to usize::MAX).
        app.pr_description_scroll_offset = usize::MAX;

        let out = render_at(&mut app, 40, 12);
        assert!(
            out.contains("UNIQUE_TAIL_MARKER"),
            "final line must be reachable when scrolled to the bottom:\n{out}"
        );
    }

    #[test]
    fn test_empty_no_description_cache() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::PrDescription;
        assert_snapshot!(render_full(&mut app), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │PR Description - PR #1                                                                            │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Description───────────────────────────────────────────────────────────────────────────────────────┐
        │  No description provided.                                                                        │
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
}
