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

use crate::app::App;
use crate::github::PullRequestSummary;

pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // PR list
            Constraint::Length(3), // Footer
        ])
        .split(frame.area());

    // Header
    let filter_str = app.pr_list_state_filter.display_name();
    let header_text = format!("PR List: {} ({})", app.repo, filter_str);
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL).title("octorus"));
    frame.render_widget(header, chunks[0]);

    // PR list
    if app.pr_list_loading && app.pr_list.is_none() {
        // 初回ローディング
        let loading = Paragraph::new(format!("{} Loading PRs...", app.spinner_char())).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Pull Requests"),
        );
        frame.render_widget(loading, chunks[1]);
    } else if let Some(ref prs) = app.pr_list {
        if prs.is_empty() {
            let empty = Paragraph::new("No pull requests found").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Pull Requests"),
            );
            frame.render_widget(empty, chunks[1]);
        } else {
            let total_prs = prs.len();
            let items = build_pr_list_items(prs, app.selected_pr);

            let title = if app.pr_list_loading {
                format!("Pull Requests ({}) {}", total_prs, app.spinner_char())
            } else if app.pr_list_has_more {
                format!("Pull Requests ({}+)", total_prs)
            } else {
                format!("Pull Requests ({})", total_prs)
            };

            // Use ListState for stateful rendering with automatic scroll management
            let mut list_state = ListState::default()
                .with_offset(app.pr_list_scroll_offset)
                .with_selected(Some(app.selected_pr));

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().bg(Color::DarkGray));
            frame.render_stateful_widget(list, chunks[1], &mut list_state);

            // Update scroll offset from ListState for next frame
            app.pr_list_scroll_offset = list_state.offset();

            // Scrollbar
            if total_prs > 1 {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("▲"))
                    .end_symbol(Some("▼"));

                let mut scrollbar_state =
                    ScrollbarState::new(total_prs.saturating_sub(1)).position(app.selected_pr);

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
        // pr_list が None (エラー時など)
        let empty = Paragraph::new("Failed to load pull requests").block(
            Block::default()
                .borders(Borders::ALL)
                .title("Pull Requests"),
        );
        frame.render_widget(empty, chunks[1]);
    }

    // Footer
    let footer_text =
        "j/k/↑↓: move | Enter: select | gg/G: top/bottom | o: open | c: closed | a: all | r: refresh | q: quit | ?: help";
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[2]);
}

fn build_pr_list_items(prs: &[PullRequestSummary], selected: usize) -> Vec<ListItem<'static>> {
    prs.iter()
        .enumerate()
        .map(|(i, pr)| {
            let is_selected = i == selected;

            // Draft marker
            let draft_marker = if pr.is_draft { "[DRAFT] " } else { "" };

            // PR number - yellow and bold when selected
            let number_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let number_span = Span::styled(format!("#{:<5}", pr.number), number_style);

            // Draft + Title (truncate if too long, respecting char boundaries)
            let title_width = 50;
            let full_title = format!("{}{}", draft_marker, pr.title);
            let title = truncate_string(&full_title, title_width);
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
                format!("{:<width$}", title, width = title_width),
                title_style,
            );

            // Author
            let author_span = Span::styled(
                format!("by @{}", pr.author.login),
                Style::default().fg(Color::Cyan),
            );

            // Labels (show first 2)
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

            let line = Line::from(vec![
                number_span,
                Span::raw("  "),
                title_span,
                Span::raw("  "),
                author_span,
                labels_span,
            ]);

            ListItem::new(line)
        })
        .collect()
}

/// Truncate a string to fit within a given width, respecting char boundaries
fn truncate_string(s: &str, max_width: usize) -> String {
    if s.chars().count() <= max_width {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_width.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}
