use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, PendingGitOpsConfirm, SimulationResult};
use crate::gitfilm::GitfilmFileEntry;

fn status_color(status: &str) -> Color {
    match status {
        "modified" => Color::Yellow,
        "clean" => Color::Green,
        s if s.contains("modified") => Color::Yellow,
        s if s.contains("new file") | s.contains("staged") => Color::Green,
        s if s.contains("deleted") => Color::Red,
        s if s.contains("untracked") => Color::DarkGray,
        _ => Color::White,
    }
}

fn status_indicator(status: &str) -> &str {
    match status {
        "modified" => "M ",
        "clean" => "  ",
        s if s.contains("modified") => "M ",
        s if s.contains("new file") => "A ",
        s if s.contains("deleted") => "D ",
        s if s.contains("untracked") => "??",
        s if s.contains("staged") => "S ",
        _ => "  ",
    }
}

fn render_file_entries<'a>(entries: &'a [GitfilmFileEntry]) -> Vec<Line<'a>> {
    entries
        .iter()
        .map(|entry| {
            let indicator = status_indicator(&entry.status);
            let color = status_color(&entry.status);
            Line::from(vec![
                Span::styled(
                    format!("  {} ", indicator),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(entry.path.clone(), Style::default().fg(color)),
            ])
        })
        .collect()
}

pub fn render_simulating(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let width = 40_u16.min(area.width.saturating_sub(4));
    let height = 3_u16.min(area.height.saturating_sub(4));
    let modal_area = centered_rect(width, height, area);

    frame.render_widget(Clear, modal_area);

    let spinner = app.spinner_char();
    let text = Line::from(vec![Span::styled(
        format!(" {} Simulating... (Esc: cancel)", spinner),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, modal_area);
}

pub fn render_preview(frame: &mut Frame, app: &App) {
    let Some(ref ops) = app.git_ops_state else {
        return;
    };
    let Some(PendingGitOpsConfirm::Previewing {
        ref op,
        ref result,
        scroll_offset,
    }) = ops.pending_confirm
    else {
        return;
    };

    let area = frame.area();
    let modal_width = (area.width as f32 * 0.7) as u16;
    let modal_height = (area.height as f32 * 0.6) as u16;
    let modal_area = centered_rect(
        modal_width.max(40).min(area.width.saturating_sub(4)),
        modal_height.max(10).min(area.height.saturating_sub(4)),
        area,
    );

    frame.render_widget(Clear, modal_area);

    let SimulationResult::Success(ref preview) = result;

    let mut lines: Vec<Line> = Vec::new();

    // Before section
    lines.push(Line::from(Span::styled(
        "  Before:",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));

    if preview.before.working_tree.is_empty() && preview.before.staging_area.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no files)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        if !preview.before.working_tree.is_empty() {
            lines.push(Line::from(Span::styled(
                "  Working Tree:",
                Style::default().fg(Color::DarkGray),
            )));
            lines.extend(render_file_entries(&preview.before.working_tree));
        }
        if !preview.before.staging_area.is_empty() {
            lines.push(Line::from(Span::styled(
                "  Staging Area:",
                Style::default().fg(Color::DarkGray),
            )));
            lines.extend(render_file_entries(&preview.before.staging_area));
        }
    }

    lines.push(Line::from(""));

    // After section
    lines.push(Line::from(Span::styled(
        "  After:",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));

    if preview.after.working_tree.is_empty() && preview.after.staging_area.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no files)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        if !preview.after.working_tree.is_empty() {
            lines.push(Line::from(Span::styled(
                "  Working Tree:",
                Style::default().fg(Color::DarkGray),
            )));
            lines.extend(render_file_entries(&preview.after.working_tree));
        }
        if !preview.after.staging_area.is_empty() {
            lines.push(Line::from(Span::styled(
                "  Staging Area:",
                Style::default().fg(Color::DarkGray),
            )));
            lines.extend(render_file_entries(&preview.after.staging_area));
        }
    }

    let title = format!(" Confirm: {} ", op.display_command());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(" Y: confirm | n: cancel | j/k: scroll ")
        .border_style(Style::default().fg(Color::Yellow));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset as u16, 0));

    frame.render_widget(paragraph, modal_area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
