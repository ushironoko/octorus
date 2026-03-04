use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::app::App;
use crate::ui::common::build_pr_info;

pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),   // Body
            Constraint::Length(1), // Footer
        ])
        .split(frame.area());

    // Header
    render_header(frame, app, chunks[0]);

    // Body
    render_body(frame, app, chunks[1]);

    // Footer
    render_footer(frame, app, chunks[2]);
}

fn render_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let pr_info = build_pr_info(app);
    let header = Paragraph::new(Line::from(vec![
        Span::styled("PR Description", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" - "),
        Span::styled(pr_info, Style::default().fg(Color::Cyan)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

fn render_body(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let content_height = area.height.saturating_sub(2) as usize;

    let Some(ref cache) = app.pr_description_cache else {
        // No description
        let no_desc = Paragraph::new(Line::from(Span::styled(
            "  No description provided.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().borders(Borders::ALL).title("Description"));
        frame.render_widget(no_desc, area);
        return;
    };

    let total_lines = cache.lines.len();
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

    let start = app.pr_description_scroll_offset;
    let end = (start + content_height).min(total_lines);

    // render_cached_lines を使うが、context行の先頭スペース(diff marker)を除去する
    let lines: Vec<Line<'_>> = cache.lines[start..end]
        .iter()
        .map(|cached| {
            let spans: Vec<Span<'_>> = cached
                .spans
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let text = cache.resolve(s.content);
                    // 最初のspanの先頭1文字がdiff marker(スペース)なので除去
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

    let body = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Description{}", scroll_info)),
    );
    frame.render_widget(body, area);

    // Scrollbar
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
        " {}/Esc: close | j/k: scroll | J/K: page | g/G: top/bottom | o: open in browser",
        kb.quit.display()
    );
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(footer, area);
}
