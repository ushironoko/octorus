//! The footer notice's look (#250): `✓` in the theme's success colour and `✗` in
//! its danger colour. Both colours come from the theme, not from fixed values.

use ratatui::style::Color;

use crate::app::{App, Toast};
use crate::config::ThemeColors;

use super::footer::build_footer_line;
use super::test_buffer::theme_with;

#[test]
fn a_success_notice_is_a_check_in_the_theme_success_colour() {
    let mut app = App::new_for_test();
    app.theme = theme_with(ThemeColors {
        success: Some(Color::Magenta),
        ..Default::default()
    });
    app.cmt.submission_result = Some(Toast::success("Submitted"));

    let line = build_footer_line(&app, "help");

    assert_eq!(line.spans[0].content, "✓ Submitted");
    assert_eq!(line.spans[0].style.fg, Some(Color::Magenta));
}

#[test]
fn a_failure_notice_is_a_cross_in_the_theme_danger_colour() {
    let mut app = App::new_for_test();
    app.theme = theme_with(ThemeColors {
        danger: Some(Color::Yellow),
        ..Default::default()
    });
    app.cmt.submission_result = Some(Toast::failure("Failed: network error"));

    let line = build_footer_line(&app, "help");

    assert_eq!(line.spans[0].content, "✗ Failed: network error");
    assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
}

#[test]
fn the_default_theme_shows_success_green_and_failure_red() {
    let mut app = App::new_for_test();

    app.cmt.submission_result = Some(Toast::success("Submitted"));
    let success = build_footer_line(&app, "help");
    assert_eq!(success.spans[0].style.fg, Some(Color::Green));

    app.cmt.submission_result = Some(Toast::failure("Failed: network error"));
    let failure = build_footer_line(&app, "help");
    assert_eq!(failure.spans[0].style.fg, Some(Color::Red));
}
