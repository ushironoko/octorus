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

    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(content_height);
    if app.pr_description_scroll_offset > max_scroll {
        app.pr_description_scroll_offset = max_scroll;
    }

    let scroll_info = if total_lines > content_height {
        format!(
            " ({}/{})",
            app.pr_description_scroll_offset + 1,
            max_scroll + 1
        )
    } else {
        String::new()
    };

    let body = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Description{}", scroll_info)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.pr_description_scroll_offset as u16, 0));
    frame.render_widget(body, area);

    if total_lines > content_height {
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll + 1).position(app.pr_description_scroll_offset);
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
    let kb = &app.config.keybindings;
    let footer_text = format!(
        " {}/Esc: close | j/k: scroll | J/K: page | g/G: top/bottom | {}: open in browser | {}: toggle rich",
        kb.quit.display(),
        kb.open_in_browser.display(),
        kb.toggle_markdown_rich.display()
    );
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
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
         q/Esc/Esc: close | j/k: scroll | J/K: page | g/G: top/bottom | O: open in browser | M: toggle rich
        ");
    }
}
