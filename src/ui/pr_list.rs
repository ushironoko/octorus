use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};

use unicode_width::UnicodeWidthStr;

use super::chrome::{pane_block, PaneTitle};
use super::common::{fit_to_width, render_update_bar};
use super::theme::{Theme, SELECTION_MARKER_WIDTH};
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
            let detail = if let Some(ref filter) = app.prs.pr_list_filter {
                format!("({}/{})", filter.matched_indices.len(), total_prs)
            } else if app.prs.pr_list.is_loading() {
                format!("({}) {}", total_prs, app.spinner_char())
            } else if app.prs.pr_list_has_more {
                format!("({}+)", total_prs)
            } else {
                format!("({})", total_prs)
            };

            let theme = app.theme;
            let attention = app.sole_attention();
            let inner_width = chunks[1]
                .width
                .saturating_sub(3 + SELECTION_MARKER_WIDTH as u16)
                as usize;
            let items = build_pr_list_items_ref(&display_prs, display_selected, inner_width, theme);

            let mut list_state = ListState::default()
                .with_offset(app.prs.pr_list_scroll_offset)
                .with_selected(Some(display_selected));

            let block = pane_block(
                theme,
                PaneTitle::Labeled {
                    label: "Pull Requests",
                    detail: &detail,
                },
                attention,
            );
            let inner_area = block.inner(chunks[1]);
            let list = attention.rows(theme).apply(List::new(items).block(block));
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
    theme: Theme,
) -> Vec<ListItem<'static>> {
    prs.iter()
        .enumerate()
        .map(|(i, pr)| {
            let is_selected = i == selected;

            let draft_marker = if pr.is_draft { "[DRAFT] " } else { "" };

            let number_span = Span::styled(format!("#{:<5}", pr.number), theme.muted.style());

            let author_width = 4 + pr.author.login.width();
            let fixed_width = 6 + 2 + 2 + author_width;
            let title_width = area_width.saturating_sub(fixed_width).max(20);
            let full_title = format!("{}{}", draft_marker, pr.title);
            let title_style = if !is_selected && pr.is_draft {
                theme.muted.style()
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
                CiStatus::Pending => Span::styled("  ○", theme.warning.style()),
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
    use super::super::theme::Attention;
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
        let theme = Theme::test_default();
        let items = build_pr_list_items_ref(&refs, 0, area_width, theme);
        let width = 160;
        let height = u16::try_from(items.len()).unwrap_or(1).max(1);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let list = Attention::Focused.rows(theme).apply(List::new(items));
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
        ┏▶ PULL REQUESTS (2)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        ┃▌ #10     Fix login bug                                                                 by @alice ▲
        ┃  #9      Add dark mode                                                                   by @bob █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  █
        ┃                                                                                                  ▼
        ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │? Help | ! Shell | q/Esc Quit                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    fn cells_with<'a>(buf: &'a Buffer, symbol: &str) -> Vec<&'a ratatui::buffer::Cell> {
        let area = buf.area();
        let mut found = Vec::new();
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buf[(x, y)];
                if cell.symbol() == symbol {
                    found.push(cell);
                }
            }
        }
        found
    }

    fn draw_pr_list(app: &mut App) -> Buffer {
        draw_sized(app, 100, 24)
    }

    fn draw_sized(app: &mut App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                crate::ui::render(frame, app);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn pr_list_app() -> App {
        let mut app = App::new_for_test();
        app.state = AppState::PullRequestList;
        app.prs.pr_list = LoadState::Loaded(vec![
            sample_pr(1, "Fix login bug", "alice"),
            sample_pr(2, "Add dark mode", "bob"),
        ]);
        app
    }

    fn title_line(text: &str) -> &str {
        text.lines()
            .find(|line| line.contains("REQUESTS") || line.contains("Requests"))
            .expect("pr list title missing")
    }

    #[test]
    fn no_color_config_reaches_the_resolved_theme() {
        use crate::app::RepositoryAvailability;
        use crate::config::Config;
        use ratatui::style::Modifier;

        let config = Config {
            no_color: true,
            theme: crate::config::ThemeConfig {
                colors: crate::config::ThemeColors {
                    accent: Some(Color::Magenta),
                    ..crate::config::ThemeColors::default()
                },
                ..crate::config::ThemeConfig::default()
            },
            ..Config::default()
        };
        let app = App::new_pr_list("owner/repo", config, RepositoryAvailability::Available);
        assert_eq!(app.theme.accent.style().fg, None);
        assert!(app
            .theme
            .accent
            .style()
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(app.theme.muted.style().add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn no_color_dim_marker_is_the_glyph_only() {
        use ratatui::style::Modifier;

        let mut app = pr_list_app();
        app.state = AppState::Help;
        app.config.no_color = true;
        app.theme = Theme::resolve(&crate::config::ThemeConfig::default(), true);
        let buf = {
            let backend = TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    render(frame, &mut app);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let marks = cells_with(&buf, "▏");
        assert!(!marks.is_empty(), "dim row did not draw ▏");
        assert!(
            marks.iter().all(|cell| {
                !cell.modifier.contains(Modifier::DIM)
                    && !cell.modifier.contains(Modifier::BOLD)
                    && cell.bg == Color::Reset
            }),
            "▏ cell is not the glyph only: {:?}",
            marks
                .iter()
                .map(|cell| (cell.modifier, cell.fg, cell.bg))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn symbol_popup_removes_the_pr_list_focus_arrow() {
        use crate::app::SymbolPopupState;

        let mut app = pr_list_app();
        app.symbol_popup = Some(SymbolPopupState {
            symbols: vec![("foo".to_string(), 1, 4), ("bar".to_string(), 6, 9)],
            selected: 0,
        });
        let text = screen_text(&draw_pr_list(&mut app));
        let title = title_line(&text);
        assert!(
            !title.contains('▶'),
            "symbol popup left the focus arrow on the pane title:\n{title}\n{text}"
        );
        assert!(
            title.contains("Pull Requests"),
            "pane title changed:\n{title}"
        );
    }

    fn symbol_popup_app() -> App {
        use crate::app::SymbolPopupState;

        let mut app = pr_list_app();
        app.symbol_popup = Some(SymbolPopupState {
            symbols: vec![("foo".to_string(), 1, 4), ("bar".to_string(), 6, 9)],
            selected: 0,
        });
        app
    }

    #[test]
    fn symbol_popup_selected_row_matches_the_pr_list_marker() {
        let list = render_screen(vec![
            sample_pr(1, "Fix login bug", "alice"),
            sample_pr(2, "Add dark mode", "bob"),
        ]);
        let list_mark = cells_with(&list, "▌")
            .into_iter()
            .next()
            .expect("pr list drew no ▌");
        let (fg, bg, modifier) = (list_mark.fg, list_mark.bg, list_mark.modifier);

        let mut app = symbol_popup_app();
        let buf = draw_pr_list(&mut app);
        let marks = cells_with(&buf, "▌");
        assert!(
            !marks.is_empty(),
            "symbol popup drew no ▌\n{}",
            screen_text(&buf)
        );
        assert!(
            marks
                .iter()
                .all(|cell| cell.fg == fg && cell.bg == bg && cell.modifier == modifier),
            "popup ▌ style {marks:?} != pr list fg={fg:?} bg={bg:?} mod={modifier:?}\n{}",
            screen_text(&buf)
        );

        let name = row_cells(&buf, "foo");
        let letter = name
            .iter()
            .find(|cell| cell.symbol() == "f")
            .expect("selected symbol name missing");
        let title = row_cells(&list, "Fix");
        let title_letter = title
            .iter()
            .find(|cell| cell.symbol() == "F")
            .expect("pr title missing");
        assert_eq!(letter.bg, title_letter.bg);
        assert_eq!(letter.modifier, title_letter.modifier);
        assert_ne!(letter.bg, Color::Cyan);
        assert_ne!(letter.fg, Color::Black);
    }

    #[test]
    fn symbol_popup_no_color_selection_is_the_bar() {
        let mut app = symbol_popup_app();
        app.theme = Theme::resolve(&crate::config::ThemeConfig::default(), true);
        let buf = draw_pr_list(&mut app);
        let marks = cells_with(&buf, "▌");
        assert!(
            !marks.is_empty(),
            "NO_COLOR symbol popup drew no ▌\n{}",
            screen_text(&buf)
        );
        assert!(
            marks.iter().all(|cell| {
                cell.bg == Color::Reset && cell.fg != Color::Cyan && cell.fg != Color::Black
            }),
            "NO_COLOR selection still carries a color: {:?}",
            marks
                .iter()
                .map(|cell| (cell.fg, cell.bg, cell.modifier))
                .collect::<Vec<_>>()
        );
    }

    fn row_cells<'a>(buf: &'a Buffer, needle: &str) -> Vec<&'a ratatui::buffer::Cell> {
        let y = (0..buf.area().height)
            .find(|y| row_text(buf, *y).contains(needle))
            .unwrap_or_else(|| panic!("row {needle} missing\n{}", screen_text(buf)));
        (0..buf.area().width).map(|x| &buf[(x, y)]).collect()
    }

    fn popup_with(names: &[&str]) -> App {
        use crate::app::SymbolPopupState;

        let mut app = pr_list_app();
        app.symbol_popup = Some(SymbolPopupState {
            symbols: names
                .iter()
                .map(|name| ((*name).to_string(), 1, 4))
                .collect(),
            selected: 0,
        });
        app
    }

    fn popup_outer_width(buf: &Buffer, name: &str) -> u16 {
        let y = (0..buf.area().height)
            .find(|y| row_text(buf, *y).contains(name) || row_text(buf, *y).contains('…'))
            .unwrap_or_else(|| panic!("popup row missing\n{}", screen_text(buf)));
        let bar = (0..buf.area().width)
            .find(|x| buf[(*x, y)].symbol() == "▌")
            .unwrap_or_else(|| panic!("selection bar missing\n{}", row_text(buf, y)));
        let left = (0..bar)
            .rev()
            .find(|x| buf[(*x, y)].symbol() == "│")
            .unwrap_or_else(|| panic!("left border missing\n{}", row_text(buf, y)));
        let right = (bar..buf.area().width)
            .find(|x| buf[(*x, y)].symbol() == "│")
            .unwrap_or_else(|| panic!("right border missing\n{}", row_text(buf, y)));
        right.saturating_sub(left).saturating_add(1)
    }

    #[test]
    fn symbol_popup_longest_name_keeps_a_gap_before_the_frame() {
        let name = "abcdefghijabcdefghij";
        let mut app = popup_with(&[name, "short"]);
        let buf = draw_sized(&mut app, 80, 16);
        let row = row_text(&buf, {
            (0..buf.area().height)
                .find(|y| row_text(&buf, *y).contains(name))
                .unwrap_or_else(|| panic!("name clipped off the popup\n{}", screen_text(&buf)))
        });
        let at = row.find(name).expect("name");
        let next = row[at + name.len()..].chars().next();
        assert_eq!(
            next,
            Some(' '),
            "longest name touches the frame: {row}\n{}",
            screen_text(&buf)
        );
    }

    #[test]
    fn symbol_popup_fullwidth_name_uses_the_same_width_as_ascii() {
        let ascii = "a".repeat(20);
        let wide = "あ".repeat(10);
        let mut ascii_app = popup_with(&[&ascii]);
        let mut wide_app = popup_with(&[&wide]);
        let ascii_buf = draw_sized(&mut ascii_app, 80, 12);
        let wide_buf = draw_sized(&mut wide_app, 80, 12);
        assert_eq!(
            popup_outer_width(&ascii_buf, &ascii),
            popup_outer_width(&wide_buf, &wide),
            "byte length sized the fullwidth name\nascii:\n{}\nwide:\n{}",
            screen_text(&ascii_buf),
            screen_text(&wide_buf)
        );
    }

    #[test]
    fn symbol_popup_rows_use_one_space_after_the_bar() {
        let mut app = popup_with(&["result", "shortFn"]);
        let rendered = screen_text(&draw_sized(&mut app, 40, 12));
        assert!(
            rendered.contains("▌ result  "),
            "selected row is not ▌, one space, the name, and the pad:\n{rendered}"
        );
        assert!(
            !rendered.contains("▌  result"),
            "selected row kept a pad before the name:\n{rendered}"
        );
        assert!(
            rendered.contains("  shortFn"),
            "unselected row is not two spaces and the name:\n{rendered}"
        );
    }

    #[test]
    fn shell_overlay_removes_the_pr_list_focus_arrow() {
        use crate::app::{CachedShellLine, ShellCommandResult, ShellPhase, ShellState};

        let phases = [
            ShellPhase::Input,
            ShellPhase::Running,
            ShellPhase::Cancelling,
            ShellPhase::Done(ShellCommandResult {
                command: "echo hi".to_string(),
                stdout: "hi\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
                cached_lines: vec![CachedShellLine {
                    text: "hi".to_string(),
                    is_stderr: false,
                }],
                total_lines: 1,
            }),
        ];
        for phase in phases {
            let mut app = pr_list_app();
            app.shell_state = Some(ShellState {
                input: String::new(),
                cursor: 0,
                phase,
                scroll_offset: 0,
            });
            let text = screen_text(&draw_pr_list(&mut app));
            let title = title_line(&text);
            assert!(
                !title.contains('▶'),
                "shell overlay left the focus arrow on the pane title:\n{title}\n{text}"
            );
            assert!(
                title.contains("Pull Requests"),
                "pane title changed:\n{title}"
            );
        }
    }

    #[test]
    fn focused_selected_row_uses_bar_bold_and_no_background() {
        let buf = render_screen(vec![
            sample_pr(1, "Fix login bug", "alice"),
            sample_pr(2, "Add dark mode", "bob"),
        ]);
        let marks = cells_with(&buf, "▌");
        assert!(!marks.is_empty(), "focused row did not draw ▌");
        assert!(
            marks
                .iter()
                .all(|cell| cell.modifier.contains(ratatui::style::Modifier::BOLD)),
            "▌ cell is not bold"
        );
        assert!(
            marks.iter().all(|cell| cell.bg == Color::Reset),
            "▌ cell has a background"
        );
        let text = screen_text(&buf);
        assert!(text.contains("▶ PULL REQUESTS (2)"), "{text}");
    }

    #[test]
    fn dim_selected_row_uses_muted_bar_without_bold() {
        let mut app = App::new_for_test();
        app.state = AppState::Help;
        app.prs.pr_list = LoadState::Loaded(vec![
            sample_pr(1, "Fix login bug", "alice"),
            sample_pr(2, "Add dark mode", "bob"),
        ]);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &mut app);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let marks = cells_with(&buf, "▏");
        assert!(!marks.is_empty(), "dim row did not draw ▏");
        assert!(
            marks
                .iter()
                .all(|cell| !cell.modifier.contains(ratatui::style::Modifier::BOLD)),
            "▏ cell is bold"
        );
        assert!(marks.iter().all(|cell| cell.bg == Color::Reset));
        assert!(cells_with(&buf, "▌").is_empty());
        let text = screen_text(&buf);
        assert!(text.contains("Pull Requests (2)"), "{text}");
        assert!(!text.contains("▶ PULL REQUESTS"), "{text}");
    }
}
