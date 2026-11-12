use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};

use super::theme::Theme;
use crate::app::{App, ShellPhase, Toast, ToastKind};
use crate::config::KeybindingsConfig;

pub fn footer_hint_back(kb: &KeybindingsConfig) -> String {
    format!(
        "{} Help | {} Shell | {} Back",
        kb.help.display(),
        kb.shell_command.display(),
        kb.quit.display(),
    )
}

pub fn footer_hint_quit(kb: &KeybindingsConfig) -> String {
    format!(
        "{} Help | {} Shell | {} Quit",
        kb.help.display(),
        kb.shell_command.display(),
        kb.quit.display(),
    )
}

/// The footer of a list whose load failed. The retry key comes first, since it is
/// the way forward.
pub fn footer_hint_retry_quit(kb: &KeybindingsConfig) -> String {
    format!(
        "{} Retry | {} Help | {} Shell | {} Quit",
        kb.retry.display(),
        kb.help.display(),
        kb.shell_command.display(),
        kb.quit.display(),
    )
}

/// Priority cascade:
/// 1. Shell input overlay (highest, focused pane only)
/// 2. Approve confirmation
/// 3. Comment, issue comment, or review submitting
/// 4. Submission result
/// 5. PR open notice
/// 6. Default help text plus loading indicators
///
/// Whichever of these applies, the config-warning badge comes first when the config
/// loaded with warnings.
pub fn build_footer_line<'a>(app: &'a App, help_text: &'a str) -> Line<'a> {
    build_footer_line_with_focus(app, help_text, true)
}

pub fn build_footer_line_with_focus<'a>(
    app: &'a App,
    help_text: &'a str,
    is_focused: bool,
) -> Line<'a> {
    build_footer_line_with_op_toast(app, None, help_text, is_focused)
}

/// Like [`build_footer_line_with_focus`], but a git operation's notice takes the
/// help text's place. That slot comes last, so the screen's other notices still
/// come first.
pub fn build_footer_line_with_op_toast<'a>(
    app: &'a App,
    op_toast: Option<&Toast>,
    help_text: &'a str,
    is_focused: bool,
) -> Line<'a> {
    with_config_warning(
        app,
        footer_status_line(app, op_toast, help_text, is_focused),
    )
}

fn footer_status_line<'a>(
    app: &'a App,
    op_toast: Option<&Toast>,
    help_text: &'a str,
    is_focused: bool,
) -> Line<'a> {
    if is_focused {
        if let Some(line) = shell_input_line(app) {
            return line;
        }
    }
    if app.is_pending_approve_confirmation() {
        Line::from(Span::styled(
            app.approve_confirmation_footer_text(),
            Style::default().fg(Color::Yellow),
        ))
    } else if app.is_submitting_comment()
        || app.is_issue_comment_submitting()
        || app.is_submitting_review()
    {
        Line::from(Span::styled(
            format!("{} Submitting...", app.spinner_char()),
            Style::default().fg(Color::Yellow),
        ))
    } else if let Some(toast) = &app.cmt.submission_result {
        let mut spans = vec![toast_span(toast, app.theme)];
        // A reload that follows a result is still running. Its progress is shown
        // beside the result, so the wait is visible while the message is up.
        if app.cmt.comments_loading {
            spans.push(Span::raw("  "));
            spans.push(loading_comments_span(app));
        }
        Line::from(spans)
    } else if let Some(message) = app.pr_open_notice() {
        Line::from(Span::styled(message, Style::default().fg(Color::Yellow)))
    } else {
        let first = match op_toast {
            Some(toast) => toast_span(toast, app.theme),
            None => Span::raw(help_text),
        };
        let mut spans = vec![first];
        if app.cmt.comments_loading {
            spans.push(Span::raw("  "));
            spans.push(loading_comments_span(app));
        }
        if app.symbol_search.is_searching() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{} Searching...", app.spinner_char()),
                Style::default().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    }
}

/// The footer of a split-view pane that shows no status. Status messages are drawn
/// in one footer, the diff pane's, so this pane shows the shell prompt while it is
/// focused and otherwise its help text.
pub fn build_pane_help_line<'a>(app: &'a App, help_text: &'a str, is_focused: bool) -> Line<'a> {
    if is_focused {
        if let Some(line) = shell_input_line(app) {
            return line;
        }
    }
    Line::from(Span::raw(help_text))
}

/// A notice as one footer span: `✓ text` in the theme's success colour, or `✗ text`
/// in its danger colour.
fn toast_span(toast: &Toast, theme: Theme) -> Span<'static> {
    let (icon, ink) = match toast.kind() {
        ToastKind::Success => ("\u{2713}", theme.success),
        ToastKind::Failure => ("\u{2717}", theme.danger),
    };
    Span::styled(format!("{icon} {}", toast.message()), ink.style())
}

/// The shell command prompt, while the shell is open for input.
fn shell_input_line(app: &App) -> Option<Line<'static>> {
    let shell = app.shell_state.as_ref()?;
    matches!(shell.phase, ShellPhase::Input)
        .then(|| render_shell_input_line(&shell.input, shell.cursor))
}

/// The badge that names the config warnings, e.g. `1 config warning (? for details)`.
/// `None` when the config loaded cleanly.
pub(crate) fn config_warning_span(app: &App) -> Option<Span<'static>> {
    let count = app.config.load_warnings.len();
    if count == 0 {
        return None;
    }
    let noun = if count == 1 { "warning" } else { "warnings" };
    let help_key = app.config.keybindings.help.display();
    Some(Span::styled(
        format!("{count} config {noun} ({help_key} for details)"),
        app.theme.warning.style(),
    ))
}

/// Draws the config-warning badge in front of a footer line, when there are warnings.
pub(crate) fn with_config_warning<'a>(app: &App, line: Line<'a>) -> Line<'a> {
    with_badge(config_warning_span(app), line)
}

/// Puts `badge`, when there is one, in front of `line`. The hints keep their place after
/// it, so the badge takes width but never replaces them.
pub(crate) fn with_badge<'a>(badge: Option<Span<'static>>, mut line: Line<'a>) -> Line<'a> {
    if let Some(badge) = badge {
        line.spans.insert(0, Span::raw("  "));
        line.spans.insert(0, badge);
    }
    line
}

fn loading_comments_span(app: &App) -> Span<'static> {
    Span::styled(
        format!("{} Loading comments...", app.spinner_char()),
        app.theme.warning.style(),
    )
}

pub fn build_footer_block(app: &App) -> Block<'static> {
    build_footer_block_with_border(app, Style::default())
}

pub fn build_footer_block_with_border(app: &App, base_style: Style) -> Block<'static> {
    build_footer_block_with_focus(app, base_style, true)
}

pub fn build_footer_block_with_focus(
    app: &App,
    base_style: Style,
    is_focused: bool,
) -> Block<'static> {
    let shell_input_active = is_focused
        && app
            .shell_state
            .as_ref()
            .is_some_and(|s| matches!(s.phase, ShellPhase::Input));
    let style = if shell_input_active {
        Style::default().fg(Color::Cyan)
    } else if app.is_pending_approve_confirmation() {
        Style::default().fg(Color::Yellow)
    } else {
        base_style
    };
    Block::default().borders(Borders::ALL).border_style(style)
}

fn render_shell_input_line(input: &str, cursor: usize) -> Line<'static> {
    let chars: Vec<char> = input.chars().collect();
    let before: String = chars[..cursor.min(chars.len())].iter().collect();
    let cursor_char: String = if cursor < chars.len() {
        chars[cursor].to_string()
    } else {
        " ".to_string()
    };
    let after: String = if cursor < chars.len() {
        chars[cursor + 1..].iter().collect()
    } else {
        String::new()
    };

    Line::from(vec![
        Span::styled(
            "! ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(before),
        Span::styled(
            cursor_char,
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(after),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppState, PrOpenSource, ReviewAction, Toast};
    use crate::config::ThemeColors;
    use crate::keybinding::{KeyBinding, KeySequence};
    use crate::ui::test_buffer::{no_color_theme, theme_with, CUSTOM_WARNING};
    use insta::assert_snapshot;

    const HELP: &str = "j/k: move | q: quit";

    fn line_to_string(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn test_idle_shows_help_text_only() {
        let app = App::new_for_test();
        let line = build_footer_line(&app, HELP);
        let text = line_to_string(&line);
        assert_eq!(text, HELP);
        assert_eq!(line.spans.len(), 1);
    }

    #[test]
    fn test_comments_loading_appends_indicator() {
        let mut app = App::new_for_test();
        app.cmt.comments_loading = true;
        let line = build_footer_line(&app, HELP);
        let text = line_to_string(&line);
        assert!(text.starts_with(HELP));
        assert!(text.contains("Loading comments..."));
        assert_eq!(line.spans.len(), 3); // help + "  " + loading
    }

    #[test]
    fn test_submitting_shows_status_only() {
        let mut app = App::new_for_test();
        app.set_submitting_for_test(true);
        let line = build_footer_line(&app, HELP);
        let text = line_to_string(&line);
        assert!(text.contains("Submitting..."));
        assert!(!text.contains(HELP));
        assert_eq!(line.spans.len(), 1);
    }

    #[tokio::test]
    async fn test_review_submit_in_flight_renders_submitting_footer() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = App::new_for_test();
        app.pr_number = Some(1);
        app.spawn_review_submit(ReviewAction::Approve, std::future::pending());
        assert!(app.is_submitting_review());

        let line = build_footer_line(&app, HELP);
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(line, frame.area()))
            .unwrap();
        let row = crate::ui::test_buffer::row_text(terminal.backend().buffer(), 0);

        assert!(row.contains("Submitting..."));
        assert!(!row.contains(HELP));
    }

    #[test]
    fn test_success_result_shows_checkmark_only() {
        let mut app = App::new_for_test();
        app.cmt.submission_result = Some(Toast::success("Submitted".to_string()));
        let line = build_footer_line(&app, HELP);
        let text = line_to_string(&line);
        assert!(text.contains("\u{2713}"));
        assert!(text.contains("Submitted"));
        assert!(!text.contains(HELP));
        assert_eq!(line.spans.len(), 1);

        // Check color is green
        let style = line.spans[0].style;
        assert_eq!(style.fg, Some(Color::Green));
    }

    #[test]
    fn test_error_result_shows_cross_only() {
        let mut app = App::new_for_test();
        app.cmt.submission_result = Some(Toast::failure("Failed: network error".to_string()));
        let line = build_footer_line(&app, HELP);
        let text = line_to_string(&line);
        assert!(text.contains("\u{2717}"));
        assert!(text.contains("Failed: network error"));
        assert!(!text.contains(HELP));
        assert_eq!(line.spans.len(), 1);

        // Check color is red
        let style = line.spans[0].style;
        assert_eq!(style.fg, Some(Color::Red));
    }

    #[test]
    fn test_inferred_commit_pr_notice_persists_in_the_pr_footer() {
        let mut app = App::new_for_test();
        app.state = AppState::FileList;
        app.pr_number = Some(123);
        app.pr_open_source = PrOpenSource::InferredCommitSubject {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };

        let line = build_footer_line(&app, HELP);

        assert_eq!(
            line_to_string(&line),
            "PR #123 inferred from commit subject; GitHub did not confirm it"
        );
        assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn test_inferred_commit_pr_notice_does_not_replace_issue_list_footer_help() {
        let mut app = App::new_for_test();
        app.state = AppState::IssueList;
        app.pr_number = Some(123);
        app.pr_open_source = PrOpenSource::InferredCommitSubject {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };

        let line = build_footer_line(&app, HELP);

        assert_snapshot!(line_to_string(&line), @"j/k: move | q: quit");
    }

    #[test]
    fn test_pending_approve_confirmation_shows_prompt() {
        let mut app = App::new_for_test();
        app.set_pending_approve_body_for_test(Some(String::new()));

        let expected = app.approve_confirmation_footer_text();
        let line = build_footer_line(&app, HELP);
        let text = line_to_string(&line);
        assert_eq!(text, expected);
        assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn test_pending_approve_confirmation_reverts_to_help_when_cleared() {
        let mut app = App::new_for_test();
        app.set_pending_approve_body_for_test(Some(String::new()));

        let expected = app.approve_confirmation_footer_text();
        let pending_line = build_footer_line(&app, HELP);
        assert_eq!(line_to_string(&pending_line), expected);

        app.set_pending_approve_body_for_test(None);
        let normal_line = build_footer_line(&app, HELP);
        assert_eq!(line_to_string(&normal_line), HELP);
    }

    #[test]
    fn test_footer_hint_back_reflects_keybindings() {
        let kb = KeybindingsConfig::default();
        let hint = footer_hint_back(&kb);
        assert!(hint.contains("Help"));
        assert!(hint.contains("Shell"));
        assert!(hint.contains("Back"));
        assert!(hint.contains(&kb.help.display()));
        assert!(hint.contains(&kb.shell_command.display()));
        assert!(hint.contains(&kb.quit.display()));
    }

    #[test]
    fn test_footer_hint_quit_reflects_keybindings() {
        let kb = KeybindingsConfig::default();
        let hint = footer_hint_quit(&kb);
        assert!(hint.contains("Quit"));
        assert!(hint.contains(&kb.help.display()));
    }

    #[test]
    fn test_footer_hint_retry_quit_leads_with_the_retry_key() {
        let kb = KeybindingsConfig::default();
        assert_eq!(
            footer_hint_retry_quit(&kb),
            "r Retry | ? Help | ! Shell | q/Esc Quit"
        );
    }

    #[test]
    fn test_footer_hint_retry_quit_reads_the_retry_key_from_keybindings() {
        let kb = KeybindingsConfig {
            retry: crate::keybinding::KeySequence::single(crate::keybinding::KeyBinding::char('x')),
            ..KeybindingsConfig::default()
        };
        assert!(footer_hint_retry_quit(&kb).starts_with("x Retry | "));
    }

    #[test]
    fn test_shell_input_renders_prompt_with_cursor() {
        use crate::app::ShellState;

        let mut app = App::new_for_test();
        app.shell_state = Some(ShellState {
            input: "ls -la".to_string(),
            cursor: 3,
            phase: ShellPhase::Input,
            scroll_offset: 0,
        });

        let line = build_footer_line(&app, HELP);
        let text = line_to_string(&line);
        assert!(text.starts_with("! "));
        assert!(text.contains("ls "));
        // Cursor char 'l' should be present
        assert!(text.contains('l'));
    }

    #[test]
    fn test_shell_input_cyan_border() {
        use crate::app::ShellState;

        let mut app = App::new_for_test();
        app.shell_state = Some(ShellState {
            input: String::new(),
            cursor: 0,
            phase: ShellPhase::Input,
            scroll_offset: 0,
        });

        // Shell input active should produce Cyan border (not default)
        let block_with_shell = build_footer_block_with_border(&app, Style::default());
        // Normal state should produce default border
        app.shell_state = None;
        let block_normal = build_footer_block_with_border(&app, Style::default());

        // Verify they differ (Cyan vs default)
        let shell_debug = format!("{:?}", block_with_shell);
        let normal_debug = format!("{:?}", block_normal);
        assert_ne!(shell_debug, normal_debug);
    }

    /// The reload after a result is shown beside the result, so the wait is visible
    /// while the result message is up.
    #[test]
    fn test_success_result_shows_the_reload_progress_beside_it() {
        let mut app = App::new_for_test();
        app.cmt.submission_result = Some(Toast::success("Added to pending review".to_string()));
        app.cmt.comments_loading = true;

        let line = build_footer_line(&app, HELP);
        let text = line_to_string(&line);

        assert!(
            text.starts_with("\u{2713} Added to pending review"),
            "{text}"
        );
        assert!(text.contains("Loading comments..."), "{text}");
    }

    fn progress_span<'a>(line: &'a Line<'a>) -> &'a Span<'a> {
        line.spans
            .iter()
            .find(|span| span.content.contains("Loading comments..."))
            .expect("the reload progress is drawn")
    }

    #[test]
    fn reload_progress_takes_the_theme_warning_color() {
        let mut app = App::new_for_test();
        app.cmt.submission_result = Some(Toast::success("Added to pending review".to_string()));
        app.cmt.comments_loading = true;
        app.theme = theme_with(ThemeColors {
            warning: Some(CUSTOM_WARNING),
            ..ThemeColors::default()
        });

        let line = build_footer_line(&app, HELP);

        assert_eq!(progress_span(&line).style.fg, Some(CUSTOM_WARNING));
    }

    #[test]
    fn reload_progress_keeps_its_words_and_drops_its_color_under_no_color() {
        let mut app = App::new_for_test();
        app.cmt.submission_result = Some(Toast::success("Added to pending review".to_string()));
        app.cmt.comments_loading = true;
        app.theme = no_color_theme();

        let line = build_footer_line(&app, HELP);

        let progress = progress_span(&line);
        assert_eq!(progress.style.fg, None);
        assert!(progress.content.contains("Loading comments..."));
    }

    fn with_config_warnings(app: &mut App, count: usize) {
        app.config.load_warnings = (0..count)
            .map(|index| {
                format!("/c/config.toml: theme: expected a table, got integer \"{index}\" (using default)")
            })
            .collect();
    }

    #[test]
    fn config_warning_badge_leads_the_idle_footer_and_keeps_the_hints() {
        let mut app = App::new_for_test();
        with_config_warnings(&mut app, 1);

        let text = line_to_string(&build_footer_line(&app, HELP));

        assert_eq!(text, format!("1 config warning (? for details)  {HELP}"));
    }

    #[test]
    fn config_warning_badge_counts_the_warnings_and_names_the_configured_help_key() {
        let mut app = App::new_for_test();
        with_config_warnings(&mut app, 2);
        app.config.keybindings.help = KeySequence::single(KeyBinding::char('h'));

        let text = line_to_string(&build_footer_line(&app, HELP));

        assert_eq!(text, format!("2 config warnings (h for details)  {HELP}"));
    }

    #[test]
    fn config_warning_badge_stays_in_front_of_a_transient_result() {
        let mut app = App::new_for_test();
        with_config_warnings(&mut app, 1);
        app.cmt.submission_result = Some(Toast::success("Added to pending review".to_string()));

        let text = line_to_string(&build_footer_line(&app, HELP));

        assert_eq!(
            text,
            "1 config warning (? for details)  \u{2713} Added to pending review"
        );
    }

    #[test]
    fn config_warning_badge_stays_in_front_of_an_op_toast() {
        let mut app = App::new_for_test();
        with_config_warnings(&mut app, 1);
        let toast = Toast::failure("Push failed: rejected".to_string());

        let text = line_to_string(&build_footer_line_with_op_toast(
            &app,
            Some(&toast),
            HELP,
            true,
        ));

        assert_eq!(
            text,
            "1 config warning (? for details)  \u{2717} Push failed: rejected"
        );
    }
}
