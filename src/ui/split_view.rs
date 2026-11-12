use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};

use super::chrome::{pane_block, PaneTitle};
use super::common::{
    build_pending_review_span, build_pr_info, pending_comment_tag, render_rally_status_bar,
};
use super::diff_view;
use super::file_list::{build_file_list_items, build_file_list_items_ref, build_tree_row_item};
use super::theme::{Attention, SplitAttention, Theme};
use crate::app::{App, DataState};
use crate::github::ChangedFile;

pub fn render(frame: &mut Frame, app: &mut App) {
    let has_rally = app.has_background_rally();

    let outer_constraints = if has_rally {
        vec![Constraint::Min(0), Constraint::Length(1)]
    } else {
        vec![Constraint::Min(0)]
    };

    let outer_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(outer_constraints)
        .split(frame.area());

    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(app.config.layout.left_panel_percent()),
            Constraint::Percentage(app.config.layout.right_panel_percent()),
        ])
        .split(outer_chunks[0]);

    let theme = app.theme;
    let panes = app.split_attention();

    render_file_list_pane(frame, app, h_chunks[0], panes.files, theme);
    render_diff_pane(frame, app, h_chunks[1], panes, theme);
    register_split_divider(app, h_chunks[0], outer_chunks[0]);

    if has_rally {
        render_rally_status_bar(frame, outer_chunks[1], app);
    }
}

/// スプリット境界(左ペイン右端の 2 桁)をドラッグリサイズ対象として登録する。
/// ペイン内容の登録より後に呼ぶことで最上位ヒットになる。
pub(super) fn register_split_divider(
    app: &mut App,
    left_pane: ratatui::layout::Rect,
    body: ratatui::layout::Rect,
) {
    app.hit_map.push(
        ratatui::layout::Rect {
            x: left_pane.right().saturating_sub(1),
            y: left_pane.y,
            width: 2,
            height: left_pane.height,
        },
        crate::ui::hit::HitTarget::SplitDivider {
            split: crate::ui::hit::SplitKind::LeftPanel,
            body_x: body.x,
            body_width: body.width,
        },
    );
}

fn render_file_list_pane(
    frame: &mut Frame,
    app: &mut App,
    area: ratatui::layout::Rect,
    attention: Attention,
    theme: Theme,
) {
    let is_focused = attention.has_keys();

    let has_filter_bar = app
        .file_list_filter
        .as_ref()
        .is_some_and(|f| f.input_active);

    let mut constraints = vec![Constraint::Length(3), Constraint::Min(0)];
    if has_filter_bar {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Length(3));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    // Local mode shares the file list's wording (`[LOCAL] Local HEAD diff`).
    // A loaded PR keeps `PR #n: title` — the author suffix the file list adds
    // is clipped by this pane.
    let pr_info = if app.is_local_mode() {
        build_pr_info(app)
    } else {
        match &app.data_state {
            DataState::Loaded { pr, .. } => {
                format!("PR #{}: {}", pr.number, pr.title)
            }
            _ => match app.pr_number {
                Some(n) => format!("PR #{}", n),
                None => "PR".to_string(),
            },
        }
    };

    let mut header_block = pane_block(theme, PaneTitle::AsIs("octorus"), attention);
    if let Some(note) = build_pending_review_span(app, theme) {
        header_block = header_block.title(Line::from(note).right_aligned());
    }
    let header = Paragraph::new(pr_info).block(header_block);
    frame.render_widget(header, chunks[0]);

    let files = app.files();
    let total_files = files.len();

    if let Some(ref filter) = app.file_list_filter {
        if filter.matched_indices.is_empty() {
            let empty_msg = format!("No matches for '{}'", filter.query);
            let detail = format!("(0/{total_files})");
            let empty = Paragraph::new(empty_msg)
                .style(Style::default().fg(Color::DarkGray))
                .block(pane_block(
                    theme,
                    PaneTitle::Labeled {
                        label: "Files",
                        detail: &detail,
                    },
                    attention,
                ));
            frame.render_widget(empty, chunks[1]);
        } else {
            let filtered: Vec<&ChangedFile> =
                filter.matched_indices.iter().map(|&i| &files[i]).collect();
            let display_selected = filter.selected.unwrap_or(0);
            let display_count = filtered.len();

            let items = build_file_list_items_ref(&filtered, &app.cmt.file_comment_counts);
            let detail = format!("({display_count}/{total_files})");
            let block = pane_block(
                theme,
                PaneTitle::Labeled {
                    label: "Files",
                    detail: &detail,
                },
                attention,
            );
            let viewport_rows = block.inner(chunks[1]).height as usize;
            let list = attention.rows(theme).apply(List::new(items).block(block));

            let mut list_state = ListState::default()
                .with_offset(app.file_list_scroll_offset)
                .with_selected(Some(display_selected));

            frame.render_stateful_widget(list, chunks[1], &mut list_state);

            app.file_list_scroll_offset = list_state.offset();

            if display_count > viewport_rows {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("▲"))
                    .end_symbol(Some("▼"));

                let mut scrollbar_state =
                    ScrollbarState::new(display_count.saturating_sub(1)).position(display_selected);

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
    } else if app.is_file_tree_active() {
        let tree = app.file_tree_state.as_ref().unwrap();
        let row_count = tree.row_count();
        let max_count = files
            .iter()
            .filter_map(|f| app.cmt.file_comment_counts.get(&f.filename).copied())
            .max()
            .unwrap_or(0);
        let col_width = super::file_list::comment_col_width(max_count);
        let items: Vec<ListItem> = tree
            .visible_rows
            .iter()
            .map(|row| build_tree_row_item(files, row, &app.cmt.file_comment_counts, col_width))
            .collect();

        let detail = format!("({total_files}) [tree]");
        let block = pane_block(
            theme,
            PaneTitle::Labeled {
                label: "Files",
                detail: &detail,
            },
            attention,
        );
        let viewport_rows = block.inner(chunks[1]).height as usize;
        let list = attention.rows(theme).apply(List::new(items).block(block));

        let mut list_state = ListState::default()
            .with_offset(tree.scroll_offset)
            .with_selected(Some(tree.selected_row));

        frame.render_stateful_widget(list, chunks[1], &mut list_state);

        if let Some(ref mut tree) = app.file_tree_state {
            tree.scroll_offset = list_state.offset();
        }

        if row_count > viewport_rows {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));
            let selected = app.file_tree_state.as_ref().map_or(0, |t| t.selected_row);
            let mut scrollbar_state =
                ScrollbarState::new(row_count.saturating_sub(1)).position(selected);
            frame.render_stateful_widget(
                scrollbar,
                chunks[1].inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }
    } else {
        let items = build_file_list_items(files, &app.cmt.file_comment_counts);
        let detail = format!("({total_files})");
        let block = pane_block(
            theme,
            PaneTitle::Labeled {
                label: "Files",
                detail: &detail,
            },
            attention,
        );
        let viewport_rows = block.inner(chunks[1]).height as usize;
        let list = attention.rows(theme).apply(List::new(items).block(block));

        let mut list_state = ListState::default()
            .with_offset(app.file_list_scroll_offset)
            .with_selected(Some(app.selected_file));

        frame.render_stateful_widget(list, chunks[1], &mut list_state);

        app.file_list_scroll_offset = list_state.offset();
        if let Some(sel) = list_state.selected() {
            app.selected_file = sel;
        }

        if total_files > viewport_rows {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            let mut scrollbar_state =
                ScrollbarState::new(total_files.saturating_sub(1)).position(app.selected_file);

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

    super::file_list::register_file_list_hits(app, chunks[1]);

    let mut next_chunk = 2;

    if has_filter_bar {
        if let Some(ref filter) = app.file_list_filter {
            let cursor_display = format!("/{}", filter.query);
            let filter_bar = Paragraph::new(Line::from(vec![
                Span::styled("Filter: ", Style::default().fg(Color::Cyan)),
                Span::styled(cursor_display, Style::default().fg(Color::White)),
                Span::styled("│", Style::default().fg(Color::DarkGray)),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
            frame.render_widget(filter_bar, chunks[next_chunk]);
        }
        next_chunk += 1;
    }

    let help_text_owned;
    let help_text = if is_focused {
        help_text_owned = super::footer::footer_hint_back(&app.config.keybindings);
        help_text_owned.as_str()
    } else {
        "←/h: focus files"
    };
    let footer_line = super::footer::build_pane_help_line(app, help_text, is_focused);
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block_with_focus(
        app,
        attention.border_style(theme),
        attention.has_keys(),
    ));
    frame.render_widget(footer, chunks[next_chunk]);
}

fn render_diff_pane(
    frame: &mut Frame,
    app: &mut App,
    area: ratatui::layout::Rect,
    panes: SplitAttention,
    theme: Theme,
) {
    if panes.comments.has_keys() {
        render_diff_pane_with_comments(frame, app, area, panes, theme);
    } else {
        render_diff_pane_normal(frame, app, area, panes.diff, theme);
    }
}

fn render_diff_pane_normal(
    frame: &mut Frame,
    app: &mut App,
    area: ratatui::layout::Rect,
    attention: Attention,
    theme: Theme,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    render_diff_header(frame, app, chunks[0], attention, theme);
    render_diff_body(frame, app, chunks[1], attention, theme);

    let footer_text_owned;
    let footer_text = if attention.has_keys() {
        footer_text_owned = super::footer::footer_hint_back(&app.config.keybindings);
        footer_text_owned.as_str()
    } else {
        "Enter/→: focus diff"
    };

    render_diff_footer(frame, app, chunks[2], footer_text, attention, theme);
}

fn render_diff_footer(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    help_text: &str,
    attention: Attention,
    theme: Theme,
) {
    let footer_line =
        super::footer::build_footer_line_with_focus(app, help_text, attention.has_keys());
    let footer = Paragraph::new(footer_line).block(super::footer::build_footer_block_with_focus(
        app,
        attention.border_style(theme),
        attention.has_keys(),
    ));
    frame.render_widget(footer, area);
}

fn render_diff_pane_with_comments(
    frame: &mut Frame,
    app: &mut App,
    area: ratatui::layout::Rect,
    panes: SplitAttention,
    theme: Theme,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(50),
            Constraint::Percentage(40),
            Constraint::Length(3),
        ])
        .split(area);

    render_diff_header(frame, app, chunks[0], panes.diff, theme);
    render_diff_body(frame, app, chunks[1], panes.diff, theme);

    let indices = app.get_comment_indices_at_current_line();
    let mut lines: Vec<Line> = vec![];

    if indices.is_empty() {
        lines.push(Line::from(Span::styled(
            "No comments. c: comment, s: suggestion",
            Style::default().fg(Color::DarkGray),
        )));
    } else if let Some(ref comments) = app.cmt.review_comments {
        for (i, &idx) in indices.iter().enumerate() {
            let Some(comment) = comments.get(idx) else {
                continue;
            };

            if i > 0 {
                lines.push(Line::from(Span::styled(
                    "───────────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
            }

            lines.push(Line::from(vec![
                Span::styled(
                    format!("@{}", comment.user.login),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!(" (line {})", comment.line_label()),
                    Style::default().fg(Color::DarkGray),
                ),
                pending_comment_tag(comment, theme),
            ]));

            for line in comment.body.lines() {
                lines.push(Line::from(line.to_string()));
            }
            lines.push(Line::from(""));
        }
    }

    let title = "Comments (j/k/↑↓: scroll, c: comment, s: suggest, r: reply)";

    let block = pane_block(
        theme,
        PaneTitle::Labeled {
            label: "",
            detail: title,
        },
        panes.comments,
    );
    let inner_area = block.inner(chunks[2]);
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: true })
        .scroll((app.cmt.comment_panel_scroll, 0));
    // The scroll counts wrapped rows, so the scrollbar measures those too.
    let total_rows = paragraph.line_count(inner_area.width);
    frame.render_widget(paragraph, chunks[2]);
    app.hit_map.push(
        inner_area,
        crate::ui::hit::HitTarget::Pane {
            pane: crate::ui::hit::PaneKind::CommentPanel,
        },
    );

    if total_rows > inner_area.height as usize {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let max_scroll = total_rows.saturating_sub(1);
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll).position(app.cmt.comment_panel_scroll as usize);

        frame.render_stateful_widget(
            scrollbar,
            chunks[2].inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }

    let footer_text = "j/k/↑↓: scroll | n/N: jump | Tab: switch | r: reply | c: comment | s: suggest | →/l: fullscreen | ←/h/q: close";
    render_diff_footer(frame, app, chunks[3], footer_text, panes.comments, theme);
}

fn render_diff_header(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    attention: Attention,
    theme: Theme,
) {
    let header_text = app
        .files()
        .get(app.selected_file)
        .map(|file| {
            format!(
                "{} (+{} -{})",
                file.filename, file.additions, file.deletions
            )
        })
        .unwrap_or_else(|| "No file selected".to_string());

    let header = Paragraph::new(header_text).block(pane_block(
        theme,
        PaneTitle::Labeled {
            label: "Diff",
            detail: "",
        },
        attention,
    ));
    frame.render_widget(header, area);
}

fn render_diff_body(
    frame: &mut Frame,
    app: &mut App,
    area: ratatui::layout::Rect,
    attention: Attention,
    theme: Theme,
) {
    let visible_height = area.height.saturating_sub(2) as usize;
    app.diff_scroll.set_visible_lines(visible_height);
    let (lines, scroll_row, first_line) = if let Some(ref cache) = app.diff_store.current {
        let line_count = cache.lines.len();
        // Slice from scroll_offset, bounded to visible viewport + buffer for wrap handling.
        let max_scroll = line_count.saturating_sub(visible_height);
        let start = app.diff_scroll.scroll_offset.min(max_scroll);
        let end = (start + visible_height + 10).min(line_count);
        let multiline_range = app
            .multiline_selection
            .as_ref()
            .map(|s| (s.start(), s.end()));
        let rendered = diff_view::render_cached_lines(
            cache,
            start..end,
            app.diff_scroll.selected_line,
            &app.cmt.file_comment_lines,
            app.config.diff.bg_color,
            multiline_range,
            area.width.saturating_sub(2),
            theme,
        );
        (rendered, 0u16, start)
    } else {
        let file = app.files().get(app.selected_file);
        let rendered = match file {
            Some(f) => match f.patch.as_ref() {
                Some(_) => vec![Line::from("Loading diff...")],
                None => {
                    if app.is_lazy_diff_loading() {
                        vec![Line::from("Loading diff...")]
                    } else {
                        vec![Line::from("No diff available")]
                    }
                }
            },
            None => vec![Line::from("No file selected")],
        };
        (rendered, app.diff_scroll.scroll_offset as u16, 0)
    };

    let block = attention.frame(theme);
    let inner_area = block.inner(area);
    diff_view::register_diff_hit_regions(
        &mut app.hit_map,
        inner_area,
        &lines,
        first_line,
        scroll_row as usize,
    );
    let diff_block = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_row, 0));

    frame.render_widget(diff_block, area);

    if let Some(ref cache) = app.diff_store.current {
        let total_lines = cache.lines.len();
        let visible_height = area.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible_height);
        if max_scroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            let clamped_position = app.diff_scroll.scroll_offset.min(max_scroll);
            let mut scrollbar_state = ScrollbarState::new(max_scroll).position(clamped_position);

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppState, DataState, Toast};
    use crate::github::{Branch, ChangedFile, PullRequest, User};
    use crate::ui::test_buffer::screen_text;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn changed(name: &str) -> ChangedFile {
        ChangedFile {
            filename: name.to_string(),
            status: "modified".to_string(),
            additions: 1,
            deletions: 0,
            patch: None,
            viewed: false,
        }
    }

    fn loaded_app() -> App {
        loaded_app_with(vec![
            changed("src/a.rs"),
            changed("src/b.rs"),
            changed("src/c.rs"),
        ])
    }

    fn loaded_app_with(files: Vec<ChangedFile>) -> App {
        let mut app = App::new_for_test();
        app.data_state = DataState::Loaded {
            pr: Box::new(PullRequest {
                number: 1,
                node_id: None,
                title: "Title".to_string(),
                body: None,
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
                    login: "alice".to_string(),
                },
                updated_at: "2025-01-01T00:00:00Z".to_string(),
            }),
            files,
        };
        app.toggle_file_tree();
        app
    }

    fn draw(app: &mut App) -> String {
        draw_sized(app, 120, 24)
    }

    fn draw_sized(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, app);
            })
            .unwrap();
        screen_text(terminal.backend().buffer())
    }

    #[test]
    fn focused_diff_title_is_the_label_and_the_path_stays_in_the_header() {
        let mut app = loaded_app();
        app.state = AppState::SplitViewDiff;
        for width in [120u16, 40] {
            let text = draw_sized(&mut app, width, 16);
            let lines: Vec<&str> = text.lines().collect();
            let title_at = lines
                .iter()
                .position(|line| line.contains("▶ DIFF"))
                .unwrap_or_else(|| panic!("focused diff title missing at width {width}:\n{text}"));
            assert!(
                !lines[title_at].contains("src/a.rs"),
                "filename leaked into the diff title at width {width}:\n{}\n{text}",
                lines[title_at]
            );
            assert!(
                lines[title_at + 1].contains("src/a.rs"),
                "diff header dropped the path at width {width}:\n{text}"
            );
        }
    }

    #[test]
    fn file_pane_title_shows_the_arrow_only_when_focused() {
        let mut app = loaded_app();
        app.state = AppState::SplitViewFileList;
        let focused = draw(&mut app);
        assert!(
            focused.contains("▶ FILES (3) [tree]"),
            "focused title missing:\n{focused}"
        );

        app.state = AppState::SplitViewDiff;
        let dim = draw(&mut app);
        assert!(
            dim.contains("Files (3) [tree]"),
            "dim title missing:\n{dim}"
        );
        assert!(!dim.contains("▶ FILES"), "dim title drew an arrow:\n{dim}");
    }

    fn loaded_local_app(auto_focus: bool) -> App {
        let mut app = loaded_app();
        app.set_local_mode(true);
        app.set_local_auto_focus(auto_focus);
        if let DataState::Loaded { pr, .. } = &mut app.data_state {
            pr.number = 0;
            pr.title = "Local HEAD diff".to_string();
        }
        app.state = AppState::SplitViewFileList;
        app
    }

    fn draw_file_list(app: &mut App) -> String {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::file_list::render(frame, app))
            .unwrap();
        screen_text(terminal.backend().buffer())
    }

    /// The file list header comes from `build_pr_info`. The split view used to
    /// format the loaded PR itself, so the synthetic local PR (number 0) read
    /// `PR #0: Local HEAD diff`.
    #[test]
    fn local_mode_split_header_matches_the_file_list() {
        for (auto_focus, expected) in [
            (false, "[LOCAL] Local HEAD diff"),
            (true, "[LOCAL AF] Local HEAD diff"),
        ] {
            let mut app = loaded_local_app(auto_focus);
            assert_eq!(crate::ui::common::build_pr_info(&app), expected);

            let split = draw(&mut app);
            assert!(
                split.contains(expected),
                "auto_focus={auto_focus}:\n{split}"
            );
            assert!(
                !split.contains("PR #0"),
                "auto_focus={auto_focus} still says PR #0:\n{split}"
            );

            let list = draw_file_list(&mut app);
            assert!(
                list.contains(expected),
                "file list auto_focus={auto_focus}:\n{list}"
            );
        }
    }

    #[test]
    fn local_mode_split_header_while_loading_is_not_pr_zero() {
        let mut app = App::new_for_test();
        app.set_local_mode(true);
        app.pr_number = Some(0);
        app.state = AppState::SplitViewFileList;

        let text = draw(&mut app);

        assert!(text.contains("[LOCAL] Local HEAD diff"), "{text}");
        assert!(!text.contains("PR #0"), "{text}");
    }

    /// A loaded PR keeps `PR #n: title`. The file list adds `by @login`, which
    /// this narrower pane clips (`Add zen mode by @us`).
    #[test]
    fn split_header_keeps_a_loaded_pr_title_without_the_author() {
        let mut app = loaded_app();
        app.state = AppState::SplitViewFileList;

        let split = draw(&mut app);
        assert!(split.contains("PR #1: Title"), "{split}");
        assert!(!split.contains("by @alice"), "{split}");

        let list = draw_file_list(&mut app);
        assert!(
            list.contains("PR #1: Title by @alice"),
            "file list header drifted:\n{list}"
        );
    }

    /// The status line belongs to one footer. Both panes used to draw it, so a
    /// message showed twice, on the file pane and on the diff pane. The file pane
    /// keeps its own help text in both focus states.
    #[test]
    fn a_status_message_is_drawn_once_whichever_pane_has_focus() {
        let mut app = loaded_app();
        let focused_file_help = crate::ui::footer::footer_hint_back(&app.config.keybindings);
        app.cmt.submission_result = Some(Toast::success("Added to pending review".to_string()));

        for (state, file_pane_help) in [
            (AppState::SplitViewFileList, focused_file_help.as_str()),
            (AppState::SplitViewDiff, "←/h: focus files"),
        ] {
            app.state = state;

            let text = draw(&mut app);

            assert_eq!(
                text.matches("Added to pending review").count(),
                1,
                "{state:?}:\n{text}"
            );
            assert!(
                text.contains(file_pane_help),
                "{state:?}: the file pane lost its help {file_pane_help:?}:\n{text}"
            );
        }
    }

    #[test]
    fn the_reload_progress_is_drawn_once_in_the_split_view() {
        let mut app = loaded_app();
        app.state = AppState::SplitViewFileList;
        app.cmt.comments_loading = true;

        let text = draw(&mut app);

        assert_eq!(text.matches("Loading comments...").count(), 1, "{text}");
    }

    #[test]
    fn comment_panel_takes_the_arrow_from_the_diff() {
        let mut app = loaded_app();
        app.state = AppState::SplitViewDiff;
        app.cmt.comment_panel_open = true;
        let text = draw(&mut app);
        assert!(
            text.contains("▶ Comments ("),
            "comment panel is not focused:\n{text}"
        );
        assert!(
            !text.contains("▶ DIFF"),
            "diff kept the arrow while comments are open:\n{text}"
        );
    }

    #[test]
    fn split_file_pane_whose_rows_fit_draws_no_scrollbar() {
        let mut app = loaded_app();
        app.state = AppState::SplitViewFileList;

        let text = draw(&mut app);

        assert!(!text.contains('▲'), "{text}");
    }

    #[test]
    fn split_file_pane_with_rows_past_the_pane_keeps_its_scrollbar() {
        let files = (1..=40)
            .map(|n| changed(&format!("src/file{n}.rs")))
            .collect();
        let mut app = loaded_app_with(files);
        app.state = AppState::SplitViewFileList;

        let text = draw(&mut app);

        assert!(text.contains('▲'), "{text}");
    }

    /// The marker sits between `-fn b() {}` and the `+` rows, as git writes it.
    const NO_NEWLINE_PATCH: &str = concat!(
        "@@ -1,2 +1,3 @@\n",
        " fn a() {}\n",
        "-fn b() {}\n",
        "\\ No newline at end of file\n",
        "+fn b() { 1 }\n",
        "+fn c() {}\n",
    );

    #[test]
    fn split_view_draws_the_marker_muted_in_the_prefix_column() {
        use crate::config::ThemeConfig;
        use crate::ui::test_buffer::{find_column, row_text};

        let marker = "\\ No newline at end of file";
        let mut app = loaded_app();
        if let DataState::Loaded { files, .. } = &mut app.data_state {
            files[0].patch = Some(NO_NEWLINE_PATCH.to_string());
        }
        app.diff_store.current = Some(diff_view::build_plain_diff_cache(NO_NEWLINE_PATCH, 4));
        app.state = AppState::SplitViewDiff;
        let mut terminal = Terminal::new(TestBackend::new(120, 16)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let row_with = |needle: &str| {
            (0..buf.area().height)
                .find(|y| row_text(&buf, *y).contains(needle))
                .unwrap_or_else(|| panic!("no row shows {needle:?}\n{}", screen_text(&buf)))
        };
        let marker_y = row_with(marker);
        let marker_x = find_column(&buf, marker_y, marker).unwrap();
        let content_x = find_column(&buf, row_with("fn a() {}"), "fn a() {}").unwrap();
        assert_eq!(marker_x + 1, content_x, "{}", screen_text(&buf));

        let muted = Theme::resolve(&ThemeConfig::default(), false)
            .muted
            .style()
            .fg
            .expect("the muted token has a color");
        for x in marker_x..marker_x + u16::try_from(marker.len()).unwrap() {
            assert_eq!(buf[(x, marker_y)].fg, muted, "marker cell {x}");
        }
    }
}

#[cfg(test)]
mod pending_header_tests {
    use super::*;
    use crate::app::{App, AppState, PendingDrafts};
    use crate::config::ThemeColors;
    use crate::ui::test_buffer::{cells_of, row_text, theme_with, CUSTOM_WARNING};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The split view's file pane carries the PR header, so the note is on its border.
    #[test]
    fn split_view_file_pane_border_shows_the_pending_review_drafts() {
        let mut app = App::new_for_test();
        app.state = AppState::SplitViewFileList;
        app.pr_number = Some(244);
        app.cmt.pending_drafts = PendingDrafts::Known(3);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let border = row_text(terminal.backend().buffer(), 0);
        assert!(border.contains("Pending review: 3 drafts"), "{border}");
    }

    #[test]
    fn split_view_file_pane_border_note_takes_the_theme_warning_color() {
        let mut app = App::new_for_test();
        app.state = AppState::SplitViewFileList;
        app.pr_number = Some(244);
        app.cmt.pending_drafts = PendingDrafts::Known(3);
        app.theme = theme_with(ThemeColors {
            warning: Some(CUSTOM_WARNING),
            ..ThemeColors::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let cells = cells_of(terminal.backend().buffer(), "Pending review: 3 drafts")
            .expect("the note is drawn on the border");
        assert!(
            cells.iter().all(|cell| cell.fg == CUSTOM_WARNING),
            "{cells:?}"
        );
    }
}

#[cfg(test)]
mod pending_comment_panel_tests {
    use super::*;
    use crate::app::AppState;
    use crate::github::comment::CommentPublication;
    use crate::ui::test_buffer::{
        app_with_comment_on_added_line, comment_on_added_line, screen_text,
    };
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The split view's diff pane with the comment panel open on the `+added` line.
    fn split_screen(publication: CommentPublication) -> String {
        let mut app = app_with_comment_on_added_line(comment_on_added_line(publication));
        app.state = AppState::SplitViewDiff;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        screen_text(terminal.backend().buffer())
    }

    #[test]
    fn split_panel_tags_a_draft_header() {
        let text = split_screen(CommentPublication::PendingDraft);

        assert!(text.contains("@alice (line 2) · pending review"), "{text}");
    }

    #[test]
    fn split_panel_leaves_a_published_header_untagged() {
        let text = split_screen(CommentPublication::Published);

        assert!(text.contains("@alice (line 2)"), "{text}");
        assert!(!text.contains("pending review"), "{text}");
    }

    /// The split diff pane with the comment panel open on the `+added` line,
    /// whose comment has the given body.
    fn split_screen_with_body(body: &str) -> String {
        let mut comment = comment_on_added_line(CommentPublication::Published);
        comment.body = body.to_string();
        let mut app = app_with_comment_on_added_line(comment);
        app.state = AppState::SplitViewDiff;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        screen_text(terminal.backend().buffer())
    }

    #[test]
    fn split_comment_panel_whose_text_fits_draws_no_scrollbar() {
        let text = split_screen_with_body("Short.");

        assert!(!text.contains('▲'), "{text}");
    }

    #[test]
    fn split_comment_panel_with_text_past_its_pane_keeps_its_scrollbar() {
        let body = (1..=40)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let text = split_screen_with_body(&body);

        assert!(text.contains('▲'), "{text}");
    }

    #[test]
    fn split_comment_panel_counts_wrapped_rows_for_its_scrollbar() {
        // One logical line that wraps past the pane still overflows it.
        let text = split_screen_with_body(&"word ".repeat(200));

        assert!(text.contains('▲'), "{text}");
    }
}
