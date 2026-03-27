use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};

use super::common::{build_pr_info, truncate_with_width};
use crate::ai::{RallyState, ReviewAction, RevieweeStatus};
use crate::app::{AiRallyState, App, LogEntry, LogEventType, PauseState};

pub fn render(frame: &mut Frame, app: &mut App) {
    let pr_info = build_pr_info(app);

    let Some(rally_state) = &mut app.ai_rally_state else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(10), Constraint::Length(3)])
        .split(frame.area());

    render_header(frame, chunks[0], rally_state, &pr_info);
    render_main_content(frame, chunks[1], rally_state);
    render_status_bar(frame, chunks[2], rally_state);

    if rally_state.showing_log_detail {
        render_log_detail_modal(frame, rally_state);
    }
}

fn render_header(frame: &mut Frame, area: Rect, state: &AiRallyState, pr_info: &str) {
    let base_state_text = match state.state {
        RallyState::Initializing => "Initializing...",
        RallyState::ReviewerReviewing => "Reviewer reviewing...",
        RallyState::RevieweeFix => "Reviewee fixing...",
        RallyState::WaitingForClarification => "Waiting for clarification",
        RallyState::WaitingForPermission => "Waiting for permission",
        RallyState::WaitingForPostConfirmation => "Waiting for post confirmation",
        RallyState::Completed => "Completed!",
        RallyState::Aborted => "Aborted",
        RallyState::Error => "Error",
    };

    let state_text = match state.pause_state {
        PauseState::PauseRequested => format!("{} (Pausing...)", base_state_text),
        PauseState::Paused => format!("{} (PAUSED)", base_state_text),
        PauseState::Running => base_state_text.to_string(),
    };

    let state_color = if state.pause_state == PauseState::Paused {
        Color::Yellow
    } else {
        match state.state {
            RallyState::Initializing => Color::Blue,
            RallyState::ReviewerReviewing => Color::Yellow,
            RallyState::RevieweeFix => Color::Cyan,
            RallyState::WaitingForClarification
            | RallyState::WaitingForPermission
            | RallyState::WaitingForPostConfirmation => Color::Magenta,
            RallyState::Completed => Color::Green,
            RallyState::Aborted => Color::Yellow,
            RallyState::Error => Color::Red,
        }
    };

    let title = format!(
        " AI Rally - Iteration {}/{} ",
        state.iteration, state.max_iterations
    );

    let header = Paragraph::new(vec![
        Line::from(Span::styled(pr_info, Style::default().fg(Color::White))),
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::Gray)),
            Span::styled(
                state_text,
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(state_color)),
    );

    frame.render_widget(header, area);
}

fn render_main_content(frame: &mut Frame, area: Rect, state: &mut AiRallyState) {
    if state.pending_config_warning.is_some() {
        render_config_warning(frame, area, state);
        return;
    }

    let is_waiting = matches!(
        state.state,
        RallyState::WaitingForClarification
            | RallyState::WaitingForPermission
            | RallyState::WaitingForPostConfirmation
    );

    let chunks = if is_waiting {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(35),
                Constraint::Length(6),
                Constraint::Min(10),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ])
            .split(area)
    };

    render_history(frame, chunks[0], state);

    if is_waiting {
        render_waiting_prompt(frame, chunks[1], state);
        render_logs(frame, chunks[2], state);
    } else {
        render_logs(frame, chunks[1], state);
    }
}

fn render_config_warning(frame: &mut Frame, area: Rect, state: &AiRallyState) {
    let warnings = match &state.pending_config_warning {
        Some(w) => w,
        None => return,
    };

    let mut lines = vec![
        Line::from(vec![Span::styled(
            "Local .octorus/ overrides detected that affect AI behavior:",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
    ];

    for (key, value) in warnings {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}: ", key), Style::default().fg(Color::Red)),
            Span::styled(value.clone(), Style::default().fg(Color::White)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "These overrides could alter AI agent behavior in unexpected ways.",
        Style::default().fg(Color::Yellow),
    )]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "Press 'y' to accept and continue, 'n'/'q'/Esc to cancel",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));

    let warning = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Local Override Security Warning ")
            .border_style(Style::default().fg(Color::Yellow)),
    );

    frame.render_widget(warning, area);
}

fn render_waiting_prompt(frame: &mut Frame, area: Rect, state: &AiRallyState) {
    let (title, content, help) = match state.state {
        RallyState::WaitingForClarification => {
            let question = state
                .pending_question
                .as_deref()
                .unwrap_or("(No question provided)");
            (
                " Clarification Required ",
                format!("Question: {}", question),
                "Press 'y' to open editor and respond, 'n' to skip, 'q' to abort",
            )
        }
        RallyState::WaitingForPermission => {
            let (action, reason) = state
                .pending_permission
                .as_ref()
                .map(|p| (p.action.as_str(), p.reason.as_str()))
                .unwrap_or(("(No action)", "(No reason)"));
            (
                " Permission Required ",
                format!("Action: {}\nReason: {}", action, reason),
                "Press 'y' to approve, 'n' to deny, 'q' to abort",
            )
        }
        RallyState::WaitingForPostConfirmation => {
            if let Some(ref info) = state.pending_review_post {
                let summary = truncate_with_width(&info.summary, 120);
                (
                    " Review Post Confirmation ",
                    format!(
                        "Action: {}\nSummary: {}\nComments: {}",
                        info.action, summary, info.comment_count
                    ),
                    "Press 'y' to post to PR, 'n' to skip, 'q' to abort",
                )
            } else if let Some(ref info) = state.pending_fix_post {
                let summary = truncate_with_width(&info.summary, 120);
                let files_display = if info.files_modified.len() <= 5 {
                    info.files_modified.join(", ")
                } else {
                    let shown: Vec<&str> = info
                        .files_modified
                        .iter()
                        .take(5)
                        .map(|s| s.as_str())
                        .collect();
                    format!(
                        "{} (+{} more)",
                        shown.join(", "),
                        info.files_modified.len() - 5
                    )
                };
                (
                    " Fix Post Confirmation ",
                    format!("Summary: {}\nFiles: {}", summary, files_display),
                    "Press 'y' to post to PR, 'n' to skip, 'q' to abort",
                )
            } else {
                return;
            }
        }
        _ => return,
    };

    let lines = vec![
        Line::from(vec![Span::styled(
            content,
            Style::default().fg(Color::White),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(help, Style::default().fg(Color::Yellow))]),
    ];

    let prompt = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Magenta)),
    );

    frame.render_widget(prompt, area);
}

fn render_history(frame: &mut Frame, area: Rect, state: &AiRallyState) {
    let visible_height = area.height.saturating_sub(2) as usize;

    let items: Vec<ListItem> = state
        .history
        .iter()
        .filter_map(|event| {
            let (prefix, content, color) = match event {
                crate::ai::orchestrator::RallyEvent::IterationStarted(i) => (
                    format!("[{}]", i),
                    "Iteration started".to_string(),
                    Color::Blue,
                ),
                crate::ai::orchestrator::RallyEvent::ReviewCompleted(review) => {
                    let action_text = match review.action {
                        ReviewAction::Approve => "APPROVE",
                        ReviewAction::RequestChanges => "REQUEST_CHANGES",
                        ReviewAction::Comment => "COMMENT",
                    };
                    let color = match review.action {
                        ReviewAction::Approve => Color::Green,
                        ReviewAction::RequestChanges => Color::Red,
                        ReviewAction::Comment => Color::Yellow,
                    };
                    (
                        format!("Review: {}", action_text),
                        truncate_with_width(&review.summary, 60).into_owned(),
                        color,
                    )
                }
                crate::ai::orchestrator::RallyEvent::FixCompleted(fix) => {
                    let status_text = match fix.status {
                        RevieweeStatus::Completed => "COMPLETED",
                        RevieweeStatus::NeedsClarification => "NEEDS_CLARIFICATION",
                        RevieweeStatus::NeedsPermission => "NEEDS_PERMISSION",
                        RevieweeStatus::Error => "ERROR",
                    };
                    let color = match fix.status {
                        RevieweeStatus::Completed => Color::Green,
                        RevieweeStatus::NeedsClarification | RevieweeStatus::NeedsPermission => {
                            Color::Yellow
                        }
                        RevieweeStatus::Error => Color::Red,
                    };
                    (
                        format!("Fix: {}", status_text),
                        truncate_with_width(&fix.summary, 60).into_owned(),
                        color,
                    )
                }
                crate::ai::orchestrator::RallyEvent::ClarificationNeeded(q) => (
                    "Clarification".to_string(),
                    truncate_with_width(q, 60).into_owned(),
                    Color::Magenta,
                ),
                crate::ai::orchestrator::RallyEvent::PermissionNeeded(action, _) => (
                    "Permission".to_string(),
                    truncate_with_width(action, 60).into_owned(),
                    Color::Magenta,
                ),
                crate::ai::orchestrator::RallyEvent::Approved(summary) => (
                    "APPROVED".to_string(),
                    truncate_with_width(summary, 60).into_owned(),
                    Color::Green,
                ),
                crate::ai::orchestrator::RallyEvent::Error(e) => {
                    ("ERROR".to_string(), truncate_with_width(e, 60).into_owned(), Color::Red)
                }
                _ => return None,
            };

            Some(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{}: ", prefix),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(content, Style::default().fg(Color::White)),
            ])))
        })
        .collect();

    let total = items.len();
    let scroll_offset = total.saturating_sub(visible_height);
    let visible_items: Vec<ListItem> = items.into_iter().skip(scroll_offset).collect();

    let list = List::new(visible_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" History ")
            .border_style(Style::default().fg(Color::Gray)),
    );

    frame.render_widget(list, area);
}

fn render_logs(frame: &mut Frame, area: Rect, state: &mut AiRallyState) {
    let visible_height = area.height.saturating_sub(2) as usize;
    state.last_visible_log_height = visible_height;
    let total_logs = state.logs.len();

    let scroll_offset = if state.log_scroll_offset == 0 {
        total_logs.saturating_sub(visible_height)
    } else {
        state.log_scroll_offset
    };

    let items: Vec<ListItem> = state
        .logs
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(idx, entry)| {
            let is_selected = state.selected_log_index == Some(idx);
            format_log_entry(entry, is_selected)
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " Logs ({}/{}) [j/k/↑↓: select, Enter: detail] ",
            scroll_offset.saturating_add(visible_height).min(total_logs),
            total_logs
        ))
        .border_style(Style::default().fg(Color::Gray));

    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    let list = List::new(items);
    frame.render_widget(list, inner_area);

    if total_logs > visible_height {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state =
            ScrollbarState::new(total_logs.saturating_sub(visible_height)).position(scroll_offset);

        frame.render_stateful_widget(
            scrollbar,
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn format_log_entry(entry: &LogEntry, is_selected: bool) -> ListItem<'static> {
    let (icon, color) = match entry.event_type {
        LogEventType::Info => ("[i]", Color::Blue),
        LogEventType::Thinking => ("[~]", Color::Magenta),
        LogEventType::ToolUse => ("[>]", Color::Cyan),
        LogEventType::ToolResult => ("[+]", Color::Green),
        LogEventType::Text => ("[.]", Color::White),
        LogEventType::Review => ("[R]", Color::Yellow),
        LogEventType::Fix => ("[F]", Color::Cyan),
        LogEventType::Error => ("[!]", Color::Red),
    };

    let type_label = match entry.event_type {
        LogEventType::Info => "Info",
        LogEventType::Thinking => "Think",
        LogEventType::ToolUse => "Tool",
        LogEventType::ToolResult => "Result",
        LogEventType::Text => "Output",
        LogEventType::Review => "Review",
        LogEventType::Fix => "Fix",
        LogEventType::Error => "Error",
    };

    let selector = if is_selected { ">" } else { " " };

    let display_message = truncate_with_width(&entry.message, 80).into_owned();

    let mut item = ListItem::new(Line::from(vec![
        Span::styled(
            selector.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[{}] ", entry.timestamp),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(format!("{} ", icon), Style::default().fg(color)),
        Span::styled(
            format!("{}: ", type_label),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(display_message, Style::default().fg(Color::White)),
    ]));

    if is_selected {
        item = item.style(Style::default().bg(Color::DarkGray));
    }

    item
}

fn render_log_detail_modal(frame: &mut Frame, state: &AiRallyState) {
    let Some(selected_idx) = state.selected_log_index else {
        return;
    };

    let Some(entry) = state.logs.get(selected_idx) else {
        return;
    };

    let area = frame.area();
    let modal_width = (area.width as f32 * 0.8) as u16;
    let modal_height = (area.height as f32 * 0.6) as u16;
    let modal_x = (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = (area.height.saturating_sub(modal_height)) / 2;

    let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let (type_label, color) = match entry.event_type {
        LogEventType::Info => ("Info", Color::Blue),
        LogEventType::Thinking => ("Thinking", Color::Magenta),
        LogEventType::ToolUse => ("Tool Use", Color::Cyan),
        LogEventType::ToolResult => ("Tool Result", Color::Green),
        LogEventType::Text => ("Output", Color::White),
        LogEventType::Review => ("Review", Color::Yellow),
        LogEventType::Fix => ("Fix", Color::Cyan),
        LogEventType::Error => ("Error", Color::Red),
    };

    let title = format!(" {} - {} ", type_label, entry.timestamp);

    let content = Paragraph::new(entry.message.clone())
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_bottom(Line::from(" Press Esc/Enter/q to close ").centered())
                .border_style(Style::default().fg(color)),
        );

    frame.render_widget(content, modal_area);
}

fn render_status_bar(frame: &mut Frame, area: Rect, state: &AiRallyState) {
    let help_text = if state.pending_config_warning.is_some() {
        "y: Accept and continue | n/q: Cancel and return"
    } else if state.showing_log_detail {
        "Esc/Enter/q: Close detail"
    } else if state.pause_state == PauseState::Paused {
        "p: Resume | j/k/↑↓: select | Enter: detail | b: Background | q: Abort"
    } else if state.pause_state == PauseState::PauseRequested {
        "p: Cancel pause | j/k/↑↓: select | Enter: detail | b: Background | q: Abort"
    } else {
        match state.state {
            RallyState::WaitingForClarification => {
                "y: Open editor | n: Skip | j/k/↑↓: select | Enter: detail | q: Abort"
            }
            RallyState::WaitingForPermission => {
                "y: Approve | n: Deny | j/k/↑↓: select | Enter: detail | q: Abort"
            }
            RallyState::WaitingForPostConfirmation => {
                "y: Post to PR | n: Skip | j/k/↑↓: select | Enter: detail | q: Abort"
            }
            RallyState::Completed => "j/k/↑↓: select | Enter: detail | b: Background | q: Close",
            RallyState::Aborted => "j/k/↑↓: select | Enter: detail | b: Background | q: Close",
            RallyState::Error => {
                "r: Retry | j/k/↑↓: select | Enter: detail | b: Background | q: Close"
            }
            _ => "p: Pause | j/k/↑↓: select | Enter: detail | b: Background | q: Abort",
        }
    };

    let status_bar = Paragraph::new(Line::from(vec![Span::styled(
        help_text,
        Style::default().fg(Color::Cyan),
    )]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(status_bar, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::RallyState;
    use crate::app::{AiRallyState, App, AppState, LogEntry, LogEventType, PauseState};
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn make_rally_state() -> AiRallyState {
        AiRallyState {
            iteration: 1,
            max_iterations: 3,
            state: RallyState::Initializing,
            history: vec![],
            logs: vec![],
            log_scroll_offset: 0,
            selected_log_index: None,
            showing_log_detail: false,
            pending_question: None,
            pending_permission: None,
            pending_review_post: None,
            pending_fix_post: None,
            last_visible_log_height: 0,
            pending_config_warning: None,
            pause_state: PauseState::Running,
        }
    }

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
    fn test_initializing_state() {
        let mut app = App::new_for_test();
        app.state = AppState::AiRally;
        app.ai_rally_state = Some(make_rally_state());

        assert_snapshot!(render_full(&mut app), @"
        ┌ AI Rally - Iteration 1/3 ────────────────────────────────────────────────────────────────────────┐
        │PR #1                                                                                             │
        │Status: Initializing...                                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌ History ─────────────────────────────────────────────────────────────────────────────────────────┐
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌ Logs (0/0) [j/k/↑↓: select, Enter: detail] ──────────────────────────────────────────────────────┐
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │p: Pause | j/k/↑↓: select | Enter: detail | b: Background | q: Abort                              │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_no_rally_state() {
        let mut app = App::new_for_test();
        app.state = AppState::AiRally;
        app.ai_rally_state = None;

        let output = render_full(&mut app);
        assert!(!output.contains("AI Rally"), "should render empty when no state");
    }

    #[test]
    fn test_completed_state() {
        let mut app = App::new_for_test();
        app.state = AppState::AiRally;
        let mut rally = make_rally_state();
        rally.state = RallyState::Completed;
        rally.iteration = 3;
        app.ai_rally_state = Some(rally);

        let output = render_full(&mut app);
        assert!(output.contains("Completed!"), "should show completed status");
    }

    #[test]
    fn test_with_logs() {
        let mut app = App::new_for_test();
        app.state = AppState::AiRally;
        let mut rally = make_rally_state();
        rally.logs.push(LogEntry {
            timestamp: "12:00:00".to_string(),
            event_type: LogEventType::Info,
            message: "Rally started".to_string(),
        });
        rally.logs.push(LogEntry {
            timestamp: "12:00:01".to_string(),
            event_type: LogEventType::Thinking,
            message: "Analyzing code...".to_string(),
        });
        app.ai_rally_state = Some(rally);

        let output = render_full(&mut app);
        assert!(output.contains("Rally started"), "should show log messages");
        assert!(output.contains("Analyzing code..."), "should show thinking log");
    }

    #[test]
    fn test_paused_state() {
        let mut app = App::new_for_test();
        app.state = AppState::AiRally;
        let mut rally = make_rally_state();
        rally.state = RallyState::ReviewerReviewing;
        rally.pause_state = PauseState::Paused;
        app.ai_rally_state = Some(rally);

        let output = render_full(&mut app);
        assert!(output.contains("PAUSED"), "should show paused indicator");
    }
}
