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
    let footer_line = if app.is_issue_comment_submitting() {
        Line::from(Span::styled(
            " Submitting...",
            Style::default().fg(Color::Yellow),
        ))
    } else if let Some((success, ref message)) = app.submission_result {
        let (icon, color) = if success {
            ("\u{2713}", Color::Green)
        } else {
            ("\u{2717}", Color::Red)
        };
        Line::from(Span::styled(
            format!(" {} {}", icon, message),
            Style::default().fg(color),
        ))
    } else {
        let kb = &app.config.keybindings;
        Line::from(Span::styled(
            format!(
                " {}/Esc: back | j/k: move | Enter: detail | {}: comment | {}: browser",
                kb.quit.display(),
                kb.comment.display(),
                kb.open_in_browser.display(),
            ),
            Style::default().fg(Color::DarkGray),
        ))
    };
    let footer = Paragraph::new(footer_line);
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

    // Truncate body for list view (char-boundary-safe)
    let body_text: String = comment.body.lines().collect::<Vec<_>>().join(" ");
    let max_chars = body_width * 2;
    let truncated = if body_text.chars().count() > max_chars {
        let s: String = body_text.chars().take(max_chars).collect();
        format!("{}...", s)
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

fn render_detail(frame: &mut Frame, app: &mut App) {
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

    // Content with scroll — wrap first, then paginate on wrapped rows
    let content_height = chunks[1].height.saturating_sub(2) as usize;
    let content_width = chunks[1].width.saturating_sub(2) as usize;

    // Pre-wrap each raw line to the available width, then flatten into wrapped rows
    let wrapped_rows: Vec<String> = comment
        .body
        .lines()
        .flat_map(|line| {
            if line.is_empty() {
                vec![String::new()]
            } else {
                wrap_text(line, content_width)
            }
        })
        .collect();

    let total_rows = wrapped_rows.len();

    // Clamp scroll so it can't overshoot
    let state = app.issue_state.as_mut().unwrap();
    let max_scroll = total_rows.saturating_sub(content_height);
    if state.issue_comment_detail_scroll > max_scroll {
        state.issue_comment_detail_scroll = max_scroll;
    }
    let scroll = state.issue_comment_detail_scroll;

    let body_lines: Vec<Line> = wrapped_rows
        .into_iter()
        .skip(scroll)
        .take(content_height)
        .map(|row| Line::from(row))
        .collect();

    let scroll_info = if total_rows > content_height {
        format!(" ({}/{})", scroll + 1, max_scroll + 1)
    } else {
        String::new()
    };

    let content = Paragraph::new(body_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Content{}", scroll_info)),
    );
    frame.render_widget(content, chunks[1]);

    // Footer
    let kb = &app.config.keybindings;
    let footer_text = format!(
        "j/k: scroll | J/K: page | {}: reply | Enter/Esc: back to list",
        kb.reply.display(),
    );
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(footer, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::User;

    #[test]
    fn test_format_comment_item_with_japanese_text_does_not_panic() {
        // body_width=10 → max_chars=20, but the text is 30+ multibyte chars
        // This would panic with byte-based slicing: &body_text[..20] on UTF-8
        let comment = IssueComment {
            id: "IC_1".to_string(),
            body: "これはテストです。日本語のコメントが正しく切り詰められるか確認します。"
                .to_string(),
            author: User {
                login: "user1".to_string(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            author_association: "OWNER".to_string(),
            url: String::new(),
        };

        // Should not panic
        let _ = format_comment_item(&comment, 0, false, 10);
    }

    #[test]
    fn test_format_comment_item_truncates_long_body() {
        let comment = IssueComment {
            id: "IC_1".to_string(),
            body: "a".repeat(100),
            author: User {
                login: "user1".to_string(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            author_association: "NONE".to_string(),
            url: String::new(),
        };

        // body_width=10 → max_chars=20, body has 100 chars → should truncate
        let item = format_comment_item(&comment, 0, false, 10);
        // Verify it doesn't panic and produces output
        let lines = item.height();
        assert!(lines > 0);
    }

    #[test]
    fn test_format_comment_item_mixed_ascii_multibyte() {
        let comment = IssueComment {
            id: "IC_1".to_string(),
            body: "Hello世界こんにちはRust言語".to_string(),
            author: User {
                login: "user1".to_string(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            author_association: "NONE".to_string(),
            url: String::new(),
        };

        // body_width=5 → max_chars=10, mixed content
        let _ = format_comment_item(&comment, 0, false, 5);
    }
}
