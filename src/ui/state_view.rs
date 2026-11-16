//! One drawing of loading, empty, and failed panes.
//!
//! The text sits at the top left of the area it is given. Callers draw the
//! pane border and pass the inner rect. A failure's words and color come from
//! [`GhFailure`]; this module does not read stderr.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::common::truncate_with_width;
use super::theme::Theme;
use crate::app::App;
use crate::config::KeybindingsConfig;
use crate::github::{FailureSeverity, FailureSurface, GhFailure};

pub(super) enum ViewState<'a> {
    Loading {
        what: &'a str,
    },
    Empty {
        title: &'a str,
        hint: Option<&'a str>,
    },
    Error {
        title: &'a str,
        reason: String,
        actions: Vec<(String, &'a str)>,
        severity: FailureSeverity,
    },
}

pub(super) fn render_state(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ViewState<'_>,
    spinner: &str,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = state_lines(state, theme, spinner, usize::from(area.width));
    frame.render_widget(Paragraph::new(lines), area);
}

/// A bordered pane whose body is [`render_state`]. `title` is the pane title,
/// not the state's heading. `None` draws a border with no title.
pub(super) fn render_in_block(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: Option<&str>,
    state: &ViewState<'_>,
    spinner: &str,
) {
    let mut block = Block::default().borders(Borders::ALL);
    if let Some(title) = title {
        block = block.title(title.to_string());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    render_state(frame, inner, theme, state, spinner);
}

pub(super) fn retry_action(kb: &KeybindingsConfig) -> (String, &'static str) {
    (kb.retry.display_primary(), "retry")
}

pub(super) fn clear_filter_hint(kb: &KeybindingsConfig) -> String {
    format!("{}: clear filter", kb.quit.display())
}

pub(super) fn comment_empty_hint(kb: &KeybindingsConfig) -> String {
    format!(
        "{}: comment, {}: suggestion",
        kb.comment.display_primary(),
        kb.suggestion.display_primary(),
    )
}

pub(super) fn failure_symbol(severity: FailureSeverity) -> &'static str {
    match severity {
        FailureSeverity::Warning => "⚠",
        FailureSeverity::Danger => "✕",
    }
}

pub(super) fn failure_style(theme: Theme, severity: FailureSeverity) -> Style {
    match severity {
        FailureSeverity::Warning => theme.warning.style(),
        FailureSeverity::Danger => theme.danger.style(),
    }
}

fn heading_style(theme: Theme, severity: FailureSeverity) -> Style {
    match severity {
        FailureSeverity::Warning => theme.warning.bold(),
        FailureSeverity::Danger => theme.danger.bold(),
    }
}

/// The comment panel when the current line has nothing to list. An error
/// replaces the empty hint. Loading stays on the footer, which already shows it.
pub(super) fn render_comment_placeholder(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(message) = app.cmt.comment_error.as_deref() {
        render_comment_error(frame, area, app, message);
        return;
    }
    let hint = comment_empty_hint(&app.config.keybindings);
    let state = ViewState::Empty {
        title: "No comments",
        hint: Some(&hint),
    };
    render_state(frame, area, &app.theme, &state, app.spinner_char());
}

pub(super) fn render_comment_error(frame: &mut Frame, area: Rect, app: &App, message: &str) {
    let failure = GhFailure::from_message(message);
    let title = failure.title(FailureSurface::Comments);
    let state = ViewState::Error {
        title: &title,
        reason: failure.reason(),
        actions: vec![retry_action(&app.config.keybindings)],
        severity: failure.severity(),
    };
    render_state(frame, area, &app.theme, &state, app.spinner_char());
}

fn state_lines(
    state: &ViewState<'_>,
    theme: &Theme,
    spinner: &str,
    width: usize,
) -> Vec<Line<'static>> {
    match state {
        ViewState::Loading { what } => vec![bold_line(format!("{spinner} {what}"), width)],
        ViewState::Empty { title, hint } => {
            let mut lines = vec![bold_line(format!("○ {title}"), width)];
            if let Some(hint) = hint {
                lines.push(styled_line(hint.to_string(), theme.muted.style(), width));
            }
            lines
        }
        ViewState::Error {
            title,
            reason,
            actions,
            severity,
        } => {
            let mut lines = vec![styled_line(
                format!("{} {title}", failure_symbol(*severity)),
                heading_style(*theme, *severity),
                width,
            )];
            if !reason.is_empty() {
                lines.push(styled_line(reason.clone(), theme.muted.style(), width));
            }
            if !actions.is_empty() {
                let actions = actions
                    .iter()
                    .map(|(key, label)| format!("{key}: {label}"))
                    .collect::<Vec<_>>()
                    .join(" │ ");
                lines.push(styled_line(actions, Style::default(), width));
            }
            lines
        }
    }
}

fn bold_line(text: String, width: usize) -> Line<'static> {
    styled_line(text, Style::default().add_modifier(Modifier::BOLD), width)
}

fn styled_line(text: String, style: Style, width: usize) -> Line<'static> {
    let fitted = truncate_with_width(&text, width).into_owned();
    Line::from(Span::styled(fitted, style))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ThemeColors, ThemeConfig};
    use crate::github::GhFailureKind;
    use crate::keybinding::{KeyBinding, KeySequence};
    use crate::ui::test_buffer::{
        screen_text, theme_with, CUSTOM_DANGER, CUSTOM_MUTED, CUSTOM_WARNING,
    };
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    fn draw(
        state: &ViewState<'_>,
        theme: Theme,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_state(frame, frame.area(), &theme, state, "⠋"))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn loading_is_bold_at_the_top_left_and_uses_no_hardcoded_color() {
        let state = ViewState::Loading {
            what: "Loading pull requests…",
        };
        let buf = draw(&state, Theme::test_default(), 40, 3);
        let text = screen_text(&buf);
        assert!(text.starts_with("⠋ Loading pull requests…"), "{text}");
        let cell = &buf[(0, 0)];
        assert_eq!(cell.symbol(), "⠋");
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert_eq!(cell.fg, Color::Reset);
    }

    #[test]
    fn empty_draws_the_circle_and_a_muted_hint() {
        let theme = theme_with(ThemeColors {
            muted: Some(CUSTOM_MUTED),
            ..ThemeColors::default()
        });
        let state = ViewState::Empty {
            title: "No open pull requests",
            hint: Some("q/Esc: clear filter"),
        };
        let buf = draw(&state, theme, 40, 3);
        let text = screen_text(&buf);
        assert!(text.contains("○ No open pull requests"), "{text}");
        assert!(text.contains("q/Esc: clear filter"), "{text}");
        assert_eq!(buf[(0, 0)].symbol(), "○");
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(0, 1)].fg, CUSTOM_MUTED);
    }

    #[test]
    fn danger_error_is_a_bold_cross_and_a_muted_reason() {
        let theme = theme_with(ThemeColors {
            danger: Some(CUSTOM_DANGER),
            muted: Some(CUSTOM_MUTED),
            ..ThemeColors::default()
        });
        let state = ViewState::Error {
            title: "gh is not authenticated",
            reason: "run: gh auth login".to_string(),
            actions: vec![("r".to_string(), "retry")],
            severity: FailureSeverity::Danger,
        };
        let buf = draw(&state, theme, 40, 4);
        let text = screen_text(&buf);
        assert!(text.contains("✕ gh is not authenticated"), "{text}");
        assert!(text.contains("run: gh auth login"), "{text}");
        assert!(text.contains("r: retry"), "{text}");
        assert_eq!(buf[(0, 0)].fg, CUSTOM_DANGER);
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(0, 1)].fg, CUSTOM_MUTED);
        assert!(!buf[(0, 1)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn warning_error_uses_the_warning_token_and_keeps_the_symbol_without_color() {
        let theme = theme_with(ThemeColors {
            warning: Some(CUSTOM_WARNING),
            ..ThemeColors::default()
        });
        let state = ViewState::Error {
            title: "Can't reach GitHub",
            reason: "Check your network connection.".to_string(),
            actions: vec![("z".to_string(), "retry")],
            severity: FailureSeverity::Warning,
        };
        let buf = draw(&state, theme, 48, 4);
        assert_eq!(buf[(0, 0)].symbol(), "⚠");
        assert_eq!(buf[(0, 0)].fg, CUSTOM_WARNING);
        assert!(screen_text(&buf).contains("z: retry"));

        let plain = Theme::resolve(&ThemeConfig::default(), true);
        let buf = draw(&state, plain, 48, 4);
        let text = screen_text(&buf);
        assert!(text.contains('⚠'), "{text}");
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn no_color_danger_keeps_the_cross_and_bold() {
        let plain = Theme::resolve(&ThemeConfig::default(), true);
        let state = ViewState::Error {
            title: "gh is not installed",
            reason: "Install GitHub CLI (https://cli.github.com), then retry.".to_string(),
            actions: vec![("r".to_string(), "retry")],
            severity: FailureSeverity::Danger,
        };
        let buf = draw(&state, plain, 64, 4);
        assert_eq!(buf[(0, 0)].symbol(), "✕");
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_blank_reason_skips_the_reason_line() {
        let state = ViewState::Error {
            title: "Couldn't load pull requests",
            reason: String::new(),
            actions: vec![("r".to_string(), "retry")],
            severity: FailureSeverity::Danger,
        };
        let text = screen_text(&draw(&state, Theme::test_default(), 40, 4));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("Couldn't load pull requests"), "{text}");
        assert!(lines[1].contains("r: retry"), "{text}");
    }

    #[test]
    fn a_long_reason_is_cut_with_an_ellipsis() {
        let state = ViewState::Error {
            title: "Couldn't load pull requests",
            reason: "abcdefghijklmnopqrstuvwxyz".to_string(),
            actions: vec![("r".to_string(), "retry")],
            severity: FailureSeverity::Danger,
        };
        let text = screen_text(&draw(&state, Theme::test_default(), 10, 4));
        assert!(text.contains('…'), "{text}");
        assert!(!text.contains("uvwxyz"), "{text}");
    }

    #[test]
    fn a_zero_area_does_not_panic() {
        let backend = TestBackend::new(1, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = ViewState::Loading {
            what: "Loading issues…",
        };
        terminal
            .draw(|frame| {
                render_state(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    },
                    &Theme::test_default(),
                    &state,
                    "⠋",
                );
            })
            .unwrap();
    }

    #[test]
    fn hints_come_from_the_bound_keys() {
        let mut kb = crate::config::Config::default().keybindings;
        kb.retry = KeySequence::single(KeyBinding::char('z'));
        kb.quit = KeySequence::single(KeyBinding::char('x'));
        kb.comment = KeySequence::single(KeyBinding::char('m'));
        kb.suggestion = KeySequence::single(KeyBinding::char('n'));
        assert_eq!(retry_action(&kb), ("z".to_string(), "retry"));
        assert_eq!(clear_filter_hint(&kb), "x: clear filter");
        assert_eq!(comment_empty_hint(&kb), "m: comment, n: suggestion");
    }

    #[test]
    fn screen_sources_do_not_classify_http_status() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let mut pending = vec![root];
        let markers = ["http 404", "http 403", "could not resolve to a repository"];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                let source = strip_cfg_test(&source).to_ascii_lowercase();
                for marker in markers {
                    assert!(
                        !source.contains(marker),
                        "{} contains {marker}",
                        path.display()
                    );
                }
            }
        }
    }

    /// Drop `#[cfg(test)]` items so fixture messages are not mistaken for checks.
    fn strip_cfg_test(source: &str) -> String {
        let mut out = String::new();
        let mut rest = source;
        while let Some(at) = rest.find("#[cfg(test)]") {
            out.push_str(&rest[..at]);
            let after = &rest[at + "#[cfg(test)]".len()..];
            let semi = after.find(';');
            let brace = after.find('{');
            let end = match (semi, brace) {
                (Some(semi), Some(brace)) if semi < brace => at + "#[cfg(test)]".len() + semi + 1,
                (_, Some(brace)) => {
                    let open = at + "#[cfg(test)]".len() + brace;
                    let mut depth = 0;
                    let mut closed = rest.len();
                    for (i, ch) in rest[open..].char_indices() {
                        match ch {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    closed = open + i + 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    closed
                }
                (Some(semi), None) => at + "#[cfg(test)]".len() + semi + 1,
                (None, None) => rest.len(),
            };
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn display_kind_helpers_agree_with_gh_failure() {
        let failure = GhFailure {
            kind: GhFailureKind::Offline,
            message: "dial tcp".to_string(),
        };
        assert_eq!(failure_symbol(failure.severity()), "⚠");
        let missing = GhFailure::other("HTTP 404");
        assert_eq!(failure_symbol(missing.severity()), "✕");
    }

    fn comment_panel_app() -> crate::app::App {
        use crate::github::comment::CommentPublication;
        use crate::ui::test_buffer::{app_with_comment_on_added_line, comment_on_added_line};

        let mut app =
            app_with_comment_on_added_line(comment_on_added_line(CommentPublication::Published));
        app.state = crate::app::AppState::DiffView;
        app
    }

    fn draw_ui(app: &mut crate::app::App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, app))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn empty_comment_line_with_an_error_draws_the_failure_not_the_hint() {
        let mut app = comment_panel_app();
        app.cmt.review_comments = Some(vec![]);
        app.cmt.file_comment_positions.clear();
        app.cmt.comment_error = Some("comments failed".to_string());
        let text = screen_text(&draw_ui(&mut app, 100, 30));
        assert!(text.contains("✕ Couldn't load comments"), "{text}");
        assert!(text.contains("comments failed"), "{text}");
        assert!(text.contains("r: retry"), "{text}");
        assert!(!text.contains("No comments"), "{text}");
        assert!(!text.contains("c: comment, s: suggestion"), "{text}");
    }

    #[test]
    fn empty_comment_line_without_an_error_uses_the_empty_hint() {
        let mut app = comment_panel_app();
        app.cmt.review_comments = Some(vec![]);
        app.cmt.file_comment_positions.clear();
        app.cmt.comment_error = None;
        let text = screen_text(&draw_ui(&mut app, 100, 30));
        assert!(text.contains("○ No comments"), "{text}");
        assert!(text.contains("c: comment, s: suggestion"), "{text}");
    }

    #[test]
    fn a_comment_line_keeps_its_comments_when_the_refresh_failed() {
        let mut app = comment_panel_app();
        app.cmt.comment_error = Some("comments failed".to_string());
        let text = screen_text(&draw_ui(&mut app, 100, 30));
        assert!(text.contains("Consider a guard here"), "{text}");
        assert!(!text.contains("○ No comments"), "{text}");
        assert!(!text.contains("Couldn't load comments"), "{text}");
    }

    #[test]
    fn narrow_comment_error_draws_without_a_too_small_gate() {
        let mut app = comment_panel_app();
        app.cmt.review_comments = Some(vec![]);
        app.cmt.file_comment_positions.clear();
        app.cmt.comment_error = Some("comments failed".to_string());
        let text = screen_text(&draw_ui(&mut app, 58, 14));
        assert!(!text.contains("Terminal too small"), "{text}");
        assert!(
            text.contains("Couldn't load comments") || text.contains('✕'),
            "{text}"
        );
    }

    #[test]
    fn write_view_state_artifacts() {
        let Ok(dir) = std::env::var("OCTORUS_ARTIFACTS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut unauth = crate::app::App::new_for_test();
        unauth.state = crate::app::AppState::PullRequestList;
        unauth.prs.pr_list = crate::app::LoadState::Error(GhFailure {
            kind: GhFailureKind::Unauthenticated,
            message: "please run: gh auth login".to_string(),
        });
        save_artifact(
            &dir,
            "pr-list-unauthenticated",
            "PR list, gh is not authenticated",
            &draw_ui(&mut unauth, 100, 30),
        );

        let mut offline = crate::app::App::new_for_test();
        offline.state = crate::app::AppState::PullRequestList;
        offline.prs.pr_list = crate::app::LoadState::Error(GhFailure {
            kind: GhFailureKind::Offline,
            message: "dial tcp".to_string(),
        });
        save_artifact(
            &dir,
            "pr-list-offline",
            "PR list, offline",
            &draw_ui(&mut offline, 100, 30),
        );

        let mut missing = crate::app::App::new_for_test();
        missing.state = crate::app::AppState::PullRequestList;
        missing.prs.pr_list =
            crate::app::LoadState::Error(GhFailure::other("gh: Not Found (HTTP 404)"));
        save_artifact(
            &dir,
            "pr-list-404",
            "PR list, repository HTTP 404",
            &draw_ui(&mut missing, 100, 30),
        );

        let mut issues = crate::app::App::new_for_test();
        issues.state = crate::app::AppState::IssueList;
        let mut issue_state = crate::app::IssueState::new();
        issue_state.issues = crate::app::LoadState::Error(GhFailure::other("something unexpected"));
        issues.issue_state = Some(issue_state);
        save_artifact(
            &dir,
            "issue-list-other-error",
            "Issue list, other failure",
            &draw_ui(&mut issues, 100, 30),
        );

        let mut files = crate::app::App::new_for_test();
        files.state = crate::app::AppState::FileList;
        files.data_state = crate::app::DataState::Error(GhFailure::other("disk full"));
        save_artifact(
            &dir,
            "file-list-error",
            "File list, other failure",
            &draw_ui(&mut files, 100, 30),
        );

        let mut loading = crate::app::App::new_for_test();
        loading.state = crate::app::AppState::FileList;
        save_artifact(
            &dir,
            "file-list-loading",
            "File list, loading PR data",
            &draw_ui(&mut loading, 100, 30),
        );

        let mut cockpit =
            crate::app::App::new_cockpit("owner/repo", crate::config::Config::default(), true);
        cockpit
            .cockpit_state
            .as_mut()
            .unwrap()
            .mentioned_issues_count =
            crate::app::LoadState::Error(GhFailure::other("counts broke"));
        save_artifact(
            &dir,
            "cockpit-counts-error",
            "Cockpit counts, other failure",
            &draw_ui(&mut cockpit, 100, 30),
        );

        let mut comments = comment_panel_app();
        comments.cmt.review_comments = Some(vec![]);
        comments.cmt.file_comment_positions.clear();
        comments.cmt.comment_error = Some("comments failed".to_string());
        save_artifact(
            &dir,
            "comments-error-empty",
            "Comment panel, empty line with a fetch error",
            &draw_ui(&mut comments, 100, 30),
        );

        let mut narrow = crate::app::App::new_for_test();
        narrow.state = crate::app::AppState::PullRequestList;
        narrow.prs.pr_list = crate::app::LoadState::Error(GhFailure {
            kind: GhFailureKind::Unauthenticated,
            message: "please run: gh auth login".to_string(),
        });
        save_artifact(
            &dir,
            "narrow-58x14",
            "PR list at 58x14, still the unauthenticated failure",
            &draw_ui(&mut narrow, 58, 14),
        );
    }

    fn save_artifact(
        dir: &std::path::Path,
        name: &str,
        caption: &str,
        buf: &ratatui::buffer::Buffer,
    ) {
        let mut text = String::new();
        text.push_str(caption);
        text.push('\n');
        text.push_str(&ansi_screen(buf));
        std::fs::write(dir.join(format!("{name}.txt")), text).unwrap();
    }

    fn ansi_screen(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        let area = buf.area();
        for y in 0..area.height {
            let mut current = None;
            for x in 0..area.width {
                let cell = &buf[(x, y)];
                if cell.symbol().is_empty() {
                    continue;
                }
                let style = (cell.fg, cell.bg, cell.modifier);
                if current != Some(style) {
                    out.push_str("\u{1b}[0m");
                    out.push_str(&sgr(cell.fg, cell.bg, cell.modifier));
                    current = Some(style);
                }
                out.push_str(cell.symbol());
            }
            out.push_str("\u{1b}[0m\n");
        }
        out
    }

    fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
        let mut codes = Vec::new();
        if modifier.contains(Modifier::BOLD) {
            codes.push("1".to_string());
        }
        if modifier.contains(Modifier::DIM) {
            codes.push("2".to_string());
        }
        if modifier.contains(Modifier::ITALIC) {
            codes.push("3".to_string());
        }
        if modifier.contains(Modifier::UNDERLINED) {
            codes.push("4".to_string());
        }
        if modifier.contains(Modifier::REVERSED) {
            codes.push("7".to_string());
        }
        if let Some(code) = color_code(fg, false) {
            codes.push(code);
        }
        if let Some(code) = color_code(bg, true) {
            codes.push(code);
        }
        if codes.is_empty() {
            String::new()
        } else {
            format!("\u{1b}[{}m", codes.join(";"))
        }
    }

    fn color_code(color: Color, background: bool) -> Option<String> {
        let named = |base: u8| {
            if background {
                (base + 10).to_string()
            } else {
                base.to_string()
            }
        };
        let rgb_prefix = if background { "48" } else { "38" };
        match color {
            Color::Reset => None,
            Color::Black => Some(named(30)),
            Color::Red => Some(named(31)),
            Color::Green => Some(named(32)),
            Color::Yellow => Some(named(33)),
            Color::Blue => Some(named(34)),
            Color::Magenta => Some(named(35)),
            Color::Cyan => Some(named(36)),
            Color::Gray => Some(named(37)),
            Color::DarkGray => Some(named(90)),
            Color::LightRed => Some(named(91)),
            Color::LightGreen => Some(named(92)),
            Color::LightYellow => Some(named(93)),
            Color::LightBlue => Some(named(94)),
            Color::LightMagenta => Some(named(95)),
            Color::LightCyan => Some(named(96)),
            Color::White => Some(named(97)),
            Color::Rgb(r, g, b) => Some(format!("{rgb_prefix};2;{r};{g};{b}")),
            Color::Indexed(index) => Some(format!("{rgb_prefix};5;{index}")),
        }
    }
}
