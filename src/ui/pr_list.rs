use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};

use unicode_width::UnicodeWidthStr;

use super::common::{fit_to_width, render_update_bar};
use crate::app::App;
use crate::github::{CiStatus, PullRequestSummary};
use crate::ui::hit::{HitTarget, ListKind, PaneKind};

pub fn render(frame: &mut Frame, app: &mut App) {
    let has_filter_bar = app
        .prs
        .pr_list_filter
        .as_ref()
        .is_some_and(|f| f.input_active);
    let has_update = app.update_available.is_some();

    let mut constraints = vec![Constraint::Length(3), Constraint::Min(0)];
    if has_filter_bar {
        constraints.push(Constraint::Length(3));
    }
    if has_update {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(3));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let filter_str = app.prs.pr_list_state_filter.display_name();
    let header_text = format!("PR List: {} ({})", app.repo, filter_str);
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    if matches!(app.prs.pr_list, crate::app::LoadState::Loading) {
        let loading = Paragraph::new(format!("{} Loading PRs...", app.spinner_char())).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Pull Requests"),
        );
        frame.render_widget(loading, chunks[1]);
    } else if let Some(prs) = app.prs.pr_list.as_loaded() {
        if prs.is_empty() {
            let empty = Paragraph::new("No pull requests found").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Pull Requests"),
            );
            frame.render_widget(empty, chunks[1]);
        } else {
            let (display_prs, display_selected, total_display) =
                if let Some(ref filter) = app.prs.pr_list_filter {
                    if filter.matched_indices.is_empty() {
                        let empty_msg = format!("No matches for '{}'", filter.query);
                        let empty = Paragraph::new(empty_msg)
                            .style(Style::default().fg(Color::DarkGray))
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .title(format!("Pull Requests (0/{})", prs.len())),
                            );
                        frame.render_widget(empty, chunks[1]);

                        let mut next_chunk = 2;
                        if has_filter_bar {
                            super::common::render_filter_bar(frame, chunks[next_chunk], filter);
                            next_chunk += 1;
                        }

                        if has_update {
                            render_update_bar(frame, chunks[next_chunk], app);
                            next_chunk += 1;
                        }

                        render_footer(frame, chunks[next_chunk], app);
                        return;
                    }
                    let filtered: Vec<&PullRequestSummary> =
                        filter.matched_indices.iter().map(|&i| &prs[i]).collect();
                    let sel = filter.selected.unwrap_or(0);
                    let total = filtered.len();
                    (filtered, sel, total)
                } else {
                    let all: Vec<&PullRequestSummary> = prs.iter().collect();
                    let sel = app.prs.selected_pr;
                    let total = all.len();
                    (all, sel, total)
                };

            let total_prs = prs.len();
            let title = if let Some(ref filter) = app.prs.pr_list_filter {
                format!(
                    "Pull Requests ({}/{})",
                    filter.matched_indices.len(),
                    total_prs
                )
            } else if app.prs.pr_list.is_loading() {
                format!("Pull Requests ({}) {}", total_prs, app.spinner_char())
            } else if app.prs.pr_list_has_more {
                format!("Pull Requests ({}+)", total_prs)
            } else {
                format!("Pull Requests ({})", total_prs)
            };

            let inner_width = chunks[1].width.saturating_sub(3) as usize;
            let items = build_pr_list_items_ref(&display_prs, display_selected, inner_width);

            let mut list_state = ListState::default()
                .with_offset(app.prs.pr_list_scroll_offset)
                .with_selected(Some(display_selected));

            let block = Block::default().borders(Borders::ALL).title(title);
            let inner_area = block.inner(chunks[1]);
            let list = List::new(items)
                .block(block)
                .highlight_style(Style::default().bg(Color::DarkGray));
            frame.render_stateful_widget(list, chunks[1], &mut list_state);

            app.prs.pr_list_scroll_offset = list_state.offset();
            app.hit_map.push(
                inner_area,
                HitTarget::Pane {
                    pane: PaneKind::List(ListKind::PrList),
                },
            );
            for i in 0..inner_area.height as usize {
                let row = list_state.offset() + i;
                if row >= total_display {
                    break;
                }
                let index = app
                    .prs
                    .pr_list_filter
                    .as_ref()
                    .map_or(row, |filter| filter.matched_indices[row]);
                app.hit_map.push(
                    Rect {
                        x: inner_area.x,
                        y: inner_area.y.saturating_add(i as u16),
                        width: inner_area.width,
                        height: 1,
                    },
                    HitTarget::ListRow {
                        list: ListKind::PrList,
                        row,
                        index,
                    },
                );
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
        let empty = Paragraph::new("Failed to load pull requests").block(
            Block::default()
                .borders(Borders::ALL)
                .title("Pull Requests"),
        );
        frame.render_widget(empty, chunks[1]);
    }

    let mut next_chunk = 2;
    if has_filter_bar {
        if let Some(ref filter) = app.prs.pr_list_filter {
            super::common::render_filter_bar(frame, chunks[next_chunk], filter);
        }
        next_chunk += 1;
    }

    if has_update {
        render_update_bar(frame, chunks[next_chunk], app);
        next_chunk += 1;
    }

    render_footer(frame, chunks[next_chunk], app);
}

fn render_footer(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let help_text = super::footer::footer_hint_quit(&app.config.keybindings);
    let line = super::footer::build_footer_line(app, &help_text);
    let footer = Paragraph::new(line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

fn build_pr_list_items_ref(
    prs: &[&PullRequestSummary],
    selected: usize,
    area_width: usize,
) -> Vec<ListItem<'static>> {
    prs.iter()
        .enumerate()
        .map(|(i, pr)| {
            let is_selected = i == selected;

            let draft_marker = if pr.is_draft { "[DRAFT] " } else { "" };

            let number_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let number_span = Span::styled(format!("#{:<5}", pr.number), number_style);

            let author_width = 4 + pr.author.login.width();
            let fixed_width = 6 + 2 + 2 + author_width;
            let title_width = area_width.saturating_sub(fixed_width).max(20);
            let full_title = format!("{}{}", draft_marker, pr.title);
            let title_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if pr.is_draft {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let title_span = Span::styled(
                fit_to_width(&full_title, title_width).into_owned(),
                title_style,
            );

            let author_span = Span::styled(
                format!("by @{}", pr.author.login),
                Style::default().fg(Color::Cyan),
            );

            let labels_str = if !pr.labels.is_empty() {
                let label_names: Vec<&str> =
                    pr.labels.iter().take(2).map(|l| l.name.as_str()).collect();
                if pr.labels.len() > 2 {
                    format!(" [{}+{}]", label_names.join(", "), pr.labels.len() - 2)
                } else {
                    format!(" [{}]", label_names.join(", "))
                }
            } else {
                String::new()
            };
            let labels_span = Span::styled(labels_str, Style::default().fg(Color::Blue));

            let ci_status = CiStatus::from_rollup(&pr.status_check_rollup);
            let ci_span = match ci_status {
                CiStatus::Success => Span::styled("  ✓", Style::default().fg(Color::Green)),
                CiStatus::Failure => Span::styled("  ✕", Style::default().fg(Color::Red)),
                CiStatus::Pending => Span::styled("  ○", Style::default().fg(Color::Yellow)),
                CiStatus::None => Span::raw(""),
            };

            let line = Line::from(vec![
                number_span,
                Span::raw("  "),
                title_span,
                Span::raw("  "),
                author_span,
                labels_span,
                ci_span,
            ]);

            ListItem::new(line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_buffer::{find_column, row_text, screen_text};
    use super::*;
    use crate::app::{App, AppState, LoadState};
    use crate::github::{PullRequestSummary, User};
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn sample_pr(number: u32, title: &str, author: &str) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: title.to_string(),
            state: "open".to_string(),
            author: User {
                login: author.to_string(),
            },
            is_draft: false,
            labels: vec![],
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            status_check_rollup: vec![],
        }
    }

    fn render_rows(prs: &[PullRequestSummary], area_width: usize) -> Buffer {
        let refs: Vec<&PullRequestSummary> = prs.iter().collect();
        let items = build_pr_list_items_ref(&refs, 0, area_width);
        let width = 160;
        let height = u16::try_from(items.len()).unwrap_or(1).max(1);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
                let mut state = ListState::default().with_selected(Some(0));
                frame.render_stateful_widget(list, frame.area(), &mut state);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_screen(prs: Vec<PullRequestSummary>) -> Buffer {
        let mut app = App::new_for_test();
        app.state = AppState::PullRequestList;
        app.prs.pr_list = LoadState::Loaded(prs);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &mut app);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn fullwidth_pr_title_keeps_author_column_aligned_with_ascii() {
        let prs = vec![
            sample_pr(1, "Fix login bug", "alice"),
            sample_pr(2, "Add dark mode", "alice"),
            sample_pr(3, "ログイン修正", "alice"),
            sample_pr(4, &"あ".repeat(40), "alice"),
        ];
        let buf = render_rows(&prs, 40);
        let marker = "by @alice";
        let cols: Vec<Option<u16>> = (0..4).map(|y| find_column(&buf, y, marker)).collect();
        let rendered = (0..4)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        let ascii = cols[1].expect("ascii author column");
        assert_eq!(
            cols[2],
            Some(ascii),
            "short fullwidth title shifted the author column. columns={cols:?}\n{rendered}"
        );
        assert_eq!(
            cols[3],
            Some(ascii),
            "truncated fullwidth title shifted the author column. columns={cols:?}\n{rendered}"
        );
    }

    #[test]
    fn ascii_pr_titles_keep_the_same_author_column() {
        let prs = vec![
            sample_pr(1, "Fix login bug", "alice"),
            sample_pr(2, "Add dark mode", "alice"),
            sample_pr(10, "x", "alice"),
        ];
        let buf = render_rows(&prs, 40);
        let marker = "by @alice";
        let cols: Vec<Option<u16>> = (0..3).map(|y| find_column(&buf, y, marker)).collect();
        let rendered = (0..3)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            cols.iter().all(|col| *col == cols[0]),
            "ascii titles disagree on the author column. columns={cols:?}\n{rendered}"
        );
    }

    #[test]
    fn ascii_pr_list_screen_snapshot() {
        let buf = render_screen(vec![
            sample_pr(10, "Fix login bug", "alice"),
            sample_pr(9, "Add dark mode", "bob"),
        ]);
        assert_snapshot!(screen_text(&buf), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │PR List: test/repo (open)                                                                         │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Pull Requests (2)─────────────────────────────────────────────────────────────────────────────────┐
        │#10     Fix login bug                                                                   by @alice ▲
        │#9      Add dark mode                                                                     by @bob █
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
        │? Help | ! Shell | q/Esc Quit                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }
}
