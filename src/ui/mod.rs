mod ai_rally;
mod browse;
mod checks_list;
pub(crate) mod chrome;
mod cockpit;
mod comment_list;
pub(crate) mod common;
pub mod diff_view;
mod file_list;
pub(super) mod footer;
mod git_ops;
mod help;
pub mod hit;
mod issue_comment_list;
mod issue_detail;
mod issue_list;
mod pr_description;
mod pr_list;
mod simulate_modal;
mod split_view;
pub mod text_area;
pub mod theme;

#[cfg(test)]
mod test_buffer;
#[cfg(test)]
mod toast_footer_tests;

use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Margin, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use unicode_width::UnicodeWidthStr;

use crate::app::{App, AppState, DataState, ShellCommandResult, ShellPhase};
use common::truncate_with_width;

static KITTY_ENABLED: AtomicBool = AtomicBool::new(false);
static MOUSE_CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn setup_terminal(mouse_capture: bool) -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Enable Kitty keyboard protocol for Shift+Enter detection.
    // Must be AFTER EnterAlternateScreen — some terminals reset keyboard
    // enhancement flags on screen switch.
    // Only DISAMBIGUATE_ESCAPE_CODES is needed; REPORT_EVENT_TYPES is omitted
    // to avoid affecting existing key handling.
    if execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok()
    {
        KITTY_ENABLED.store(true, Ordering::SeqCst);
    }
    if mouse_capture {
        write_enable_mouse_capture(&mut stdout)?;
        MOUSE_CAPTURE_ENABLED.store(true, Ordering::SeqCst);
    }
    let backend = CrosstermBackend::new(stdout);
    match Terminal::new(backend) {
        Ok(terminal) => Ok(terminal),
        Err(e) => {
            cleanup_mouse_capture();
            Err(e.into())
        }
    }
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    cleanup_mouse_capture();
    cleanup_keyboard_enhancement();
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

pub fn write_enable_mouse_capture(w: &mut impl io::Write) -> io::Result<()> {
    execute!(w, EnableMouseCapture)
}

pub fn write_disable_mouse_capture(w: &mut impl io::Write) -> io::Result<()> {
    execute!(w, DisableMouseCapture)
}

/// ランタイムトグルでマウスキャプチャを有効化する。
/// エスケープ発行が成功したときだけ状態を更新する。
pub fn enable_mouse_capture_runtime() -> io::Result<()> {
    write_enable_mouse_capture(&mut io::stdout())?;
    MOUSE_CAPTURE_ENABLED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Disable mouse capture if previously enabled.
/// Uses CAS to prevent double-disable. Safe to call multiple times.
pub fn cleanup_mouse_capture() {
    if MOUSE_CAPTURE_ENABLED
        .compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed)
        .is_ok()
    {
        let _ = write_disable_mouse_capture(&mut io::stdout());
    }
}

/// Pop Kitty keyboard enhancement flags if previously pushed.
/// Uses CAS to prevent double-pop. Safe to call multiple times.
pub fn cleanup_keyboard_enhancement() {
    if KITTY_ENABLED
        .compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed)
        .is_ok()
    {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
}

pub fn render(frame: &mut Frame, app: &mut App) {
    app.hit_map.clear();
    if !app.state.is_data_state_independent() {
        if matches!(app.data_state, DataState::Loading) {
            file_list::render_loading(frame, app);
            return;
        }
        if let DataState::Error(ref msg) = app.data_state {
            file_list::render_error(frame, app, msg);
            return;
        }
    }

    match app.state {
        AppState::PullRequestList => pr_list::render(frame, app),
        AppState::FileList => file_list::render(frame, app),
        AppState::DiffView => diff_view::render(frame, app),
        AppState::TextInput => diff_view::render_text_input(frame, app),
        AppState::CommentList => comment_list::render(frame, app),
        AppState::Help => help::render(frame, app),
        AppState::AiRally => ai_rally::render(frame, app),
        AppState::SplitViewFileList | AppState::SplitViewDiff => split_view::render(frame, app),
        AppState::PrDescription => pr_description::render(frame, app),
        AppState::ChecksList => checks_list::render(frame, app),
        AppState::IssueList => issue_list::render(frame, app),
        AppState::IssueDetail => issue_detail::render(frame, app),
        AppState::IssueCommentList => issue_comment_list::render(frame, app),
        AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff => git_ops::render(frame, app),
        AppState::Cockpit => cockpit::render(frame, app),
        AppState::RepoBrowseTree | AppState::RepoBrowseFile | AppState::RepoBrowseGraph => {
            browse::render(frame, app)
        }
    }

    // GitOps シミュレーションモーダル
    if matches!(
        app.state,
        AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff
    ) {
        if let Some(ref ops) = app.git_ops_state {
            match &ops.pending_confirm {
                Some(crate::app::PendingGitOpsConfirm::Simulating { .. }) => {
                    simulate_modal::render_simulating(frame, app);
                }
                Some(crate::app::PendingGitOpsConfirm::Previewing { .. }) => {
                    simulate_modal::render_preview(frame, app);
                }
                _ => {}
            }
        }
    }
    simulate_modal::register_hit_regions(app, frame.area());

    if app.symbol_popup.is_some() {
        render_symbol_popup(frame, app);
    }

    let shell_popup = if let Some(ref shell) = app.shell_state {
        match &shell.phase {
            ShellPhase::Input => Some(None),
            ShellPhase::Running => {
                render_shell_running_indicator(frame, app, false);
                Some(None)
            }
            ShellPhase::Cancelling => {
                render_shell_running_indicator(frame, app, true);
                Some(None)
            }
            ShellPhase::Done(result) => Some(Some(render_shell_output_popup(
                frame,
                result,
                shell.scroll_offset,
            ))),
        }
    } else {
        None
    };
    match shell_popup {
        // 出力ポップアップ: 外側クリックで閉じる + 内部はホイールスクロール
        Some(Some(popup_area)) => {
            app.hit_map
                .push(frame.area(), hit::HitTarget::Backdrop { dismiss: true });
            app.hit_map.push(popup_area, hit::HitTarget::OverlaySurface);
            app.hit_map.push(
                Block::default().borders(Borders::ALL).inner(popup_area),
                hit::HitTarget::Pane {
                    pane: hit::PaneKind::ShellOutput,
                },
            );
        }
        // 入力中/実行中はクリックを吸収する(誤操作防止)
        Some(None) => {
            app.hit_map
                .push(frame.area(), hit::HitTarget::Backdrop { dismiss: false });
        }
        None => {}
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

const SYMBOL_POPUP_TITLE: &str = "Select symbol (j/k/↑↓: move, Enter: jump, Esc: cancel)";
const SYMBOL_POPUP_TITLE_SHORT: &str = "Select symbol (Enter: jump, Esc: cancel)";
const SYMBOL_POPUP_TITLE_MIN: &str = "Select symbol";
/// Blank columns after a symbol name, before the popup's right border.
const SYMBOL_NAME_PAD: &str = "  ";

/// The symbol popup title for `inner_width` columns: the first of the three titles that fits,
/// or the shortest one cut with an ellipsis when none does.
fn symbol_popup_title(inner_width: usize) -> String {
    [
        SYMBOL_POPUP_TITLE,
        SYMBOL_POPUP_TITLE_SHORT,
        SYMBOL_POPUP_TITLE_MIN,
    ]
    .into_iter()
    .find(|title| title.width() <= inner_width)
    .map(str::to_string)
    .unwrap_or_else(|| match inner_width {
        0 => String::new(),
        width => truncate_with_width(SYMBOL_POPUP_TITLE_MIN, width).into_owned(),
    })
}

/// Row text for one symbol in `text_width` columns (the row minus its selection marker).
/// The name keeps its pad when both fit; otherwise the pad goes first, and the name is cut
/// with an ellipsis only when it does not fit on its own.
fn symbol_row_text(name: &str, text_width: usize) -> String {
    if text_width == 0 {
        String::new()
    } else if name.width() + SYMBOL_NAME_PAD.width() <= text_width {
        format!("{name}{SYMBOL_NAME_PAD}")
    } else if name.width() <= text_width {
        name.to_string()
    } else {
        truncate_with_width(name, text_width).into_owned()
    }
}

/// Clears the popup plus one column on each side, so no diff text is left right beside a border.
///
/// Each row's span is widened when a wide glyph crosses its edge, so the glyph is cleared whole.
/// The terminal skips the trailing cell of a wide glyph, so a border drawn over that cell never
/// reaches the screen. The span stays off the outermost columns, which hold the pane borders.
fn clear_popup_surround(frame: &mut Frame, popup: Rect) {
    let bounds = frame.area();
    let left = popup
        .left()
        .saturating_sub(1)
        .max(bounds.left().saturating_add(1));
    let right = popup
        .right()
        .saturating_add(1)
        .min(bounds.right().saturating_sub(1));
    if left >= right {
        return;
    }
    let buf = frame.buffer_mut();
    for y in popup.top()..popup.bottom() {
        let mut span = left..right;
        if span.start > bounds.left() && buf[(span.start - 1, y)].symbol().width() == 2 {
            span.start -= 1;
        }
        if span.end < bounds.right() && buf[(span.end - 1, y)].symbol().width() == 2 {
            span.end += 1;
        }
        for x in span {
            buf[(x, y)].reset();
        }
    }
}

fn render_symbol_popup(frame: &mut Frame, app: &mut App) {
    let Some(popup) = app.symbol_popup.clone() else {
        return;
    };
    let theme = app.theme;
    let area = frame.area();

    let border = 2usize;
    let name_max = popup
        .symbols
        .iter()
        .map(|(name, _, _)| name.width())
        .max()
        .unwrap_or(10);
    let max_width = name_max + theme::SELECTION_MARKER_WIDTH + SYMBOL_NAME_PAD.len() + border;
    let width = u16::try_from(max_width.max(20))
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(4));
    let height = u16::try_from(popup.symbols.len() + border)
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(4));
    let popup_area = centered_rect(width, height, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(symbol_popup_title(usize::from(
            popup_area.width.saturating_sub(2),
        )))
        .border_style(theme::Attention::Focused.border_style(theme));
    let inner = block.inner(popup_area);
    let inner_height = usize::from(inner.height);
    let offset = browse::scroll_offset(popup.selected, popup.symbols.len(), inner_height);
    let text_width = usize::from(inner.width).saturating_sub(theme::SELECTION_MARKER_WIDTH);
    let visible: Vec<_> = popup
        .symbols
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_height)
        .map(|(index, (name, _, _))| (index, symbol_row_text(name, text_width)))
        .collect();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|(_, name)| ListItem::new(Line::from(Span::raw(name.clone()))))
        .collect();

    clear_popup_surround(frame, popup_area);

    let list = theme::Attention::Focused
        .rows(theme)
        .apply(List::new(items).block(block));
    let mut list_state =
        ListState::default().with_selected(Some(popup.selected.saturating_sub(offset)));
    frame.render_stateful_widget(list, popup_area, &mut list_state);

    app.hit_map
        .push(area, hit::HitTarget::Backdrop { dismiss: true });
    app.hit_map.push(popup_area, hit::HitTarget::OverlaySurface);
    for (visible_row, (index, _)) in visible.iter().enumerate() {
        app.hit_map.push(
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(visible_row as u16),
                width: inner.width,
                height: 1,
            },
            hit::HitTarget::ListRow {
                list: hit::ListKind::SymbolPopup,
                row: *index,
                index: *index,
            },
        );
    }
}

fn render_shell_running_indicator(frame: &mut Frame, app: &App, cancelling: bool) {
    let area = frame.area();
    let width = 40u16.min(area.width.saturating_sub(4));
    let height = 3u16;
    let popup_area = centered_rect(width, height, area);

    frame.render_widget(Clear, popup_area);

    let (text, color) = if cancelling {
        (format!("{} Cancelling...", app.spinner_char()), Color::Red)
    } else {
        (
            format!("{} Running... (Ctrl+C: cancel)", app.spinner_char()),
            Color::Yellow,
        )
    };
    let paragraph = Paragraph::new(Line::from(Span::styled(text, Style::default().fg(color))))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .title("Shell"),
        );
    frame.render_widget(paragraph, popup_area);
}

fn render_shell_output_popup(
    frame: &mut Frame,
    result: &ShellCommandResult,
    scroll_offset: usize,
) -> Rect {
    let area = frame.area();
    let width = (area.width * 80 / 100).max(40).min(area.width);
    let height = (area.height * 70 / 100).max(10).min(area.height);
    let popup_area = centered_rect(width, height, area);

    frame.render_widget(Clear, popup_area);

    let success = result.exit_code == Some(0);
    let (icon, border_color) = if success {
        ("\u{2713}", Color::Green)
    } else {
        ("\u{2717}", Color::Red)
    };

    let exit_str = result
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());
    let title = format!(" {} $ {} (exit: {}) ", icon, result.command, exit_str);

    let lines: Vec<Line> = result
        .cached_lines
        .iter()
        .map(|cl| {
            if cl.is_stderr {
                Line::from(Span::styled(
                    cl.text.clone(),
                    Style::default().fg(Color::Red),
                ))
            } else if cl.text == "(no output)" {
                Line::from(Span::styled(
                    cl.text.clone(),
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                Line::from(Span::raw(cl.text.clone()))
            }
        })
        .collect();

    let content_height = popup_area.height.saturating_sub(2) as usize;
    let max_scroll = result.total_lines.saturating_sub(content_height);
    let clamped_scroll = scroll_offset.min(max_scroll);

    let footer_text = " q/Esc: close | j/k: scroll | Ctrl-d/u: page | g/G: top/bottom ";
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(title)
        .title_bottom(Line::from(Span::styled(
            footer_text,
            Style::default().fg(Color::DarkGray),
        )));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((clamped_scroll as u16, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, popup_area);

    if max_scroll > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(clamped_scroll);
        frame.render_stateful_widget(
            scrollbar,
            popup_area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }

    popup_area
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn test_write_enable_mouse_capture_emits_tracking_sequences() {
        let mut buf: Vec<u8> = Vec::new();
        write_enable_mouse_capture(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\x1b[?1000h"), "normal tracking on: {out:?}");
        assert!(out.contains("\x1b[?1006h"), "SGR mode on: {out:?}");
    }

    #[test]
    fn test_write_disable_mouse_capture_emits_reset_sequences() {
        let mut buf: Vec<u8> = Vec::new();
        write_disable_mouse_capture(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\x1b[?1000l"), "normal tracking off: {out:?}");
        assert!(out.contains("\x1b[?1006l"), "SGR mode off: {out:?}");
    }
}

#[cfg(test)]
mod symbol_popup_tests {
    use super::*;
    use crate::app::{App, AppState, DataState, SymbolPopupState};
    use crate::config::ThemeConfig;
    use crate::github::{Branch, ChangedFile, PullRequest, User};
    use crate::ui::diff_view::build_plain_diff_cache;
    use crate::ui::test_buffer::screen_text;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::Terminal;

    const PATCH: &str =
        "@@ -1,3 +1,3 @@\n fn main() {\n-let user = build();\n+let user = build_user_name();\n }";

    /// A diff pane showing `patch`, with a symbol popup listing `names` (first one selected).
    fn popup_over_diff(names: &[&str], patch: &str) -> App {
        let mut app = App::new_for_test();
        app.state = AppState::DiffView;
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
            files: vec![ChangedFile {
                filename: "src/a.rs".to_string(),
                status: "modified".to_string(),
                additions: 1,
                deletions: 1,
                patch: Some(patch.to_string()),
                viewed: false,
            }],
        };
        app.diff_store.current = Some(build_plain_diff_cache(patch, 4));
        app.symbol_popup = Some(SymbolPopupState {
            symbols: names
                .iter()
                .map(|name| ((*name).to_string(), 0, 0))
                .collect(),
            selected: 0,
        });
        app
    }

    fn draw(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Outer rectangle of the popup. The corners are read from the top border row, just above
    /// the selection bar, so a side border broken on the bar row still reports the same box.
    fn popup_bounds(buf: &Buffer) -> Rect {
        let area = buf.area();
        let (bar_x, bar_y) = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .find(|(x, y)| buf[(*x, *y)].symbol() == "▌")
            .unwrap_or_else(|| panic!("no selection bar\n{}", screen_text(buf)));
        let top = bar_y
            .checked_sub(1)
            .unwrap_or_else(|| panic!("bar on the first row\n{}", screen_text(buf)));
        let left = (0..bar_x)
            .rev()
            .find(|x| buf[(*x, top)].symbol() == "┌")
            .unwrap_or_else(|| panic!("no top-left corner\n{}", screen_text(buf)));
        let right = (bar_x..area.width)
            .find(|x| buf[(*x, top)].symbol() == "┐")
            .unwrap_or_else(|| panic!("no top-right corner\n{}", screen_text(buf)));
        let bottom = (top..area.height)
            .find(|y| buf[(left, *y)].symbol() == "└")
            .unwrap_or_else(|| panic!("no bottom-left corner\n{}", screen_text(buf)));
        Rect::new(left, top, right - left + 1, bottom - top + 1)
    }

    /// Display text of row `y` over columns `from..to`. A wide glyph's trailing cell is skipped.
    fn cells_text(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
        let mut text = String::new();
        let mut x = from;
        while x < to {
            let symbol = buf[(x, y)].symbol();
            text.push_str(symbol);
            x += u16::try_from(symbol.width()).unwrap_or(1).max(1);
        }
        text
    }

    /// Text between the popup's borders, one entry per inner row.
    fn inner_rows(buf: &Buffer, popup: Rect) -> Vec<String> {
        (popup.top() + 1..popup.bottom() - 1)
            .map(|y| cells_text(buf, y, popup.left() + 1, popup.right() - 1))
            .collect()
    }

    /// The top border row, corners included, as display text.
    fn top_border(buf: &Buffer, popup: Rect) -> String {
        cells_text(buf, popup.top(), popup.left(), popup.right())
    }

    /// Width of the popup's inner area, which holds the title and the rows.
    fn inner_width(popup: Rect) -> usize {
        usize::from(popup.width) - 2
    }

    /// Asserts both side borders and all four corners are drawn on every popup row.
    fn assert_frame_intact(buf: &Buffer, popup: Rect) {
        let (left, right) = (popup.left(), popup.right() - 1);
        for y in popup.top()..popup.bottom() {
            let (left_glyph, right_glyph) = if y == popup.top() {
                ("┌", "┐")
            } else if y + 1 == popup.bottom() {
                ("└", "┘")
            } else {
                ("│", "│")
            };
            assert_eq!(
                buf[(left, y)].symbol(),
                left_glyph,
                "left border broken on row {y}\n{}",
                screen_text(buf)
            );
            assert_eq!(
                buf[(right, y)].symbol(),
                right_glyph,
                "right border broken on row {y}\n{}",
                screen_text(buf)
            );
        }
    }

    #[test]
    fn title_is_the_full_title_when_the_popup_fits_it() {
        let long = "x".repeat(60);
        let mut app = popup_over_diff(&[&long, "short"], PATCH);
        let buf = draw(&mut app, 120, 12);
        let popup = popup_bounds(&buf);
        let title = "Select symbol (j/k/↑↓: move, Enter: jump, Esc: cancel)";
        let fill = "─".repeat(inner_width(popup) - title.width());
        assert_eq!(
            top_border(&buf, popup),
            format!("┌{title}{fill}┐"),
            "{}",
            screen_text(&buf)
        );
    }

    #[test]
    fn title_falls_back_to_shorter_titles_then_an_ellipsis() {
        // (terminal width, longest name, expected title)
        let cases = [
            (
                60u16,
                "y".repeat(40),
                "Select symbol (Enter: jump, Esc: cancel)",
            ),
            (30, "foo".to_string(), "Select symbol"),
            (24, "foo".to_string(), "Select symbol"),
            (16, "foo".to_string(), "Select sy…"),
            (10, "foo".to_string(), "Sel…"),
        ];
        for (width, name, title) in cases {
            let mut app = popup_over_diff(&[&name, "short"], PATCH);
            let buf = draw(&mut app, width, 12);
            let popup = popup_bounds(&buf);
            assert!(
                title.width() <= inner_width(popup),
                "{width} cols: test title does not fit"
            );
            let fill = "─".repeat(inner_width(popup) - title.width());
            assert_eq!(
                top_border(&buf, popup),
                format!("┌{title}{fill}┐"),
                "{width} cols\n{}",
                screen_text(&buf)
            );
        }
    }

    #[test]
    fn long_ascii_name_ends_with_an_ellipsis_and_keeps_the_frame() {
        let name = "x".repeat(29);
        let mut app = popup_over_diff(&[&name, "short"], PATCH);
        let buf = draw(&mut app, 30, 12);
        let popup = popup_bounds(&buf);
        let inner = inner_width(popup);
        let rows = inner_rows(&buf, popup);
        assert_eq!(
            rows[0],
            format!("▌ {}…", "x".repeat(inner - 3)),
            "{}",
            screen_text(&buf)
        );
        assert_eq!(rows[1], format!("  short{}", " ".repeat(inner - 7)));
        assert_frame_intact(&buf, popup);
    }

    #[test]
    fn long_fullwidth_name_is_cut_on_a_glyph_boundary_with_an_ellipsis() {
        let name = "あ".repeat(20);
        let mut app = popup_over_diff(&[&name, "short"], PATCH);
        let buf = draw(&mut app, 30, 12);
        let popup = popup_bounds(&buf);
        let inner = inner_width(popup);
        // Ten glyphs (20 columns) and "…" fill 21 of the 22 text columns; the spare column stays blank.
        let rows = inner_rows(&buf, popup);
        assert_eq!(
            rows[0],
            format!("▌ {}… ", "あ".repeat(10)),
            "{}",
            screen_text(&buf)
        );
        assert_eq!(rows[0].width(), inner);
        assert_frame_intact(&buf, popup);
    }

    #[test]
    fn tiny_areas_draw_the_popup_without_panicking() {
        for (width, height) in [
            (1u16, 1u16),
            (2, 2),
            (3, 3),
            (3, 10),
            (10, 3),
            (1, 12),
            (12, 1),
        ] {
            let mut app = popup_over_diff(&["foo", "bar"], PATCH);
            draw(&mut app, width, height);
        }
        let mut app = popup_over_diff(&[], PATCH);
        draw(&mut app, 8, 8);
    }

    /// One diff row per line that exactly fills the pane's inner width, so none word-wraps.
    fn full_width_patch(inner_width: usize) -> String {
        let row = |fill: char| format!("+{}", fill.to_string().repeat(inner_width - 1));
        format!("@@ -1 +1 @@\n{}\n{}\n{}", row('x'), row('y'), row('z'))
    }

    #[test]
    fn popup_leaves_no_diff_column_beside_its_border_at_the_width_cap() {
        // A name too long for the popup puts it at the width cap. That leaves a pane border
        // and one diff column on each side, and that column must not show.
        for (width, name_len) in [(80u16, 70usize), (60, 50)] {
            let name = "n".repeat(name_len);
            let patch = full_width_patch(usize::from(width) - 2);
            let mut app = popup_over_diff(&[&name, "short"], &patch);
            let buf = draw(&mut app, width, 12);
            let popup = popup_bounds(&buf);
            assert_eq!(popup.width, width - 4, "{width} cols: not at the width cap");
            for y in popup.top()..popup.bottom() {
                assert_eq!(
                    buf[(popup.left() - 1, y)].symbol(),
                    " ",
                    "{width} cols: diff text beside the left border on row {y}\n{}",
                    screen_text(&buf)
                );
                assert_eq!(
                    buf[(popup.right(), y)].symbol(),
                    " ",
                    "{width} cols: diff text beside the right border on row {y}\n{}",
                    screen_text(&buf)
                );
            }
            assert_frame_intact(&buf, popup);
        }
    }

    #[test]
    fn popup_clears_a_fullwidth_glyph_that_touches_either_border_whole() {
        // 80 columns with short names: a 20-column popup at columns 30..=49.
        // Row A has a glyph at 29..=30 (across the left border), row B one at 50..=51.
        let patch = format!(
            "@@ -1 +1 @@\n+{}あ{}\n+{}い{}\n+{}",
            "x".repeat(27),
            "y".repeat(48),
            "x".repeat(48),
            "z".repeat(27),
            "w".repeat(77),
        );
        let mut app = popup_over_diff(&["short", "other"], &patch);
        let buf = draw(&mut app, 80, 12);
        let popup = popup_bounds(&buf);
        assert_eq!(
            (popup.left(), popup.width),
            (30, 20),
            "geometry changed\n{}",
            screen_text(&buf)
        );
        for y in popup.top()..popup.bottom() {
            assert_eq!(
                buf[(popup.left() - 1, y)].symbol(),
                " ",
                "glyph left beside the left border on row {y}\n{}",
                screen_text(&buf)
            );
            assert_eq!(
                buf[(popup.right(), y)].symbol(),
                " ",
                "glyph left beside the right border on row {y}\n{}",
                screen_text(&buf)
            );
        }
        // Only the row with the glyph widens its gap. Row 5 keeps its diff column beyond the
        // one-column gap. Row 6 loses its glyph (columns right..=right+1) and the next diff
        // column shows again at right+2.
        let (first, second) = (popup.top() + 1, popup.top() + 2);
        assert_eq!(buf[(popup.right() + 1, first)].symbol(), "y");
        assert_eq!(buf[(popup.right(), second)].symbol(), " ");
        assert_eq!(buf[(popup.right() + 1, second)].symbol(), " ");
        assert_eq!(buf[(popup.right() + 2, second)].symbol(), "z");
        assert_frame_intact(&buf, popup);
    }

    #[test]
    fn border_uses_the_theme_accent_and_no_color_drops_its_foreground() {
        let mut app = popup_over_diff(&["foo", "bar"], PATCH);
        let themed = draw(&mut app, 80, 12);
        let popup = popup_bounds(&themed);
        assert_eq!(themed[(popup.left(), popup.top())].fg, Color::Cyan);

        app.theme = theme::Theme::resolve(&ThemeConfig::default(), true);
        let plain = draw(&mut app, 80, 12);
        let popup = popup_bounds(&plain);
        for x in popup.left()..popup.right() {
            for y in [popup.top(), popup.bottom() - 1] {
                assert_eq!(plain[(x, y)].fg, Color::Reset, "top/bottom border {x},{y}");
            }
        }
        for y in popup.top()..popup.bottom() {
            for x in [popup.left(), popup.right() - 1] {
                assert_eq!(plain[(x, y)].fg, Color::Reset, "side border {x},{y}");
            }
        }
    }

    #[test]
    fn rows_use_one_space_after_the_bar_and_the_pad_after_the_name() {
        let mut app = popup_over_diff(&["result", "shortFn"], PATCH);
        let buf = draw(&mut app, 40, 12);
        let popup = popup_bounds(&buf);
        let inner = inner_width(popup);
        let rows = inner_rows(&buf, popup);
        assert_eq!(rows[0], format!("▌ result{}", " ".repeat(inner - 8)));
        assert_eq!(rows[1], format!("  shortFn{}", " ".repeat(inner - 9)));
    }

    #[test]
    fn longest_name_keeps_a_gap_before_the_frame() {
        let name = "abcdefghijabcdefghij";
        let mut app = popup_over_diff(&[name, "short"], PATCH);
        let buf = draw(&mut app, 80, 16);
        let popup = popup_bounds(&buf);
        let inner = inner_width(popup);
        let rows = inner_rows(&buf, popup);
        assert_eq!(
            rows[0],
            format!("▌ {name}{}", " ".repeat(inner - 2 - name.len())),
            "{}",
            screen_text(&buf)
        );
    }

    #[test]
    fn fullwidth_name_uses_the_same_width_as_ascii() {
        let ascii = "a".repeat(20);
        let wide = "あ".repeat(10);
        let mut ascii_app = popup_over_diff(&[&ascii], PATCH);
        let mut wide_app = popup_over_diff(&[&wide], PATCH);
        let ascii_buf = draw(&mut ascii_app, 80, 12);
        let wide_buf = draw(&mut wide_app, 80, 12);
        assert_eq!(
            popup_bounds(&ascii_buf).width,
            popup_bounds(&wide_buf).width,
            "byte length sized the fullwidth name\nascii:\n{}\nwide:\n{}",
            screen_text(&ascii_buf),
            screen_text(&wide_buf)
        );
    }

    #[test]
    fn no_color_selection_is_the_bar() {
        let mut app = popup_over_diff(&["foo", "bar"], PATCH);
        app.theme = theme::Theme::resolve(&ThemeConfig::default(), true);
        let buf = draw(&mut app, 80, 12);
        let popup = popup_bounds(&buf);
        let bar = (popup.left()..popup.right())
            .find(|x| buf[(*x, popup.top() + 1)].symbol() == "▌")
            .expect("bar");
        let cell = &buf[(bar, popup.top() + 1)];
        assert_eq!(cell.bg, Color::Reset);
        assert_ne!(cell.fg, Color::Cyan);
        assert_ne!(cell.fg, Color::Black);
    }
}
