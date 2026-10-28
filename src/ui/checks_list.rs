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

use super::common::fit_to_width;
use crate::app::App;
use crate::github::CheckItem;
use crate::ui::hit::{HitTarget, ListKind, PaneKind};

pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let pr_label = app
        .chk
        .checks_target_pr
        .map(|n| format!("PR #{}", n))
        .unwrap_or_else(|| "PR".to_string());
    let header_text = format!("CI Checks: {}", pr_label);
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    if app.chk.checks_loading {
        let loading = Paragraph::new(format!("{} Loading checks...", app.spinner_char()))
            .block(Block::default().borders(Borders::ALL).title("CI Checks"));
        frame.render_widget(loading, chunks[1]);
    } else if let Some(ref checks) = app.chk.checks {
        if checks.is_empty() {
            let empty = Paragraph::new("No CI checks found").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("CI Checks (0)"),
            );
            frame.render_widget(empty, chunks[1]);
        } else {
            let total = checks.len();
            let items = build_check_list_items(checks, app.chk.selected_check);

            let mut list_state = ListState::default()
                .with_offset(app.chk.checks_scroll_offset)
                .with_selected(Some(app.chk.selected_check));

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("CI Checks ({})", total)),
                )
                .highlight_style(Style::default().bg(Color::DarkGray));

            frame.render_stateful_widget(list, chunks[1], &mut list_state);
            app.chk.checks_scroll_offset = list_state.offset();

            if total > 1 {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("▲"))
                    .end_symbol(Some("▼"));

                let mut scrollbar_state =
                    ScrollbarState::new(total.saturating_sub(1)).position(app.chk.selected_check);

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
        let empty = Paragraph::new("Failed to load checks")
            .block(Block::default().borders(Borders::ALL).title("CI Checks"));
        frame.render_widget(empty, chunks[1]);
    }

    register_checks_list_hits(app, chunks[1]);

    let help_text = super::footer::footer_hint_back(&app.config.keybindings);
    let footer = Paragraph::new(help_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[2]);
}

fn register_checks_list_hits(app: &mut App, area: ratatui::layout::Rect) {
    let inner = Block::default().borders(Borders::ALL).inner(area);
    app.hit_map.push(
        inner,
        HitTarget::Pane {
            pane: PaneKind::List(ListKind::ChecksList),
        },
    );

    let total = if app.chk.checks_loading {
        0
    } else {
        app.chk.checks.as_ref().map_or(0, Vec::len)
    };
    let offset = app.chk.checks_scroll_offset;
    for (visible_row, index) in (offset..total).take(usize::from(inner.height)).enumerate() {
        app.hit_map.push(
            ratatui::layout::Rect {
                x: inner.x,
                y: inner.y.saturating_add(visible_row as u16),
                width: inner.width,
                height: 1,
            },
            HitTarget::ListRow {
                list: ListKind::ChecksList,
                row: index,
                index,
            },
        );
    }
}

fn check_status_icon(check: &CheckItem) -> (char, Color) {
    match check.bucket.as_deref() {
        Some("pass") => ('✓', Color::Green),
        Some("fail") => ('✕', Color::Red),
        Some("pending") => ('○', Color::Yellow),
        Some("skipping") => ('-', Color::DarkGray),
        Some("cancel") => ('✕', Color::DarkGray),
        _ => match check.state.as_str() {
            "SUCCESS" | "PASS" => ('✓', Color::Green),
            "FAILURE" | "FAIL" | "STARTUP_FAILURE" | "ERROR" => ('✕', Color::Red),
            "PENDING" | "QUEUED" | "IN_PROGRESS" => ('○', Color::Yellow),
            "SKIPPING" | "NEUTRAL" => ('-', Color::DarkGray),
            "CANCELLED" => ('✕', Color::DarkGray),
            _ => ('?', Color::White),
        },
    }
}

fn format_duration(started: &Option<String>, completed: &Option<String>) -> String {
    let (Some(started), Some(completed)) = (started.as_deref(), completed.as_deref()) else {
        return "-".to_string();
    };

    let Ok(start) = chrono::DateTime::parse_from_rfc3339(started) else {
        return "-".to_string();
    };
    let Ok(end) = chrono::DateTime::parse_from_rfc3339(completed) else {
        return "-".to_string();
    };

    let duration = end.signed_duration_since(start);
    let secs = duration.num_seconds();
    if secs < 0 {
        return "-".to_string();
    }
    if secs < 60 {
        format!("{}s", secs)
    } else {
        let mins = secs / 60;
        let remaining_secs = secs % 60;
        format!("{}m {:02}s", mins, remaining_secs)
    }
}

fn build_check_list_items(checks: &[CheckItem], selected: usize) -> Vec<ListItem<'static>> {
    checks
        .iter()
        .enumerate()
        .map(|(i, check)| {
            let is_selected = i == selected;
            let (icon, icon_color) = check_status_icon(check);
            let duration = format_duration(&check.started_at, &check.completed_at);

            let name_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let workflow = if check.workflow.is_empty() {
                "-"
            } else {
                &check.workflow
            };

            let name_width = 30;
            let workflow_width = 15;

            let line = Line::from(vec![
                Span::styled(format!(" {} ", icon), Style::default().fg(icon_color)),
                Span::raw(" "),
                Span::styled(
                    fit_to_width(&check.name, name_width).into_owned(),
                    name_style,
                ),
                Span::raw("  "),
                Span::styled(
                    fit_to_width(workflow, workflow_width).into_owned(),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw("  "),
                Span::styled(duration, Style::default().fg(Color::DarkGray)),
            ]);

            ListItem::new(line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_buffer::{find_column, screen_text};
    use super::*;
    use crate::app::AppState;
    use crate::ui::common::truncate_with_width;
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    #[test]
    fn test_check_status_icon_by_bucket() {
        let check = CheckItem {
            name: "test".to_string(),
            state: String::new(),
            bucket: Some("pass".to_string()),
            link: None,
            workflow: String::new(),
            description: None,
            started_at: None,
            completed_at: None,
        };
        let (icon, color) = check_status_icon(&check);
        assert_eq!(icon, '✓');
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn test_check_status_icon_fallback_to_state() {
        let check = CheckItem {
            name: "test".to_string(),
            state: "FAILURE".to_string(),
            bucket: None,
            link: None,
            workflow: String::new(),
            description: None,
            started_at: None,
            completed_at: None,
        };
        let (icon, color) = check_status_icon(&check);
        assert_eq!(icon, '✕');
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn test_format_duration_valid() {
        let started = Some("2024-01-01T00:00:00Z".to_string());
        let completed = Some("2024-01-01T00:03:12Z".to_string());
        assert_eq!(format_duration(&started, &completed), "3m 12s");
    }

    #[test]
    fn test_format_duration_seconds_only() {
        let started = Some("2024-01-01T00:00:00Z".to_string());
        let completed = Some("2024-01-01T00:00:45Z".to_string());
        assert_eq!(format_duration(&started, &completed), "45s");
    }

    #[test]
    fn test_format_duration_none() {
        assert_eq!(format_duration(&None, &None), "-");
        assert_eq!(
            format_duration(&Some("2024-01-01T00:00:00Z".to_string()), &None),
            "-"
        );
    }

    #[test]
    fn test_truncate_with_width_short() {
        assert_eq!(truncate_with_width("hello", 10).as_ref(), "hello");
    }

    #[test]
    fn test_truncate_with_width_long() {
        let result = truncate_with_width("a very long string that needs truncation", 15);
        assert!(result.ends_with('…'));
        assert!(result.len() <= 20);
    }

    fn sample_check(name: &str, workflow: &str) -> CheckItem {
        CheckItem {
            name: name.to_string(),
            state: String::new(),
            bucket: Some("pass".to_string()),
            link: None,
            workflow: workflow.to_string(),
            description: None,
            started_at: None,
            completed_at: None,
        }
    }

    fn timed_check(name: &str, workflow: &str, completed_at: &str) -> CheckItem {
        let mut check = sample_check(name, workflow);
        check.started_at = Some("2024-01-01T00:00:00Z".to_string());
        check.completed_at = Some(completed_at.to_string());
        check
    }

    fn render_checks(checks: Vec<CheckItem>) -> Buffer {
        let mut app = App::new_for_test();
        app.state = AppState::ChecksList;
        app.chk.checks_loading = false;
        app.chk.checks_target_pr = Some(42);
        app.chk.checks = Some(checks);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &mut app);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn workflow_columns(buf: &Buffer, markers: &[&str]) -> Vec<Option<u16>> {
        let mut cols = Vec::new();
        for marker in markers {
            let mut found = None;
            for y in 0..buf.area().height {
                if let Some(x) = find_column(buf, y, marker) {
                    found = Some(x);
                    break;
                }
            }
            cols.push(found);
        }
        cols
    }

    #[test]
    fn ascii_check_names_share_one_workflow_column() {
        let buf = render_checks(vec![
            sample_check("lint", "flow-short"),
            sample_check("integration-tests", "flow-long"),
        ]);
        let cols = workflow_columns(&buf, &["flow-short", "flow-long"]);
        let rendered = screen_text(&buf);
        assert_eq!(
            cols[0], cols[1],
            "ascii check names disagree on the workflow column. columns={cols:?}\n{rendered}"
        );
    }

    #[test]
    fn fullwidth_check_name_aligns_workflow_with_ascii() {
        let buf = render_checks(vec![
            sample_check("lint", "flow-ascii"),
            sample_check("単体テスト", "flow-wide"),
            sample_check(&"あ".repeat(40), "flow-trunc"),
        ]);
        let cols = workflow_columns(&buf, &["flow-ascii", "flow-wide", "flow-trunc"]);
        let rendered = screen_text(&buf);
        assert!(
            cols.iter().all(Option::is_some),
            "workflow marker missing. columns={cols:?}\n{rendered}"
        );
        assert_eq!(
            cols[1], cols[0],
            "fullwidth check name shifted the workflow column. columns={cols:?}\n{rendered}"
        );
        assert_eq!(
            cols[2], cols[0],
            "truncated fullwidth check name shifted the workflow column. columns={cols:?}\n{rendered}"
        );
    }

    #[test]
    fn fullwidth_workflow_aligns_duration_with_ascii() {
        let buf = render_checks(vec![
            timed_check("lint", "ci", "2024-01-01T00:00:45Z"),
            timed_check("lint", "ビルド", "2024-01-01T00:03:12Z"),
        ]);
        let cols = workflow_columns(&buf, &["45s", "3m 12s"]);
        let rendered = screen_text(&buf);
        assert_eq!(
            cols[0], cols[1],
            "fullwidth workflow shifted the duration column. columns={cols:?}\n{rendered}"
        );
    }

    #[test]
    fn ascii_checks_list_screen_snapshot() {
        let buf = render_checks(vec![
            sample_check("lint", "ci"),
            sample_check("integration-tests", "nightly"),
        ]);
        assert_snapshot!(screen_text(&buf), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │CI Checks: PR #42                                                                                 │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌CI Checks (2)─────────────────────────────────────────────────────────────────────────────────────┐
        │ ✓  lint                            ci               -                                            ▲
        │ ✓  integration-tests               nightly          -                                            █
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
        │? Help | ! Shell | q/Esc Back                                                                     │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }
}
