use super::common::{render_rally_status_bar, wrap_text};
use crate::app::{App, CommentTab};
use crate::ui::hit::{HitMap, HitTarget, ListKind, PaneKind, TabGroup};
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};
use std::collections::HashSet;

fn register_comment_list_hits(
    hit_map: &mut HitMap,
    area: Rect,
    item_heights: &[usize],
    offset: usize,
) {
    let inner = Block::default().borders(Borders::ALL).inner(area);
    hit_map.push(
        inner,
        HitTarget::Pane {
            pane: PaneKind::List(ListKind::CommentList),
        },
    );

    let bottom = inner.y.saturating_add(inner.height);
    let mut y = inner.y;
    for (row, &item_height) in item_heights.iter().enumerate().skip(offset) {
        if y >= bottom {
            break;
        }

        let remaining_height = usize::from(bottom - y);
        if item_height > remaining_height {
            break;
        }
        let height = item_height as u16;
        hit_map.push(
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height,
            },
            HitTarget::ListRow {
                list: ListKind::CommentList,
                row,
                index: row,
            },
        );
        y = y.saturating_add(height);
    }
}

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
    register_comment_tab_hits(app, chunks[0]);

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

    let count = app
        .cmt
        .review_comments
        .as_ref()
        .map(|c| c.len())
        .unwrap_or(0);
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
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block(app));
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
    area: Rect,
    comments: Option<&[T]>,
    loading: bool,
    selected_index: usize,
    scroll_offset: &mut usize,
    hit_map: &mut HitMap,
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
        register_comment_list_hits(hit_map, area, &[], 0);
        return;
    }

    let Some(items_data) = comments else {
        let empty = Paragraph::new(format!("No {}", label))
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(empty, area);
        register_comment_list_hits(hit_map, area, &[], 0);
        return;
    };

    if items_data.is_empty() {
        let empty = Paragraph::new(format!("No {} found", label))
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(empty, area);
        register_comment_list_hits(hit_map, area, &[], 0);
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
    let item_heights: Vec<usize> = items.iter().map(ListItem::height).collect();

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
    register_comment_list_hits(hit_map, area, &item_heights, *scroll_offset);

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

/// タブ見出しのクリック領域。ヘッダ行のラベル描画と同じ幅計算で位置を出す。
fn register_comment_tab_hits(app: &mut App, area: ratatui::layout::Rect) {
    use unicode_width::UnicodeWidthStr;
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let review_count = app.cmt.review_threads.len();
    let discussion_count = app
        .cmt
        .discussion_comments
        .as_ref()
        .map(|c| c.len())
        .unwrap_or(0);
    let review_label = format!("[Review Threads ({})]", review_count);
    let discussion_label = format!("[Discussion ({})]", discussion_count);
    let review_width = review_label.width() as u16;
    let regions = [
        (inner.x.saturating_add(1), review_width, 0u16),
        (
            inner.x.saturating_add(1 + review_width + 2),
            discussion_label.width() as u16,
            1,
        ),
    ];
    for (x, width, index) in regions {
        app.hit_map.push(
            ratatui::layout::Rect {
                x,
                y: inner.y,
                width,
                height: 1,
            },
            HitTarget::Tab {
                group: TabGroup::CommentTabs,
                index: usize::from(index),
            },
        );
    }
}

fn render_tab_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let review_count = app.cmt.review_threads.len();
    let discussion_count = app
        .cmt
        .discussion_comments
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
                "[Review Threads ({})]{}",
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

fn outdated_left_ids(app: &App) -> HashSet<u64> {
    let Some(comments) = app.cmt.review_comments.as_deref() else {
        return HashSet::new();
    };
    let files = app.files();
    comments
        .iter()
        .filter(|comment| App::left_review_comment_is_outdated(comment, files))
        .map(|comment| comment.id)
        .collect()
}

fn render_review_comments(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let outdated_ids = outdated_left_ids(app);
    if app.cmt.expanded_thread.is_some() {
        render_expanded_thread(frame, app, area, &outdated_ids);
        return;
    }

    use std::collections::HashSet;

    // Resolved-state badges only apply in local mode; ignore any stale meta
    // that might persist from a prior mode switch.
    let resolved_ids: HashSet<u64> = if app.is_local_mode() {
        app.cmt
            .local_comment_meta
            .iter()
            .filter(|(_, meta)| meta.is_resolved)
            .map(|(id, _)| *id)
            .collect()
    } else {
        HashSet::new()
    };

    let threads = &app.cmt.review_threads;
    let comments = app.cmt.review_comments.as_deref();

    if app.cmt.comments_loading && comments.is_none() {
        let loading_msg = Paragraph::new("Loading review comments...")
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(loading_msg, area);
        return;
    }

    if threads.is_empty() {
        let empty = Paragraph::new("No review comments found")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    }

    let Some(ref all_comments) = app.cmt.review_comments else {
        return;
    };

    render_review_thread_list_data_with_hits(
        frame,
        area,
        ReviewThreadListData {
            comments: all_comments,
            threads,
            visible_threads: None,
            selected: app.cmt.selected_thread,
            resolved_ids: &resolved_ids,
            outdated_ids: &outdated_ids,
        },
        &mut app.cmt.thread_scroll_offset,
        Some(&mut app.hit_map),
    );
}

pub(crate) struct ReviewThreadListData<'a> {
    pub comments: &'a [crate::github::comment::ReviewComment],
    pub threads: &'a [crate::app::CommentThread],
    pub visible_threads: Option<&'a [usize]>,
    pub selected: usize,
    pub resolved_ids: &'a std::collections::HashSet<u64>,
    pub outdated_ids: &'a HashSet<u64>,
}

pub(crate) fn render_review_thread_list_data(
    frame: &mut Frame,
    area: Rect,
    data: ReviewThreadListData<'_>,
    scroll_offset: &mut usize,
) {
    render_review_thread_list_data_with_hits(frame, area, data, scroll_offset, None);
}

fn render_review_thread_list_data_with_hits(
    frame: &mut Frame,
    area: Rect,
    data: ReviewThreadListData<'_>,
    scroll_offset: &mut usize,
    hit_map: Option<&mut HitMap>,
) {
    let available_width = area.width.saturating_sub(4) as usize;
    let body_width = available_width.saturating_sub(4);
    let total_items = data
        .visible_threads
        .map_or(data.threads.len(), <[usize]>::len);

    let items: Vec<ListItem> = (0..total_items)
        .filter_map(|i| {
            let thread_index = data.visible_threads.map_or(i, |indices| indices[i]);
            let thread = data.threads.get(thread_index)?;
            let is_selected = i == data.selected;
            let comment = &data.comments[thread.root];
            let prefix = if is_selected { "> " } else { "  " };
            let line_info = comment.line.map(|l| format!(":{}", l)).unwrap_or_default();
            let resolved = data.resolved_ids.contains(&comment.id);

            let reply_count = thread.replies.len();
            let reply_info = if reply_count > 0 {
                format!(
                    "  ({} {})",
                    reply_count,
                    if reply_count == 1 { "reply" } else { "replies" }
                )
            } else {
                String::new()
            };

            let header_line = Line::from(vec![
                Span::raw(prefix),
                Span::styled(
                    format!("@{}", comment.user.login),
                    Style::default().fg(Color::Cyan),
                ),
                if resolved {
                    Span::raw(" ")
                } else {
                    Span::raw("")
                },
                if resolved {
                    Span::styled("[resolved]", Style::default().fg(Color::DarkGray))
                } else {
                    Span::raw("")
                },
                Span::raw(" on "),
                Span::styled(
                    format!("{}{}", comment.path, line_info),
                    Style::default().fg(Color::Green),
                ),
                if data.outdated_ids.contains(&comment.id) {
                    Span::styled(" outdated", Style::default().fg(Color::DarkGray))
                } else {
                    Span::raw("")
                },
                Span::styled(reply_info, Style::default().fg(Color::DarkGray)),
            ]);

            let mut lines = vec![header_line];
            for body_line in comment.body.lines() {
                let wrapped = wrap_text(body_line, body_width);
                for wrapped_line in wrapped {
                    lines.push(Line::from(vec![Span::raw("    "), Span::raw(wrapped_line)]));
                }
            }
            lines.push(Line::from(""));

            Some(ListItem::new(lines))
        })
        .collect();
    let item_heights: Vec<usize> = items.iter().map(ListItem::height).collect();

    let mut list_state = ListState::default()
        .with_offset(*scroll_offset)
        .with_selected(Some(data.selected));

    let block = Block::default().borders(Borders::ALL);
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, area, &mut list_state);

    *scroll_offset = list_state.offset();
    if let Some(hit_map) = hit_map {
        register_comment_list_hits(hit_map, area, &item_heights, *scroll_offset);
    }

    if total_items > 1 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state =
            ScrollbarState::new(total_items.saturating_sub(1)).position(data.selected);

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

fn render_expanded_thread(
    frame: &mut Frame,
    app: &mut App,
    area: ratatui::layout::Rect,
    outdated_ids: &HashSet<u64>,
) {
    use std::collections::HashSet;

    let Some(thread_idx) = app.cmt.expanded_thread else {
        return;
    };
    let Some(thread) = app.cmt.review_threads.get(thread_idx) else {
        return;
    };
    let Some(ref all_comments) = app.cmt.review_comments else {
        return;
    };

    // Mirror the collapsed view: only display resolved-state badges in
    // local mode, and key them on each comment's id (so a resolved reply
    // is also flagged in the expanded conversation).
    let resolved_ids: HashSet<u64> = if app.is_local_mode() {
        app.cmt
            .local_comment_meta
            .iter()
            .filter(|(_, meta)| meta.is_resolved)
            .map(|(id, _)| *id)
            .collect()
    } else {
        HashSet::new()
    };

    render_review_thread_data_with_hits(
        frame,
        area,
        all_comments,
        thread,
        app.cmt.expanded_selected,
        &mut app.cmt.expanded_scroll_offset,
        &resolved_ids,
        outdated_ids,
        Some(&mut app.hit_map),
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_review_thread_data(
    frame: &mut Frame,
    area: Rect,
    all_comments: &[crate::github::comment::ReviewComment],
    thread: &crate::app::CommentThread,
    selected: usize,
    scroll_offset: &mut usize,
    resolved_ids: &std::collections::HashSet<u64>,
    outdated_ids: &HashSet<u64>,
) {
    render_review_thread_data_with_hits(
        frame,
        area,
        all_comments,
        thread,
        selected,
        scroll_offset,
        resolved_ids,
        outdated_ids,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_review_thread_data_with_hits(
    frame: &mut Frame,
    area: Rect,
    all_comments: &[crate::github::comment::ReviewComment],
    thread: &crate::app::CommentThread,
    selected: usize,
    scroll_offset: &mut usize,
    resolved_ids: &std::collections::HashSet<u64>,
    outdated_ids: &HashSet<u64>,
    hit_map: Option<&mut HitMap>,
) {
    let available_width = area.width.saturating_sub(4) as usize;
    let body_width = available_width.saturating_sub(6);

    // Build flat list: index 0 = root, 1..=N = replies
    let comment_indices: Vec<usize> = std::iter::once(thread.root)
        .chain(thread.replies.iter().copied())
        .collect();

    let items: Vec<ListItem> = comment_indices
        .iter()
        .enumerate()
        .map(|(i, &ci)| {
            let comment = &all_comments[ci];
            let is_selected = i == selected;
            let prefix = if is_selected { "> " } else { "  " };
            let is_root = i == 0;
            let resolved = resolved_ids.contains(&comment.id);
            let resolved_badge = if resolved {
                Span::styled(" [resolved]", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            };

            let header_line = if is_root {
                let line_info = comment.line.map(|l| format!(":{}", l)).unwrap_or_default();
                Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(
                        format!("@{}", comment.user.login),
                        Style::default().fg(Color::Cyan),
                    ),
                    resolved_badge,
                    Span::raw(" on "),
                    Span::styled(
                        format!("{}{}", comment.path, line_info),
                        Style::default().fg(Color::Green),
                    ),
                    if outdated_ids.contains(&comment.id) {
                        Span::styled(" outdated", Style::default().fg(Color::DarkGray))
                    } else {
                        Span::raw("")
                    },
                ])
            } else {
                let date = comment
                    .created_at
                    .split('T')
                    .next()
                    .unwrap_or(&comment.created_at);
                Line::from(vec![
                    Span::raw(prefix),
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        format!("@{}", comment.user.login),
                        Style::default().fg(Color::Cyan),
                    ),
                    resolved_badge,
                    Span::raw("  "),
                    Span::styled(date.to_string(), Style::default().fg(Color::DarkGray)),
                ])
            };

            let indent = if is_root { "    " } else { "      " };
            let mut lines = vec![header_line];
            for body_line in comment.body.lines() {
                let wrapped = wrap_text(body_line, body_width);
                for wrapped_line in wrapped {
                    lines.push(Line::from(vec![Span::raw(indent), Span::raw(wrapped_line)]));
                }
            }
            lines.push(Line::from(""));

            ListItem::new(lines)
        })
        .collect();
    let item_heights: Vec<usize> = items.iter().map(ListItem::height).collect();

    let total_items = comment_indices.len();
    let root_comment = &all_comments[thread.root];
    let line_info = root_comment
        .line
        .map(|l| format!(":{}", l))
        .unwrap_or_default();
    let title = format!(
        "Thread: {}{} ({} comments)",
        root_comment.path, line_info, total_items
    );

    let mut list_state = ListState::default()
        .with_offset(*scroll_offset)
        .with_selected(Some(selected));

    let block = Block::default().borders(Borders::ALL).title(title);
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, area, &mut list_state);

    *scroll_offset = list_state.offset();
    if let Some(hit_map) = hit_map {
        register_comment_list_hits(hit_map, area, &item_heights, *scroll_offset);
    }

    if total_items > 1 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state =
            ScrollbarState::new(total_items.saturating_sub(1)).position(selected);

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

fn render_discussion_comments(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    use crate::github::comment::DiscussionComment;

    render_comment_list_generic(
        frame,
        area,
        app.cmt.discussion_comments.as_deref(),
        app.cmt.discussion_comments_loading,
        app.cmt.selected_discussion_comment,
        &mut app.cmt.discussion_comment_list_scroll_offset,
        &mut app.hit_map,
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
        │ [Review Threads (0)]  [Discussion (0)]                                                           │
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
    fn test_empty_review_comments() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![]);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Threads (0)]  [Discussion (0)]                                                           │
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
        app.cmt.review_comments = Some(vec![ReviewComment {
            id: 1,
            path: "src/main.rs".to_string(),
            line: Some(10),
            start_line: None,
            body: "This looks good.".to_string(),
            user: User {
                login: "reviewer1".to_string(),
            },
            created_at: "2025-01-01T00:00:00Z".to_string(),
            in_reply_to_id: None,
            location: Default::default(),
        }]);
        app.build_review_threads();

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Threads (1)]  [Discussion (0)]                                                           │
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
        │ [Review Threads (0)]  [Discussion (0)]                                                           │
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
        app.cmt.discussion_comments = Some(vec![DiscussionComment {
            id: 100,
            body: "Thanks for the fix!".to_string(),
            user: User {
                login: "commenter".to_string(),
            },
            created_at: "2025-03-01T12:00:00Z".to_string(),
        }]);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Threads (0)]  [Discussion (1)]                                                           │
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
                user: User {
                    login: "dacuna".to_string(),
                },
                created_at: "2026-03-25T02:00:00+00:00".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
            ReviewComment {
                id: 2,
                path: "src/lib.rs".to_string(),
                line: Some(42),
                start_line: None,
                body: "Consider error handling".to_string(),
                user: User {
                    login: "dacuna".to_string(),
                },
                created_at: "2026-03-25T03:00:00+00:00".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
        ]);
        app.cmt.local_comment_meta.insert(
            2,
            crate::cache::LocalCommentMeta {
                is_resolved: true,
                resolved_at: Some("2026-03-25T04:00:00+00:00".to_string()),
            },
        );
        app.build_review_threads();

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
        app.build_review_threads();

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

    #[test]
    fn test_build_review_threads_groups_replies() {
        let mut app = App::new_for_test();
        app.cmt.review_comments = Some(vec![
            ReviewComment {
                id: 100,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "Root comment".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2025-01-01T00:00:00Z".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
            ReviewComment {
                id: 101,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "First reply".to_string(),
                user: User {
                    login: "bob".to_string(),
                },
                created_at: "2025-01-01T01:00:00Z".to_string(),
                in_reply_to_id: Some(100),
                location: Default::default(),
            },
            ReviewComment {
                id: 102,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "Second reply".to_string(),
                user: User {
                    login: "carol".to_string(),
                },
                created_at: "2025-01-01T02:00:00Z".to_string(),
                in_reply_to_id: Some(100),
                location: Default::default(),
            },
            ReviewComment {
                id: 200,
                path: "src/lib.rs".to_string(),
                line: Some(5),
                start_line: None,
                body: "Independent thread".to_string(),
                user: User {
                    login: "dave".to_string(),
                },
                created_at: "2025-01-01T03:00:00Z".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
        ]);
        app.build_review_threads();

        assert_eq!(app.cmt.review_threads.len(), 2);

        let t0 = &app.cmt.review_threads[0];
        assert_eq!(t0.root, 0);
        assert_eq!(t0.replies, vec![1, 2]);

        let t1 = &app.cmt.review_threads[1];
        assert_eq!(t1.root, 3);
        assert!(t1.replies.is_empty());
    }

    #[test]
    fn test_threaded_review_comments_rendering() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![
            ReviewComment {
                id: 100,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "Needs refactoring".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2025-01-01T00:00:00Z".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
            ReviewComment {
                id: 101,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "Agreed".to_string(),
                user: User {
                    login: "bob".to_string(),
                },
                created_at: "2025-01-01T01:00:00Z".to_string(),
                in_reply_to_id: Some(100),
                location: Default::default(),
            },
        ]);
        app.build_review_threads();

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Threads (1)]  [Discussion (0)]                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │> @alice on src/main.rs:10  (1 reply)                                                             │
        │    Needs refactoring                                                                             │
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
    fn test_expanded_thread_rendering() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![
            ReviewComment {
                id: 100,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "Needs refactoring".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2025-01-01T00:00:00Z".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
            ReviewComment {
                id: 101,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "Agreed, will fix".to_string(),
                user: User {
                    login: "bob".to_string(),
                },
                created_at: "2025-01-02T00:00:00Z".to_string(),
                in_reply_to_id: Some(100),
                location: Default::default(),
            },
            ReviewComment {
                id: 102,
                path: "src/main.rs".to_string(),
                line: Some(10),
                start_line: None,
                body: "Done in latest push".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2025-01-03T00:00:00Z".to_string(),
                in_reply_to_id: Some(100),
                location: Default::default(),
            },
        ]);
        app.build_review_threads();
        app.cmt.expanded_thread = Some(0);
        app.cmt.expanded_selected = 0;

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │ [Review Threads (1)]  [Discussion (0)]                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Thread: src/main.rs:10 (3 comments)───────────────────────────────────────────────────────────────┐
        │> @alice on src/main.rs:10                                                                        ▲
        │    Needs refactoring                                                                             █
        │                                                                                                  █
        │    @bob  2025-01-02                                                                              █
        │      Agreed, will fix                                                                            █
        │                                                                                                  █
        │    @alice  2025-01-03                                                                            █
        │      Done in latest push                                                                         █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  █
        │                                                                                                  ║
        │                                                                                                  ▼
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    /// In local mode, the expanded conversation view must surface the
    /// `[resolved]` badge on each comment whose meta is resolved — both
    /// roots and replies. The collapsed view only shows the root, so
    /// without this the user has no way to see that an individual reply
    /// has been resolved.
    #[test]
    fn test_expanded_thread_renders_resolved_on_root_and_reply() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.set_local_mode(true);
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![
            ReviewComment {
                id: 400,
                path: "src/main.rs".to_string(),
                line: Some(40),
                start_line: None,
                body: "root".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2025-01-01T00:00:00Z".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
            ReviewComment {
                id: 401,
                path: "src/main.rs".to_string(),
                line: Some(40),
                start_line: None,
                body: "reply-resolved".to_string(),
                user: User {
                    login: "bob".to_string(),
                },
                created_at: "2025-01-01T01:00:00Z".to_string(),
                in_reply_to_id: Some(400),
                location: Default::default(),
            },
            ReviewComment {
                id: 402,
                path: "src/main.rs".to_string(),
                line: Some(40),
                start_line: None,
                body: "reply-open".to_string(),
                user: User {
                    login: "carol".to_string(),
                },
                created_at: "2025-01-01T02:00:00Z".to_string(),
                in_reply_to_id: Some(400),
                location: Default::default(),
            },
        ]);
        // Root resolved, first reply resolved, second reply open.
        for id in [400, 401] {
            app.cmt.local_comment_meta.insert(
                id,
                crate::cache::LocalCommentMeta {
                    is_resolved: true,
                    resolved_at: Some("2025-01-02T00:00:00Z".to_string()),
                },
            );
        }
        app.build_review_threads();
        app.cmt.expanded_thread = Some(0);
        app.cmt.expanded_selected = 0;

        let rendered = render_full(&mut app);
        assert!(
            rendered.contains("@alice [resolved] on src/main.rs:40"),
            "expanded root must show [resolved]:\n{rendered}"
        );
        assert!(
            rendered.contains("@bob [resolved]"),
            "expanded resolved reply must show [resolved]:\n{rendered}"
        );
        assert!(
            !rendered.contains("@carol [resolved]"),
            "open reply must not show [resolved]:\n{rendered}"
        );
    }

    /// A thread whose root is resolved should show `[resolved]` in the
    /// collapsed view; a thread whose only resolved comment is a *reply*
    /// must not — the badge is keyed on the root's id, not any reply's.
    #[test]
    fn test_threaded_review_resolved_keyed_on_root() {
        let mut app = App::new_for_test();
        app.state = crate::app::AppState::CommentList;
        app.set_local_mode(true);
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.review_comments = Some(vec![
            ReviewComment {
                id: 200,
                path: "src/main.rs".to_string(),
                line: Some(20),
                start_line: None,
                body: "Root resolved".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2025-01-01T00:00:00Z".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
            ReviewComment {
                id: 300,
                path: "src/main.rs".to_string(),
                line: Some(30),
                start_line: None,
                body: "Root unresolved".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2025-01-01T01:00:00Z".to_string(),
                in_reply_to_id: None,
                location: Default::default(),
            },
            ReviewComment {
                id: 301,
                path: "src/main.rs".to_string(),
                line: Some(30),
                start_line: None,
                body: "Reply resolved".to_string(),
                user: User {
                    login: "bob".to_string(),
                },
                created_at: "2025-01-01T02:00:00Z".to_string(),
                in_reply_to_id: Some(300),
                location: Default::default(),
            },
        ]);
        // Root of thread 1 is resolved; only the reply of thread 2 is resolved.
        app.cmt.local_comment_meta.insert(
            200,
            crate::cache::LocalCommentMeta {
                is_resolved: true,
                resolved_at: Some("2025-01-02T00:00:00Z".to_string()),
            },
        );
        app.cmt.local_comment_meta.insert(
            301,
            crate::cache::LocalCommentMeta {
                is_resolved: true,
                resolved_at: Some("2025-01-02T00:00:00Z".to_string()),
            },
        );
        app.build_review_threads();

        let rendered = render_full(&mut app);
        assert!(
            rendered.contains("@alice [resolved] on src/main.rs:20"),
            "thread with resolved root should display [resolved] badge:\n{rendered}"
        );
        assert!(
            !rendered.contains("@alice [resolved] on src/main.rs:30"),
            "thread whose only resolved comment is a reply must not display [resolved] on the root:\n{rendered}"
        );
    }
}
