use std::borrow::Cow;

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ai::RallyState;
use crate::app::{App, DataState};
use crate::github::CiStatus;

/// Build PR info string for header display (shared between file_list and ai_rally)
pub fn build_pr_info(app: &App) -> String {
    if app.is_local_mode() {
        let af = if app.is_local_auto_focus() { " AF" } else { "" };
        format!("[LOCAL{}] Local HEAD diff", af)
    } else {
        match &app.data_state {
            DataState::Loaded { pr, .. } => {
                format!("PR #{}: {} by @{}", pr.number, pr.title, pr.user.login)
            }
            _ => match app.pr_number {
                Some(n) => format!("PR #{}", n),
                None => "PR".to_string(),
            },
        }
    }
}

/// Build CI status span with color for header display
pub fn build_ci_status_span(app: &App) -> Span<'static> {
    match app.chk.ci_status {
        Some(CiStatus::Success) => Span::styled("  ✓ CI passed", Style::default().fg(Color::Green)),
        Some(CiStatus::Failure) => Span::styled("  ✕ CI failed", Style::default().fg(Color::Red)),
        Some(CiStatus::Pending) => {
            Span::styled("  ○ CI pending", Style::default().fg(Color::Yellow))
        }
        Some(CiStatus::None) | None => Span::raw(""),
    }
}

/// Render rally status bar for background rally indication
pub fn render_rally_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let Some(rally_state) = &app.ai_rally_state else {
        return;
    };

    let (text, color) = match rally_state.state {
        RallyState::Initializing => ("Initializing...", Color::Blue),
        RallyState::ReviewerReviewing => ("Reviewer reviewing...", Color::Yellow),
        RallyState::RevieweeFix => ("Reviewee fixing...", Color::Cyan),
        RallyState::RevieweeProposing => ("Reviewee proposing...", Color::Cyan),
        RallyState::WaitingForClarification => ("Waiting for clarification", Color::Magenta),
        RallyState::WaitingForPermission => ("Waiting for permission", Color::Magenta),
        RallyState::WaitingForPostConfirmation => ("Waiting for post confirmation", Color::Magenta),
        RallyState::Completed => ("Completed!", Color::Green),
        RallyState::Aborted => ("Aborted - Press A to view", Color::Yellow),
        RallyState::Error => ("Error - Press A to view", Color::Red),
    };

    let status = format!(
        " [Rally: {} ({}/{})] ",
        text, rally_state.iteration, rally_state.max_iterations
    );

    let bar = Paragraph::new(status)
        .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center);
    frame.render_widget(bar, area);
}

/// Render update notification bar when a newer version is available
pub fn render_update_bar(frame: &mut Frame, area: Rect, app: &App) {
    let Some(ref version) = app.update_available else {
        return;
    };

    let text = format!(" v{} available — run `or update` to upgrade ", version);
    let bar = Paragraph::new(text)
        .style(Style::default().fg(Color::Cyan))
        .alignment(Alignment::Center);
    frame.render_widget(bar, area);
}

pub fn truncate_with_width(s: &str, max_width: usize) -> Cow<'_, str> {
    if s.width() <= max_width {
        Cow::Borrowed(s)
    } else if max_width <= 1 {
        Cow::Borrowed("…")
    } else {
        let mut width = 0;
        let truncated: String = s
            .chars()
            .take_while(|c| {
                width += UnicodeWidthChar::width(*c).unwrap_or(1);
                width < max_width
            })
            .collect();
        Cow::Owned(format!("{}…", truncated))
    }
}

/// Wrap text to fit within the specified width, handling multibyte characters.
///
/// Preserves explicit newlines in the input. Each `\n` starts a new line.
pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for ch in text.chars() {
        if ch == '\n' {
            lines.push(current_line);
            current_line = String::new();
            current_width = 0;
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

pub fn render_filter_bar(frame: &mut Frame, area: Rect, filter: &crate::filter::ListFilter) {
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
    frame.render_widget(filter_bar, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::github::{Branch, PullRequest, User};

    #[test]
    fn test_build_pr_info_loaded() {
        let mut app = App::new_for_test();
        app.data_state = DataState::Loaded {
            pr: Box::new(PullRequest {
                number: 42,
                node_id: None,
                title: "Add feature X".to_string(),
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
                updated_at: "2024-01-01T00:00:00Z".to_string(),
            }),
            files: vec![],
        };
        assert_eq!(build_pr_info(&app), "PR #42: Add feature X by @alice");
    }

    #[test]
    fn test_build_pr_info_loading_with_pr_number() {
        let mut app = App::new_for_test();
        app.data_state = DataState::Loading;
        app.pr_number = Some(99);
        assert_eq!(build_pr_info(&app), "PR #99");
    }

    #[test]
    fn test_build_pr_info_loading_without_pr_number() {
        let mut app = App::new_for_test();
        app.data_state = DataState::Loading;
        app.pr_number = None;
        assert_eq!(build_pr_info(&app), "PR");
    }

    #[test]
    fn test_build_pr_info_local_mode() {
        let mut app = App::new_for_test();
        app.set_local_mode(true);
        assert_eq!(build_pr_info(&app), "[LOCAL] Local HEAD diff");
    }

    #[test]
    fn test_build_pr_info_local_mode_with_auto_focus() {
        let mut app = App::new_for_test();
        app.set_local_mode(true);
        app.set_local_auto_focus(true);
        assert_eq!(build_pr_info(&app), "[LOCAL AF] Local HEAD diff");
    }
}
