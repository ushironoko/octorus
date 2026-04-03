use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::{App, CockpitMenuItem, LoadState};

use super::footer;

const LOGO_LINES: [&str; 6] = [
    r"  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗",
    r" ██╔═══██╗ ██╔════╝ ╚══██╔══╝ ██╔═══██╗ ██╔══██╗ ██║   ██║ ██╔════╝",
    r" ██║   ██║ ██║         ██║    ██║   ██║ ██████╔╝ ██║   ██║ ███████╗",
    r" ██║   ██║ ██║         ██║    ██║   ██║ ██╔══██╗ ██║   ██║ ╚════██║",
    r" ╚██████╔╝ ╚██████╗    ██║    ╚██████╔╝ ██║  ██║ ╚██████╔╝ ███████║",
    r"  ╚═════╝   ╚═════╝    ╚═╝     ╚═════╝  ╚═╝  ╚═╝  ╚═════╝  ╚══════╝",
];

const GRADIENT_START: (u8, u8, u8) = (234, 175, 200); // #eaafc8
const GRADIENT_END: (u8, u8, u8) = (101, 78, 163); // #654ea3

fn logo_color(line_index: usize) -> Color {
    let steps = (LOGO_LINES.len() - 1) as f32;
    let t = line_index as f32 / steps;
    let r = (GRADIENT_START.0 as f32 + (GRADIENT_END.0 as f32 - GRADIENT_START.0 as f32) * t) as u8;
    let g = (GRADIENT_START.1 as f32 + (GRADIENT_END.1 as f32 - GRADIENT_START.1 as f32) * t) as u8;
    let b = (GRADIENT_START.2 as f32 + (GRADIENT_END.2 as f32 - GRADIENT_START.2 as f32) * t) as u8;
    Color::Rgb(r, g, b)
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let Some(ref cockpit) = app.cockpit_state else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(10),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);
    render_menu(frame, cockpit.selected_item, cockpit.repo_available, chunks[1]);

    let help_text = "q: quit  ?: help  r: refresh";
    let footer_line = footer::build_footer_line(app, help_text);
    let footer_block = footer::build_footer_block(app);
    let footer_widget = Paragraph::new(footer_line).block(footer_block);
    frame.render_widget(footer_widget, chunks[2]);
}

fn render_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let cockpit = app.cockpit_state.as_ref().unwrap();

    let mut lines: Vec<Line> = LOGO_LINES
        .iter()
        .enumerate()
        .map(|(i, line)| {
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(logo_color(i)),
            ))
        })
        .collect();

    lines.push(Line::raw(""));

    let mention_span = format_count_span("Issues: ", &cockpit.mentioned_issues_count, app);
    let review_span = format_count_span("  Review PRs: ", &cockpit.review_prs_count, app);

    let count_line = Line::from(vec![
        Span::raw("  "),
        mention_span.0,
        mention_span.1,
        review_span.0,
        review_span.1,
    ]);
    lines.push(count_line);

    let header = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("octorus"),
        );
    frame.render_widget(header, area);
}

fn format_count_span<'a>(
    label: &'a str,
    state: &LoadState<u32>,
    app: &App,
) -> (Span<'a>, Span<'a>) {
    let label_span = Span::styled(label, Style::default().fg(Color::DarkGray));
    let value_span = match state {
        LoadState::Loading => Span::styled(
            app.spinner_char().to_string(),
            Style::default().fg(Color::Yellow),
        ),
        LoadState::Loaded(count) => Span::styled(
            count.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        LoadState::Error(_) => Span::styled("N/A", Style::default().fg(Color::Red)),
        _ => Span::styled("-", Style::default().fg(Color::DarkGray)),
    };
    (label_span, value_span)
}

fn render_menu(
    frame: &mut Frame,
    selected: CockpitMenuItem,
    repo_available: bool,
    area: ratatui::layout::Rect,
) {
    let items: Vec<ListItem> = CockpitMenuItem::ALL
        .iter()
        .map(|item| {
            let disabled = item.requires_repo() && !repo_available;
            let style = if disabled {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let label = format!("  {:<14} {}", item.label(), item.description());
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected.index()));

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Navigation"),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::CockpitMenuItem;
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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

    fn make_cockpit_app(repo_available: bool) -> App {
        let config = crate::config::Config::default();
        App::new_cockpit("owner/repo", config, repo_available)
    }

    #[test]
    fn test_cockpit_loaded() {
        let mut app = make_cockpit_app(true);
        let cockpit = app.cockpit_state.as_mut().unwrap();
        cockpit.mentioned_issues_count = LoadState::Loaded(3);
        cockpit.review_prs_count = LoadState::Loaded(5);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │                  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗               │
        │                 ██╔═══██╗ ██╔════╝ ╚══██╔══╝ ██╔═══██╗ ██╔══██╗ ██║   ██║ ██╔════╝               │
        │                 ██║   ██║ ██║         ██║    ██║   ██║ ██████╔╝ ██║   ██║ ███████╗               │
        │                 ██║   ██║ ██║         ██║    ██║   ██║ ██╔══██╗ ██║   ██║ ╚════██║               │
        │                 ╚██████╔╝ ╚██████╗    ██║    ╚██████╔╝ ██║  ██║ ╚██████╔╝ ███████║               │
        │                  ╚═════╝   ╚═════╝    ╚═╝     ╚═════╝  ╚═╝  ╚═╝  ╚═════╝  ╚══════╝               │
        │                                                                                                  │
        │                                      Issues: 3  Review PRs: 5                                    │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Navigation────────────────────────────────────────────────────────────────────────────────────────┐
        │>   PR List        Browse pull requests                                                           │
        │    Issue List     Browse issues                                                                  │
        │    Local Diff     View local git diff                                                            │
        │    Git Ops        Git operations (stage, commit, push)                                           │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │q: quit  ?: help  r: refresh                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_cockpit_loading() {
        let mut app = make_cockpit_app(true);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │                  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗               │
        │                 ██╔═══██╗ ██╔════╝ ╚══██╔══╝ ██╔═══██╗ ██╔══██╗ ██║   ██║ ██╔════╝               │
        │                 ██║   ██║ ██║         ██║    ██║   ██║ ██████╔╝ ██║   ██║ ███████╗               │
        │                 ██║   ██║ ██║         ██║    ██║   ██║ ██╔══██╗ ██║   ██║ ╚════██║               │
        │                 ╚██████╔╝ ╚██████╗    ██║    ╚██████╔╝ ██║  ██║ ╚██████╔╝ ███████║               │
        │                  ╚═════╝   ╚═════╝    ╚═╝     ╚═════╝  ╚═╝  ╚═╝  ╚═════╝  ╚══════╝               │
        │                                                                                                  │
        │                                      Issues: ⠋  Review PRs: ⠋                                    │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Navigation────────────────────────────────────────────────────────────────────────────────────────┐
        │>   PR List        Browse pull requests                                                           │
        │    Issue List     Browse issues                                                                  │
        │    Local Diff     View local git diff                                                            │
        │    Git Ops        Git operations (stage, commit, push)                                           │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │q: quit  ?: help  r: refresh                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_cockpit_local_only() {
        let mut app = make_cockpit_app(false);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │                  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗               │
        │                 ██╔═══██╗ ██╔════╝ ╚══██╔══╝ ██╔═══██╗ ██╔══██╗ ██║   ██║ ██╔════╝               │
        │                 ██║   ██║ ██║         ██║    ██║   ██║ ██████╔╝ ██║   ██║ ███████╗               │
        │                 ██║   ██║ ██║         ██║    ██║   ██║ ██╔══██╗ ██║   ██║ ╚════██║               │
        │                 ╚██████╔╝ ╚██████╗    ██║    ╚██████╔╝ ██║  ██║ ╚██████╔╝ ███████║               │
        │                  ╚═════╝   ╚═════╝    ╚═╝     ╚═════╝  ╚═╝  ╚═╝  ╚═════╝  ╚══════╝               │
        │                                                                                                  │
        │                                      Issues: -  Review PRs: -                                    │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Navigation────────────────────────────────────────────────────────────────────────────────────────┐
        │>   PR List        Browse pull requests                                                           │
        │    Issue List     Browse issues                                                                  │
        │    Local Diff     View local git diff                                                            │
        │    Git Ops        Git operations (stage, commit, push)                                           │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │q: quit  ?: help  r: refresh                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_cockpit_menu_selection_git_ops() {
        let mut app = make_cockpit_app(true);
        let cockpit = app.cockpit_state.as_mut().unwrap();
        cockpit.selected_item = CockpitMenuItem::GitOps;
        cockpit.mentioned_issues_count = LoadState::Loaded(0);
        cockpit.review_prs_count = LoadState::Loaded(0);

        assert_snapshot!(render_full(&mut app), @"
        ┌octorus───────────────────────────────────────────────────────────────────────────────────────────┐
        │                  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗               │
        │                 ██╔═══██╗ ██╔════╝ ╚══██╔══╝ ██╔═══██╗ ██╔══██╗ ██║   ██║ ██╔════╝               │
        │                 ██║   ██║ ██║         ██║    ██║   ██║ ██████╔╝ ██║   ██║ ███████╗               │
        │                 ██║   ██║ ██║         ██║    ██║   ██║ ██╔══██╗ ██║   ██║ ╚════██║               │
        │                 ╚██████╔╝ ╚██████╗    ██║    ╚██████╔╝ ██║  ██║ ╚██████╔╝ ███████║               │
        │                  ╚═════╝   ╚═════╝    ╚═╝     ╚═════╝  ╚═╝  ╚═╝  ╚═════╝  ╚══════╝               │
        │                                                                                                  │
        │                                      Issues: 0  Review PRs: 0                                    │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Navigation────────────────────────────────────────────────────────────────────────────────────────┐
        │    PR List        Browse pull requests                                                           │
        │    Issue List     Browse issues                                                                  │
        │    Local Diff     View local git diff                                                            │
        │>   Git Ops        Git operations (stage, commit, push)                                           │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        │                                                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │q: quit  ?: help  r: refresh                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }
}
