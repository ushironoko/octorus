use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, CockpitMenuItem, LoadState};
use crate::ui::hit::{HitTarget, ListKind, PaneKind};

use super::chrome::{pane_block, PaneTitle};
use super::footer;

const LOGO_LINES: [&str; 6] = [
    r"  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗",
    r" ██╔═══██╗ ██╔════╝ ╚══██╔══╝ ██╔═══██╗ ██╔══██╗ ██║   ██║ ██╔════╝",
    r" ██║   ██║ ██║         ██║    ██║   ██║ ██████╔╝ ██║   ██║ ███████╗",
    r" ██║   ██║ ██║         ██║    ██║   ██║ ██╔══██╗ ██║   ██║ ╚════██║",
    r" ╚██████╔╝ ╚██████╗    ██║    ╚██████╔╝ ██║  ██║ ╚██████╔╝ ███████║",
    r"  ╚═════╝   ╚═════╝    ╚═╝     ╚═════╝  ╚═╝  ╚═╝  ╚═════╝  ╚══════╝",
];

/// OCTORUS in double-line box drawing, six rows of 41 columns. The header draws it
/// when `LOGO_LINES` is wider than the terminal allows.
const COMPACT_LOGO_LINES: [&str; 6] = [
    "╔═══╗ ╔════ ══╦══ ╔═══╗ ╔═══╗ ║   ║ ╔═══╗",
    "║   ║ ║       ║   ║   ║ ║   ║ ║   ║ ║    ",
    "║   ║ ║       ║   ║   ║ ╠═══╝ ║   ║ ╚═══╗",
    "║   ║ ║       ║   ║   ║ ║   ║ ║   ║     ║",
    "║   ║ ║       ║   ║   ║ ║   ║ ║   ║     ║",
    "╚═══╝ ╚════   ║   ╚═══╝ ╚   ╝ ╚═══╝ ╚═══╝",
];

/// The word the header falls back to when even `COMPACT_LOGO_LINES` is too wide.
const TEXT_MARK: &str = "OCTORUS";

const GRADIENT_START: (u8, u8, u8) = (234, 175, 200); // #eaafc8
const GRADIENT_END: (u8, u8, u8) = (101, 78, 163); // #654ea3

/// The header gradient at `t`, from 0.0 (the start colour) to 1.0 (the end colour).
fn gradient_at(t: f32) -> Color {
    let mix = |from: u8, to: u8| (from as f32 + (to as f32 - from as f32) * t) as u8;
    Color::Rgb(
        mix(GRADIENT_START.0, GRADIENT_END.0),
        mix(GRADIENT_START.1, GRADIENT_END.1),
        mix(GRADIENT_START.2, GRADIENT_END.2),
    )
}

fn logo_color(line_index: usize) -> Color {
    let steps = (LOGO_LINES.len() - 1) as f32;
    gradient_at(line_index as f32 / steps)
}

/// Which mark the header draws. The widest one that fits wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderMark {
    /// `LOGO_LINES`, 67 columns.
    Full,
    /// `COMPACT_LOGO_LINES`, 41 columns.
    Compact,
    /// `TEXT_MARK` as plain text, cut to the width.
    Text,
}

/// The mark for a header whose content area is `inner_width` columns wide.
fn select_header_mark(inner_width: usize) -> HeaderMark {
    if logo_width(&LOGO_LINES) <= inner_width {
        HeaderMark::Full
    } else if logo_width(&COMPACT_LOGO_LINES) <= inner_width {
        HeaderMark::Compact
    } else {
        HeaderMark::Text
    }
}

/// The display width of the widest line in `lines`.
fn logo_width(lines: &[&str]) -> usize {
    lines.iter().map(|line| line.width()).max().unwrap_or(0)
}

/// The header rows for `mark`. Every mark takes the same six rows, so the counts
/// line stays in place, and no row is wider than `inner_width`.
fn mark_lines(mark: HeaderMark, inner_width: usize) -> Vec<Line<'static>> {
    match mark {
        HeaderMark::Full => gradient_lines(&LOGO_LINES),
        HeaderMark::Compact => gradient_lines(&COMPACT_LOGO_LINES),
        HeaderMark::Text => text_mark_lines(inner_width),
    }
}

fn gradient_lines(rows: &[&'static str]) -> Vec<Line<'static>> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| Line::from(Span::styled(*row, Style::default().fg(logo_color(i)))))
        .collect()
}

/// `TEXT_MARK` cut to `inner_width` columns, with a gradient across its letters. It
/// sits in the middle row of the six, so the word is centred vertically.
fn text_mark_lines(inner_width: usize) -> Vec<Line<'static>> {
    let shown = truncate_to_width(TEXT_MARK, inner_width);
    let last = (TEXT_MARK.chars().count().max(2) - 1) as f32;
    let letters: Vec<Span<'static>> = shown
        .char_indices()
        .enumerate()
        .map(|(pos, (start, ch))| {
            let style = Style::default()
                .fg(gradient_at(pos as f32 / last))
                .add_modifier(Modifier::BOLD);
            Span::styled(&shown[start..start + ch.len_utf8()], style)
        })
        .collect();

    let mut rows = vec![Line::raw(""); LOGO_LINES.len()];
    rows[(LOGO_LINES.len() - 1) / 2] = Line::from(letters);
    rows
}

/// The longest prefix of `text` that is at most `max_width` columns wide. It cuts
/// between chars, so a wide glyph that does not fit is dropped, never split.
fn truncate_to_width(text: &str, max_width: usize) -> &str {
    let mut used = 0;
    for (start, ch) in text.char_indices() {
        used += ch.width().unwrap_or(0);
        if used > max_width {
            return &text[..start];
        }
    }
    text
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let Some((selected, repo_available)) = app
        .cockpit_state
        .as_ref()
        .map(|cockpit| (cockpit.selected_item, cockpit.repo_available))
    else {
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
    render_menu(frame, app, selected, repo_available, chunks[1]);

    let help_text = "q: quit  ?: help  r: refresh";
    let footer_line = footer::build_footer_line(app, help_text);
    let footer_block = footer::build_footer_block(app);
    let footer_widget = Paragraph::new(footer_line).block(footer_block);
    frame.render_widget(footer_widget, chunks[2]);
}

fn render_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let cockpit = app.cockpit_state.as_ref().unwrap();

    let block = Block::default().borders(Borders::ALL).title("octorus");
    let inner_width = block.inner(area).width as usize;

    let mut lines = mark_lines(select_header_mark(inner_width), inner_width);
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
        .block(block);
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
    app: &mut App,
    selected: CockpitMenuItem,
    repo_available: bool,
    area: Rect,
) {
    let theme = app.theme;
    let attention = app.sole_attention();
    let items: Vec<ListItem> = CockpitMenuItem::ALL
        .iter()
        .map(|item| {
            let disabled = item.requires_repo() && !repo_available;
            let style = if disabled {
                theme.muted.style()
            } else {
                Style::default()
            };
            let label = format!("  {:<14} {}", item.label(), item.description());
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected.index()));

    let block = pane_block(
        theme,
        PaneTitle::Labeled {
            label: "Navigation",
            detail: "",
        },
        attention,
    );
    let inner_area = block.inner(area);
    let list = attention.rows(theme).apply(List::new(items).block(block));

    frame.render_stateful_widget(list, area, &mut list_state);

    app.hit_map.push(
        inner_area,
        HitTarget::Pane {
            pane: PaneKind::List(ListKind::CockpitMenu),
        },
    );
    for (visible_row, index) in (list_state.offset()..CockpitMenuItem::ALL.len())
        .take(inner_area.height as usize)
        .enumerate()
    {
        app.hit_map.push(
            Rect {
                x: inner_area.x,
                y: inner_area.y.saturating_add(visible_row as u16),
                width: inner_area.width,
                height: 1,
            },
            HitTarget::ListRow {
                list: ListKind::CockpitMenu,
                row: index,
                index,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::CockpitMenuItem;
    use crate::ui::test_buffer::screen_text;
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    /// Draws the cockpit at `width` x `height` and returns the drawn buffer.
    fn render_at(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_full(app: &mut App) -> String {
        screen_text(&render_at(app, 100, 24))
    }

    /// The cells inside the header's border on row `y`. Spaces are kept, so the
    /// string is as wide as the header's inner area.
    fn header_row(buf: &Buffer, y: u16) -> String {
        let width = buf.area().width;
        (1..width.saturating_sub(1))
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    /// The inner rows the mark occupies, above the blank line and the counts.
    fn mark_rows(buf: &Buffer) -> Vec<String> {
        (1..=LOGO_LINES.len() as u16)
            .map(|y| header_row(buf, y))
            .collect()
    }

    /// The spaces before the first drawn cell of `row`. Spaces are one column each.
    fn leading_spaces(row: &str) -> usize {
        row.chars().take_while(|ch| *ch == ' ').count()
    }

    /// The spaces after the last drawn cell of `row`.
    fn trailing_spaces(row: &str) -> usize {
        row.chars().rev().take_while(|ch| *ch == ' ').count()
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
        ┏▶ NAVIGATION━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        ┃▌   PR List        Browse pull requests                                                           ┃
        ┃    Issue List     Browse issues                                                                  ┃
        ┃    Local Diff     View local git diff                                                            ┃
        ┃    Git Ops        Git operations (stage, commit, push)                                           ┃
        ┃    Repo Browse    Browse the repository and jump by symbol                                       ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
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
        ┏▶ NAVIGATION━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        ┃▌   PR List        Browse pull requests                                                           ┃
        ┃    Issue List     Browse issues                                                                  ┃
        ┃    Local Diff     View local git diff                                                            ┃
        ┃    Git Ops        Git operations (stage, commit, push)                                           ┃
        ┃    Repo Browse    Browse the repository and jump by symbol                                       ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
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
        ┏▶ NAVIGATION━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        ┃▌   PR List        Browse pull requests                                                           ┃
        ┃    Issue List     Browse issues                                                                  ┃
        ┃    Local Diff     View local git diff                                                            ┃
        ┃    Git Ops        Git operations (stage, commit, push)                                           ┃
        ┃    Repo Browse    Browse the repository and jump by symbol                                       ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
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
        ┏▶ NAVIGATION━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        ┃    PR List        Browse pull requests                                                           ┃
        ┃    Issue List     Browse issues                                                                  ┃
        ┃    Local Diff     View local git diff                                                            ┃
        ┃▌   Git Ops        Git operations (stage, commit, push)                                           ┃
        ┃    Repo Browse    Browse the repository and jump by symbol                                       ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┃                                                                                                  ┃
        ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │q: quit  ?: help  r: refresh                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_cockpit_full_logo_is_drawn_whole_at_100_and_80_columns() {
        for width in [100, 80] {
            let mut app = make_cockpit_app(true);
            let buf = render_at(&mut app, width, 24);
            let rows: Vec<String> = mark_rows(&buf)
                .iter()
                .map(|row| row.trim().to_string())
                .collect();
            let expected: Vec<&str> = LOGO_LINES.iter().map(|line| line.trim()).collect();
            assert_eq!(rows, expected, "width {width}");
        }
    }

    #[test]
    fn test_cockpit_compact_logo_replaces_full_logo_at_60_columns() {
        let mut app = make_cockpit_app(true);
        let buf = render_at(&mut app, 60, 24);
        let rows: Vec<String> = mark_rows(&buf)
            .iter()
            .map(|row| row.trim().to_string())
            .collect();
        let expected: Vec<&str> = COMPACT_LOGO_LINES.iter().map(|line| line.trim()).collect();
        assert_eq!(rows, expected);

        let screen = screen_text(&buf);
        assert!(
            !screen.contains(LOGO_LINES[0].trim()),
            "the 67-column logo must not be drawn at 60 columns"
        );
        assert!(
            !screen.contains(TEXT_MARK),
            "the compact logo is drawn, not the text fallback"
        );
    }

    #[test]
    fn test_cockpit_compact_logo_is_centred_with_room_to_spare_at_60_columns() {
        let mut app = make_cockpit_app(true);
        let buf = render_at(&mut app, 60, 24);
        // The mark is as wide as its widest row, so its margins are the smallest
        // leading and trailing spaces over the rows.
        let rows = mark_rows(&buf);
        let left = rows.iter().map(|row| leading_spaces(row)).min().unwrap();
        let right = rows.iter().map(|row| trailing_spaces(row)).min().unwrap();
        assert!(left >= 2 && right >= 2, "left {left}, right {right}");
        assert!(left.abs_diff(right) <= 1, "left {left}, right {right}");
    }

    #[test]
    fn test_cockpit_narrow_header_centres_the_word_without_overflow() {
        for width in [30, 20, 10] {
            let mut app = make_cockpit_app(true);
            let buf = render_at(&mut app, width, 24);
            let inner = usize::from(width) - 2;
            let word_row = (1..=8)
                .map(|y| header_row(&buf, y))
                .find(|row| row.contains(TEXT_MARK))
                .unwrap_or_else(|| panic!("width {width}: the word is not drawn"));
            assert_eq!(word_row.width(), inner, "width {width}");
            assert_eq!(word_row.trim(), TEXT_MARK, "width {width}");
            let left = leading_spaces(&word_row);
            let right = trailing_spaces(&word_row);
            assert!(
                left.abs_diff(right) <= 1,
                "width {width}: left {left}, right {right}"
            );
        }
    }

    #[test]
    fn test_cockpit_tiny_header_shows_as_much_of_the_word_as_fits() {
        for width in 1..=12u16 {
            let mut app = make_cockpit_app(true);
            let buf = render_at(&mut app, width, 24);
            let inner = usize::from(width.saturating_sub(2));
            let expected: String = TEXT_MARK.chars().take(inner).collect();
            let drawn: Vec<String> = mark_rows(&buf)
                .iter()
                .map(|row| row.trim().to_string())
                .filter(|row| !row.is_empty())
                .collect();
            let expected_rows = if expected.is_empty() {
                vec![]
            } else {
                vec![expected]
            };
            assert_eq!(drawn, expected_rows, "width {width}");
        }
    }

    #[test]
    fn test_cockpit_mark_rows_keep_the_gradient_by_line_index() {
        for (width, logo) in [(100u16, &LOGO_LINES[..]), (60, &COMPACT_LOGO_LINES[..])] {
            let mut app = make_cockpit_app(true);
            let buf = render_at(&mut app, width, 24);
            for i in 0..logo.len() {
                let y = 1 + i as u16;
                let x = 1 + leading_spaces(&header_row(&buf, y)) as u16;
                assert_eq!(buf[(x, y)].fg, logo_color(i), "width {width}, row {i}");
            }
        }
    }

    #[test]
    fn test_cockpit_compact_logo_at_60_columns() {
        let mut app = make_cockpit_app(true);
        let cockpit = app.cockpit_state.as_mut().unwrap();
        cockpit.mentioned_issues_count = LoadState::Loaded(3);
        cockpit.review_prs_count = LoadState::Loaded(5);

        assert_snapshot!(screen_text(&render_at(&mut app, 60, 24)), @"
        ┌octorus───────────────────────────────────────────────────┐
        │         ╔═══╗ ╔════ ══╦══ ╔═══╗ ╔═══╗ ║   ║ ╔═══╗        │
        │         ║   ║ ║       ║   ║   ║ ║   ║ ║   ║ ║            │
        │         ║   ║ ║       ║   ║   ║ ╠═══╝ ║   ║ ╚═══╗        │
        │         ║   ║ ║       ║   ║   ║ ║   ║ ║   ║     ║        │
        │         ║   ║ ║       ║   ║   ║ ║   ║ ║   ║     ║        │
        │         ╚═══╝ ╚════   ║   ╚═══╝ ╚   ╝ ╚═══╝ ╚═══╝        │
        │                                                          │
        │                  Issues: 3  Review PRs: 5                │
        └──────────────────────────────────────────────────────────┘
        ┏▶ NAVIGATION━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        ┃▌   PR List        Browse pull requests                   ┃
        ┃    Issue List     Browse issues                          ┃
        ┃    Local Diff     View local git diff                    ┃
        ┃    Git Ops        Git operations (stage, commit, push)   ┃
        ┃    Repo Browse    Browse the repository and jump by symbo┃
        ┃                                                          ┃
        ┃                                                          ┃
        ┃                                                          ┃
        ┃                                                          ┃
        ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
        ┌──────────────────────────────────────────────────────────┐
        │q: quit  ?: help  r: refresh                              │
        └──────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_header_mark_follows_the_inner_width() {
        assert_eq!(logo_width(&LOGO_LINES), 67, "the full logo is 67 columns");
        let compact = logo_width(&COMPACT_LOGO_LINES);

        assert_eq!(select_header_mark(98), HeaderMark::Full, "100 columns");
        assert_eq!(select_header_mark(67), HeaderMark::Full);
        assert_eq!(select_header_mark(66), HeaderMark::Compact);
        assert_eq!(select_header_mark(58), HeaderMark::Compact, "60 columns");
        assert_eq!(select_header_mark(compact), HeaderMark::Compact);
        assert_eq!(select_header_mark(compact - 1), HeaderMark::Text);
        assert_eq!(select_header_mark(0), HeaderMark::Text);
    }

    #[test]
    fn test_compact_logo_is_six_equal_rows_with_room_at_60_columns() {
        let compact = logo_width(&COMPACT_LOGO_LINES);
        assert!(COMPACT_LOGO_LINES.iter().all(|row| row.width() == compact));
        assert!(
            compact + 2 <= 58,
            "two columns clear at 60 columns, got {compact}"
        );
    }

    #[test]
    fn test_truncate_to_width_cuts_between_glyphs() {
        assert_eq!(truncate_to_width("OCTORUS", 7), "OCTORUS");
        assert_eq!(truncate_to_width("OCTORUS", 4), "OCTO");
        assert_eq!(truncate_to_width("OCTORUS", 0), "");
        assert_eq!(truncate_to_width("日本語", 3), "日");
        assert_eq!(truncate_to_width("日本語", 1), "");
        assert_eq!(truncate_to_width("ab日", 3), "ab");
        assert_eq!(truncate_to_width("ab日", 4), "ab日");
    }

    #[test]
    fn test_every_mark_line_fits_the_inner_width() {
        for inner in 0..=80 {
            for line in mark_lines(select_header_mark(inner), inner) {
                assert!(line.width() <= inner, "inner {inner}: {line:?}");
            }
        }
    }
}
