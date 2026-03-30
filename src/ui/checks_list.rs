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

use super::common::truncate_with_width;
use crate::app::App;
use crate::github::CheckItem;

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
        .chk.checks_target_pr
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

    let help_text = super::footer::footer_hint_back(&app.config.keybindings);
    let footer = Paragraph::new(help_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[2]);
}

fn check_status_icon(check: &CheckItem) -> (char, Color) {
    match check.bucket.as_deref() {
        Some("pass") => ('✓', Color::Green),
        Some("fail") => ('✕', Color::Red),
        Some("pending") => ('○', Color::Yellow),
        Some("skipping") => ('-', Color::DarkGray),
        Some("cancel") => ('✕', Color::DarkGray),
        _ => {
            match check.state.as_str() {
                "SUCCESS" | "PASS" => ('✓', Color::Green),
                "FAILURE" | "FAIL" | "STARTUP_FAILURE" | "ERROR" => ('✕', Color::Red),
                "PENDING" | "QUEUED" | "IN_PROGRESS" => ('○', Color::Yellow),
                "SKIPPING" | "NEUTRAL" => ('-', Color::DarkGray),
                "CANCELLED" => ('✕', Color::DarkGray),
                _ => ('?', Color::White),
            }
        }
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
            let name_display = truncate_with_width(&check.name, name_width);
            let workflow_display = truncate_with_width(workflow, workflow_width);

            let line = Line::from(vec![
                Span::styled(format!(" {} ", icon), Style::default().fg(icon_color)),
                Span::raw(" "),
                Span::styled(
                    format!("{:<width$}", name_display, width = name_width),
                    name_style,
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{:<width$}", workflow_display, width = workflow_width),
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
    use super::*;

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
}
