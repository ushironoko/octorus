use super::common::{render_rally_status_bar, wrap_text};
use crate::app::{App, CommentTab};
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

pub fn render(frame: &mut Frame, app: &mut App) {
    if app.is_local_mode() {
        render_local_comment_list(frame, app);
        return;
    }

    if app.cmt.discussion_comment_detail_mode {
        render_discussion_detail(frame, app);
        return;
    }

    let has_rally = app.has_background_rally();
    let constraints = if has_rally {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(3),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    render_tab_header(frame, app, chunks[0]);

    match app.cmt.comment_tab {
        CommentTab::Review => render_review_comments(frame, app, chunks[1]),
        CommentTab::Discussion => render_discussion_comments(frame, app, chunks[1]),
    }

    if has_rally {
        render_rally_status_bar(frame, chunks[2], app);
    }

    let footer_chunk_idx = if has_rally { 3 } else { 2 };
    let help_text = super::footer::footer_hint_back(&app.config.keybindings);
    let footer = Paragraph::new(help_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[footer_chunk_idx]);
}

fn render_local_comment_list(frame: &mut Frame, app: &mut App) {
    let has_rally = app.has_background_rally();
    let constraints = if has_rally {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(3),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let count = app.cmt.review_comments.as_ref().map(|c| c.len()).unwrap_or(0);
    let loading = if app.cmt.comments_loading {
        format!(" {}", app.spinner_char())
    } else {
        String::new()
    };
    let header = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("[Local Comments ({})]{}", count, loading),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    render_review_comments(frame, app, chunks[1]);

    if has_rally {
        render_rally_status_bar(frame, chunks[2], app);
    }

    let footer_chunk_idx = if has_rally { 3 } else { 2 };
    let help_text = super::footer::footer_hint_back(&app.config.keybindings);
    let footer_line = super::footer::build_footer_line(app, &help_text);
    let footer =
        Paragraph::new(footer_line).block(super::footer::build_footer_block(app));
    frame.render_widget(footer, chunks[footer_chunk_idx]);
}

/// Generic comment list renderer.
///
/// Renders a list of comments with a loading/empty state, scrollbar, and stateful selection.
///
/// - `comments`: The list of comments to render (if loaded).
/// - `loading`: Whether comments are currently loading.
/// - `selected_index`: The index of the selected comment.
/// - `scroll_offset`: Mutable reference to the scroll offset (updated after render).
/// - `label`: Label for the comment type (e.g., "review comments", "discussion comments").
/// - `format_item`: Closure to format each comment into a `ListItem`.
#[allow(clippy::too_many_arguments)]
fn render_comment_list_generic<T, F>(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    comments: Option<&[T]>,
    loading: bool,
    selected_index: usize,
    scroll_offset: &mut usize,
    label: &str,
    format_item: F,
) where
    F: Fn(&T, usize, bool, usize) -> ListItem<'static>,
{
    if loading && comments.is_none() {
        let loading_msg = Paragraph::new(format!("Loading {}...", label))
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(loading_msg, area);
        return;
    }

    let Some(items_data) = comments else {
        let empty = Paragraph::new(format!("No {}", label))
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    };

    if items_data.is_empty() {
        let empty = Paragraph::new(format!("No {} found", label))
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    }

    let available_width = area.width.saturating_sub(4) as usize;
    let body_width = available_width.saturating_sub(4);

    let items: Vec<ListItem> = items_data
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let is_selected = i == selected_index;
            format_item(item, i, is_selected, body_width)
        })
        .collect();

    let mut list_state = ListState::default()
        .with_offset(*scroll_offset)
        .with_selected(Some(selected_index));

    let block = Block::default().borders(Borders::ALL);
    let total_items = items_data.len();

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, area, &mut list_state);

    *scroll_offset = list_state.offset();

    if total_items > 1 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state =
            ScrollbarState::new(total_items.saturating_sub(1)).position(selected_index);

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

fn render_tab_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let review_count = app.cmt.review_comments.as_ref().map(|c| c.len()).unwrap_or(0);
    let discussion_count = app
        .cmt.discussion_comments
        .as_ref()
        .map(|c| c.len())
        .unwrap_or(0);

    let review_style = if app.cmt.comment_tab == CommentTab::Review {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let discussion_style = if app.cmt.comment_tab == CommentTab::Discussion {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let loading_indicator = |loading: bool| -> String {
        if loading {
            format!(" {}", app.spinner_char())
        } else {
            String::new()
        }
    };

    let header_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!(
                "[Review Comments ({})]{}",
                review_count,
                loading_indicator(app.cmt.comments_loading)
            ),
            review_style,
        ),
        Span::raw("  "),
        Span::styled(
            format!(
                "[Discussion ({})]{}",
                discussion_count,
                loading_indicator(app.cmt.discussion_comments_loading)
            ),
            discussion_style,
        ),
    ]);

    let header =
        Paragraph::new(header_line).block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, area);
}

fn render_review_comments(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    use crate::github::comment::ReviewComment;

    render_comment_list_generic(
        frame,
        area,
        app.cmt.review_comments.as_deref(),
        app.cmt.comments_loading,
        app.cmt.selected_comment,
        &mut app.cmt.comment_list_scroll_offset,
        "review comments",
        |comment: &ReviewComment, _i: usize, is_selected: bool, body_width: usize| {
            let prefix = if is_selected { "> " } else { "  " };
            let line_info = comment.line.map(|l| format!(":{}", l)).unwrap_or_default();
            let header_line = Line::from(vec![
                Span::raw(prefix),
                Span::styled(
                    format!("@{}", comment.user.login),
                    Style::default().fg(Color::Cyan),
                ),
                if comment.is_resolved {
                    Span::raw(" ")
                } else {
                    Span::raw("")
                },
                if comment.is_resolved {
                    Span::styled("[resolved]", Style::default().fg(Color::DarkGray))
                } else {
                    Span::raw("")
                },
                Span::raw(" on "),
                Span::styled(
                    format!("{}{}", comment.path, line_info),
                    Style::default().fg(Color::Green),
                ),
            ]);

            let mut lines = vec![header_line];
            for body_line in comment.body.lines() {
                let wrapped = wrap_text(body_line, body_width);
                for wrapped_line in wrapped {
                    lines.push(Line::from(vec![Span::raw("    "), Span::raw(wrapped_line)]));
                }
            }
            lines.push(Line::from(""));

            ListItem::new(lines)
        },
    );
}

fn render_discussion_comments(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    use crate::github::comment::DiscussionComment;

    render_comment_list_generic(
        frame,
        area,
        app.cmt.discussion_comments.as_deref(),
        app.cmt.discussion_comments_loading,
        app.cmt.selected_discussion_comment,
        &mut app.cmt.discussion_comment_list_scroll_offset,
        "discussion comments",
        |comment: &DiscussionComment, _i: usize, is_selected: bool, body_width: usize| {
            let prefix = if is_selected { "> " } else { "  " };

            let date = comment
                .created_at
                .split('T')
                .next()
                .unwrap_or(&comment.created_at);

            let header_line = Line::from(vec![
                Span::raw(prefix),
                Span::styled(
                    format!("@{}", comment.user.login),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw("  "),
                Span::styled(date.to_string(), Style::default().fg(Color::DarkGray)),
            ]);

            let mut lines = vec![header_line];
            let mut line_count = 0;
            let max_preview_lines = 3;
            'outer: for body_line in comment.body.lines() {
                let wrapped = wrap_text(body_line, body_width);
                for wrapped_line in wrapped {
                    if line_count >= max_preview_lines {
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled("...", Style::default().fg(Color::DarkGray)),
                        ]));
                        break 'outer;
                    }
                    lines.push(Line::from(vec![Span::raw("    "), Span::raw(wrapped_line)]));
                    line_count += 1;
                }
            }
            lines.push(Line::from(""));

            ListItem::new(lines)
        },
    );
}

fn render_discussion_detail(frame: &mut Frame, app: &App) {
    let Some(ref comments) = app.cmt.discussion_comments else {
        return;
    };
    let Some(comment) = comments.get(app.cmt.selected_discussion_comment) else {
        return;
    };

    let has_rally = app.has_background_rally();
    let constraints = if has_rally {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(3),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let date = comment
        .created_at
        .split('T')
        .next()
        .unwrap_or(&comment.created_at);
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("@{}", comment.user.login),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(date.to_string(), Style::default().fg(Color::DarkGray)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Comment Detail"),
    );
    frame.render_widget(header, chunks[0]);

    let content_height = chunks[1].height.saturating_sub(2) as usize;
    let body_lines: Vec<Line> = comment
        .body
        .lines()
        .skip(app.cmt.discussion_comment_detail_scroll)
        .take(content_height)
        .map(|line| Line::from(line.to_string()))
        .collect();

    let total_lines = comment.body.lines().count();
    let scroll_info = if total_lines > content_height {
        format!(
            " ({}/{})",
            app.cmt.discussion_comment_detail_scroll + 1,
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

    if has_rally {
        render_rally_status_bar(frame, chunks[2], app);
    }

    let footer_chunk_idx = if has_rally { 3 } else { 2 };
    let footer = Paragraph::new("j/k/↑↓: scroll | Ctrl+d/u: page | Enter/Esc: back to list")
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[footer_chunk_idx]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::github::comment::{DiscussionComment, ReviewComment};
    use crate::github::User;
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
    fn test_no_review_comments() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = None;

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Comments (0)]  [Discussion (0)]                                                          │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │No review comments                                                                                │
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
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_empty_review_comments() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![]);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Comments (0)]  [Discussion (0)]                                                          │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │No review comments found                                                                          │
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
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_with_review_comments() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![
            ReviewComment {
                id: 1,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "This looks good.".to_string(),
                user: User { login: "reviewer1".to_string() },
                created_at: "2025-01-01T00:00:00Z".to_string(),
                is_resolved: false,
                resolved_at: None,
            },
        ]);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Comments (1)]  [Discussion (0)]                                                          │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │> @reviewer1 on src/main.rs:10                                                                    │
        │    This looks good.                                                                              │
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
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_no_discussion_comments() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Discussion;
        app.cmt.discussion_comments = None;

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Comments (0)]  [Discussion (0)]                                                          │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │No discussion comments                                                                            │
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
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_with_discussion_comments() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Discussion;
        app.cmt.discussion_comments = Some(vec![
            DiscussionComment {
                id: 100,
                body: "Thanks for the fix!".to_string(),
                user: User { login: "commenter".to_string() },
                created_at: "2025-03-01T12:00:00Z".to_string(),
            },
        ]);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Comments (0)]  [Discussion (1)]                                                          │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │> @commenter  2025-03-01                                                                          │
        │    Thanks for the fix!                                                                           │
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
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_local_comment_list_rendering() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.set_local_mode(true);
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![
            ReviewComment {
                id: 1,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "Fix this variable naming".to_string(),
                user: User { login: "dacuna".to_string() },
                created_at: "2026-03-25T02:00:00+00:00".to_string(),
                is_resolved: false,
                resolved_at: None,
            },
            ReviewComment {
                id: 2,
                path: "src/lib.rs".to_string(),
                line: Some(42),
                start_line: None,
                body: "Consider error handling".to_string(),
                user: User { login: "dacuna".to_string() },
                created_at: "2026-03-25T03:00:00+00:00".to_string(),
                is_resolved: true,
                resolved_at: Some("2026-03-25T04:00:00+00:00".to_string()),
            },
        ]);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Local Comments (2)]                                                                             │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │> @dacuna on src/main.rs:10                                                                       ▲
        │    Fix this variable naming                                                                      █
        │                                                                                                  █
        │  @dacuna [resolved] on src/lib.rs:42                                                             █
        │    Consider error handling                                                                       █
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
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_local_comment_list_empty() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.set_local_mode(true);
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![]);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Local Comments (0)]                                                                             │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │No review comments found                                                                          │
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
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }
}
