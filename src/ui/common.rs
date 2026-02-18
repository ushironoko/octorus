use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    widgets::Paragraph,
    Frame,
};

use crate::ai::RallyState;
use crate::app::{App, DataState};

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

/// Render rally status bar for background rally indication
pub fn render_rally_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let Some(rally_state) = &app.ai_rally_state else {
        return;
    };

    let (text, color) = match rally_state.state {
        RallyState::Initializing => ("Initializing...", Color::Blue),
        RallyState::ReviewerReviewing => ("Reviewer reviewing...", Color::Yellow),
        RallyState::RevieweeFix => ("Reviewee fixing...", Color::Cyan),
        RallyState::WaitingForClarification => ("Waiting for clarification", Color::Magenta),
        RallyState::WaitingForPermission => ("Waiting for permission", Color::Magenta),
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
