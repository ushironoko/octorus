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
use unicode_width::UnicodeWidthChar;

use crate::app::App;
use crate::github::IssueComment;

/// Wrap text to fit within the specified width, handling multibyte characters
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for ch in text.chars() {
        if ch == '\n' {
            continue;
        }

        let char_width = ch.width().unwrap_or(1);

        if current_width + char_width > max_width {
            lines.push(current_line);
            current_line = String::new();
            current_width = 0;
        }

        current_line.push(ch);
        current_width += char_width;
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let Some(ref state) = app.issue_state else {
        return;
    };

    if state.issue_comment_detail_mode {
        render_detail(frame, app);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Comment list
            Constraint::Length(1), // Footer
        ])
        .split(frame.area());

    // Header
    let comment_count = state.issue_comments.as_ref().map(|c| c.len()).unwrap_or(0);
    let issue_number = state.issue_detail.as_ref().map(|d| d.number).unwrap_or(0);
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" Issue #{} ", issue_number),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("Comments ({})", comment_count),
            Style::default().fg(Color::White),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    // Comment list
    render_list(frame, app, chunks[1]);

    // Footer
    let kb = &app.config.keybindings;
    let footer_text = format!(
        " {}/Esc: back | j/k: move | Enter: detail | {}: browser",
        kb.quit.display(),
        kb.open_in_browser.display(),
    );
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(footer, chunks[2]);
}

fn render_list(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let Some(ref mut state) = app.issue_state else {
        return;
    };

    let Some(ref comments) = state.issue_comments else {
        let empty = Paragraph::new("No comments")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    };

    if comments.is_empty() {
        let empty = Paragraph::new("No comments")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    }

    let available_width = area.width.saturating_sub(4) as usize;
    let body_width = available_width.saturating_sub(4);

    let items: Vec<ListItem> = comments
        .iter()
        .enumerate()
        .map(|(i, comment)| {
            format_comment_item(comment, i, i == state.selected_issue_comment, body_width)
        })
        .collect();

    let total_items = comments.len();

    let mut list_state = ListState::default()
        .with_offset(state.issue_comment_list_scroll_offset)
        .with_selected(Some(state.selected_issue_comment));

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut list_state);

    // Update scroll offset from ListState
    state.issue_comment_list_scroll_offset = list_state.offset();

    // Scrollbar
    if total_items > 1 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{25b2}"))
            .end_symbol(Some("\u{25bc}"));

        let mut scrollbar_state = ScrollbarState::new(total_items.saturating_sub(1))
            .position(state.selected_issue_comment);

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

fn format_comment_item(
    comment: &IssueComment,
    _index: usize,
    is_selected: bool,
    body_width: usize,
) -> ListItem<'static> {
    let prefix = if is_selected { "> " } else { "  " };

    let date = comment
        .created_at
        .split('T')
        .next()
        .unwrap_or(&comment.created_at);

    let mut header_spans = vec![
        Span::raw(prefix.to_string()),
        Span::styled(
            format!("@{}", comment.author.login),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(date.to_string(), Style::default().fg(Color::DarkGray)),
    ];

    if !comment.author_association.is_empty() && comment.author_association != "NONE" {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            comment.author_association.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }

    let header_line = Line::from(header_spans);

    // Truncate body for list view
    let body_text: String = comment.body.lines().collect::<Vec<_>>().join(" ");
    let truncated = if body_text.len() > body_width * 2 {
        format!("{}...", &body_text[..body_width * 2])
    } else {
        body_text
    };
    let wrapped_lines = wrap_text(&truncated, body_width);

    let mut lines = vec![header_line];
    for wrapped_line in wrapped_lines {
        lines.push(Line::from(vec![Span::raw("    "), Span::raw(wrapped_line)]));
    }
    lines.push(Line::from(""));

    ListItem::new(lines)
}

fn render_detail(frame: &mut Frame, app: &App) {
    let Some(ref state) = app.issue_state else {
        return;
    };
    let Some(ref comments) = state.issue_comments else {
        return;
    };
    let Some(comment) = comments.get(state.selected_issue_comment) else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(frame.area());

    // Header
    let date = comment
        .created_at
        .split('T')
        .next()
        .unwrap_or(&comment.created_at);

    let mut header_spans = vec![
        Span::styled(
            format!("@{}", comment.author.login),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(date.to_string(), Style::default().fg(Color::DarkGray)),
    ];

    if !comment.author_association.is_empty() && comment.author_association != "NONE" {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            comment.author_association.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }

    let header = Paragraph::new(Line::from(header_spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Comment Detail"),
    );
    frame.render_widget(header, chunks[0]);

    // Content with scroll
    let content_height = chunks[1].height.saturating_sub(2) as usize;
    let body_lines: Vec<Line> = comment
        .body
        .lines()
        .skip(state.issue_comment_detail_scroll)
        .take(content_height)
        .map(|line| Line::from(line.to_string()))
        .collect();

    let total_lines = comment.body.lines().count();
    let scroll_info = if total_lines > content_height {
        format!(
            " ({}/{})",
            state.issue_comment_detail_scroll + 1,
            total_lines.saturating_sub(content_height) + 1
        )
    } else {
        String::new()
    };

    let content = Paragraph::new(body_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Content{}", scroll_info)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(content, chunks[1]);

    // Footer
    let footer_text = "j/k: scroll | J/K: page | Ctrl+d/u: half page | Enter/Esc: back to list";
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(footer, chunks[2]);
}
