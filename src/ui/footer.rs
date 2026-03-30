use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};

use crate::app::{App, ShellPhase};
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

/// Priority cascade:
/// 1. Shell input overlay (highest)
/// 2. Approve confirmation
/// 3. PR comment submitting
/// 4. Issue comment submitting
/// 5. Submission result
/// 6. Default help text + loading indicators
pub fn build_footer_line<'a>(app: &'a App, help_text: &'a str) -> Line<'a> {
    if let Some(ref shell) = app.shell_state {
        if matches!(shell.phase, ShellPhase::Input) {
            return render_shell_input_line(&shell.input, shell.cursor);
        }
    }
    if app.is_pending_approve_confirmation() {
        Line::from(Span::styled(
            app.approve_confirmation_footer_text(),
            Style::default().fg(Color::Yellow),
        ))
    } else if app.is_submitting_comment() || app.is_issue_comment_submitting() {
        Line::from(Span::styled(
            format!("{} Submitting...", app.spinner_char()),
            Style::default().fg(Color::Yellow),
        ))
    } else if let Some((success, message)) = &app.cmt.submission_result {
        let (icon, color) = if *success {
            ("\u{2713}", Color::Green)
        } else {
            ("\u{2717}", Color::Red)
        };
        Line::from(Span::styled(
            format!("{} {}", icon, message),
            Style::default().fg(color),
        ))
    } else {
        let mut spans = vec![Span::raw(help_text)];
        if app.cmt.comments_loading {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{} Loading comments...", app.spinner_char()),
                Style::default().fg(Color::Yellow),
            ));
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

pub fn build_footer_block(app: &App) -> Block<'static> {
    build_footer_block_with_border(app, Style::default())
}

pub fn build_footer_block_with_border(app: &App, base_style: Style) -> Block<'static> {
    let style = if app
        .shell_state
        .as_ref()
        .is_some_and(|s| matches!(s.phase, ShellPhase::Input))
    {
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
    use crate::app::App;

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

    #[test]
    fn test_success_result_shows_checkmark_only() {
        let mut app = App::new_for_test();
        app.cmt.submission_result = Some((true, "Submitted".to_string()));
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
        app.cmt.submission_result = Some((false, "Failed: network error".to_string()));
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
}
