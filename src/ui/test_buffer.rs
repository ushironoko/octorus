use ratatui::buffer::{Buffer, Cell};
use ratatui::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DataState};
use crate::config::{ThemeColors, ThemeConfig};
use crate::github::comment::{CommentPublication, ReviewComment, ReviewCommentLocation};
use crate::github::{Branch, ChangedFile, PullRequest, User};
use crate::ui::theme::Theme;

/// A warning color no preset uses, so a test can tell the theme's color from yellow.
pub(crate) const CUSTOM_WARNING: Color = Color::Rgb(1, 2, 3);

/// A muted color no preset uses, for the same reason as [`CUSTOM_WARNING`].
pub(crate) const CUSTOM_MUTED: Color = Color::Rgb(4, 5, 6);

/// A success color no preset uses, for the same reason as [`CUSTOM_WARNING`].
pub(crate) const CUSTOM_SUCCESS: Color = Color::Rgb(7, 8, 9);

/// A danger color no preset uses, for the same reason as [`CUSTOM_WARNING`].
pub(crate) const CUSTOM_DANGER: Color = Color::Rgb(10, 11, 12);

/// The dark preset with the given color overrides, as a `[theme]` config applies them.
pub(crate) fn theme_with(colors: ThemeColors) -> Theme {
    Theme::resolve(
        &ThemeConfig {
            colors,
            ..ThemeConfig::default()
        },
        false,
    )
}

/// The dark preset drawn without color, as `NO_COLOR` gives it.
pub(crate) fn no_color_theme() -> Theme {
    Theme::resolve(&ThemeConfig::default(), true)
}

pub(crate) fn find_column(buf: &Buffer, y: u16, needle: &str) -> Option<u16> {
    let width = buf.area().width;
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() {
        return Some(0);
    }
    for start in 0..width {
        let mut x = start;
        let mut matched = 0;
        let mut found = true;
        while matched < needle.len() {
            if x >= width {
                found = false;
                break;
            }
            let mut chars = buf[(x, y)].symbol().chars();
            let Some(ch) = chars.next() else {
                found = false;
                break;
            };
            if chars.next().is_some() || ch != needle[matched] {
                found = false;
                break;
            }
            matched += 1;
            x += 1;
        }
        if found {
            return Some(start);
        }
    }
    None
}

pub(crate) fn row_text(buf: &Buffer, y: u16) -> String {
    let mut line = String::new();
    let mut x = 0;
    while x < buf.area().width {
        let sym = buf[(x, y)].symbol();
        let width = u16::try_from(sym.width()).unwrap_or(1).max(1);
        line.push_str(sym);
        x = x.saturating_add(width);
    }
    line.trim_end().to_string()
}

pub(crate) fn screen_text(buf: &Buffer) -> String {
    (0..buf.area().height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The cells that `needle` covers on the first row that draws it, or `None` when no
/// row does. It takes one cell per char, so it is for ASCII and single-width text.
pub(crate) fn cells_of(buf: &Buffer, needle: &str) -> Option<Vec<Cell>> {
    let width = u16::try_from(needle.chars().count()).ok()?;
    (0..buf.area().height).find_map(|y| {
        let x = find_column(buf, y, needle)?;
        Some((x..x + width).map(|x| buf[(x, y)].clone()).collect())
    })
}

/// The patch `app_with_comment_on_added_line` loads. Its `+added` line is new
/// line 2, which is diff row 2.
const ADDED_LINE_PATCH: &str = "@@ -1,3 +1,4 @@\n context\n+added\n more context";

/// A review comment on the `+added` line of `app_with_comment_on_added_line`,
/// published or a draft in the viewer's pending review.
pub(crate) fn comment_on_added_line(publication: CommentPublication) -> ReviewComment {
    ReviewComment {
        id: 7,
        path: "src/main.rs".to_string(),
        line: Some(2),
        start_line: None,
        body: "Consider a guard here".to_string(),
        user: User {
            login: "alice".to_string(),
        },
        created_at: "2026-11-10T00:00:00Z".to_string(),
        in_reply_to_id: None,
        location: ReviewCommentLocation {
            publication,
            ..Default::default()
        },
    }
}

/// An app on the diff of `src/main.rs`, with the comment panel open on the
/// `+added` line. `comment` sits on that line as the diff's comment positions
/// place it. The caller sets the app state.
pub(crate) fn app_with_comment_on_added_line(comment: ReviewComment) -> App {
    let mut app = App::new_for_test();
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
                login: "owner".to_string(),
            },
            updated_at: "2026-11-10T00:00:00Z".to_string(),
        }),
        files: vec![ChangedFile {
            filename: "src/main.rs".to_string(),
            status: "modified".to_string(),
            additions: 1,
            deletions: 0,
            patch: Some(ADDED_LINE_PATCH.to_string()),
            viewed: false,
        }],
    };
    app.selected_file = 0;
    app.cmt.review_comments = Some(vec![comment]);
    app.update_file_comment_positions();
    app.diff_scroll.selected_line = 2;
    app.cmt.comment_panel_open = true;
    app
}
