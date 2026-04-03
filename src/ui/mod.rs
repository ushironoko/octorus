mod ai_rally;
mod checks_list;
mod cockpit;
mod comment_list;
pub(crate) mod common;
pub mod diff_view;
mod file_list;
pub(super) mod footer;
mod git_ops;
mod help;
mod issue_comment_list;
mod issue_detail;
mod issue_list;
mod pr_description;
mod pr_list;
mod simulate_modal;
mod split_view;
pub mod text_area;

use anyhow::Result;
use crossterm::{
    event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::app::{App, AppState, DataState, ShellCommandResult, ShellPhase};

static KITTY_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
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
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    cleanup_keyboard_enhancement();
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
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
        AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff => {
            git_ops::render(frame, app)
        }
        AppState::Cockpit => cockpit::render(frame, app),
    }

    // GitOps シミュレーションモーダル
    if matches!(app.state, AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff) {
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

    if let Some(ref popup) = app.symbol_popup {
        render_symbol_popup(frame, popup);
    }

    if let Some(ref shell) = app.shell_state {
        match &shell.phase {
            ShellPhase::Input => {} // Handled by build_footer_line + build_footer_block_with_border
            ShellPhase::Running => render_shell_running_indicator(frame, app, false),
            ShellPhase::Cancelling => render_shell_running_indicator(frame, app, true),
            ShellPhase::Done(result) => {
                render_shell_output_popup(frame, result, shell.scroll_offset)
            }
        }
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn render_symbol_popup(frame: &mut Frame, popup: &crate::app::SymbolPopupState) {
    let area = frame.area();

    let max_width = popup
        .symbols
        .iter()
        .map(|(name, _, _)| name.len())
        .max()
        .unwrap_or(10) as u16
        + 6; // padding + borders
    let height = (popup.symbols.len() as u16 + 2).min(area.height.saturating_sub(4)); // +2 for borders
    let width = max_width.max(20).min(area.width.saturating_sub(4));

    let popup_area = centered_rect(width, height, area);

    frame.render_widget(Clear, popup_area);

    let items: Vec<ListItem> = popup
        .symbols
        .iter()
        .enumerate()
        .map(|(i, (name, _, _))| {
            let style = if i == popup.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(format!("  {}  ", name), style)))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Select symbol (j/k/↑↓: move, Enter: jump, Esc: cancel)")
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(list, popup_area);
}

fn render_shell_running_indicator(frame: &mut Frame, app: &App, cancelling: bool) {
    let area = frame.area();
    let width = 40u16.min(area.width.saturating_sub(4));
    let height = 3u16;
    let popup_area = centered_rect(width, height, area);

    frame.render_widget(Clear, popup_area);

    let (text, color) = if cancelling {
        (
            format!("{} Cancelling...", app.spinner_char()),
            Color::Red,
        )
    } else {
        (
            format!("{} Running... (Ctrl+C: cancel)", app.spinner_char()),
            Color::Yellow,
        )
    };
    let paragraph = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(color),
    )))
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
) {
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
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll).position(clamped_scroll);
        frame.render_stateful_widget(
            scrollbar,
            popup_area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}
