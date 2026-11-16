use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};
use unicode_width::UnicodeWidthStr;

use super::common::truncate_with_width;
use super::state_view::{failure_style, failure_symbol};
use super::theme::Theme;
use crate::app::{App, AppState, CommentTab, DataState, LoadState, ShellPhase, Toast, ToastKind};
use crate::config::KeybindingsConfig;
use crate::github::{FailureSurface, GhFailure};

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
/// 1. Shell input overlay (highest, focused pane only)
/// 2. Approve confirmation
/// 3. Comment, issue comment, or review submitting
/// 4. Submission result
/// 5. Load/fetch failure. A comment failure waits out `comments_loading`.
///    The inferred-PR notice, the help hints, and the loading/searching
///    indicators stay on the same line.
/// 6. PR open notice, with those same indicators
/// 7. Default help text plus loading indicators
///
/// Whichever of these applies, the config-warning badge comes first when the config
/// loaded with warnings.
pub fn build_footer_line<'a>(app: &'a App, help_text: &'a str) -> Line<'a> {
    build_footer_line_fitted(app, help_text, usize::MAX)
}

/// The footer line fitted to `width` columns, the inside of the footer border.
/// A load failure is truncated there: hints first, then the reason. The symbol,
/// the title, and the retry key stay while the width allows them.
pub fn build_footer_line_fitted<'a>(app: &'a App, help_text: &'a str, width: usize) -> Line<'a> {
    build_footer_line_with_focus_fitted(app, help_text, true, width)
}

pub fn build_footer_line_with_focus<'a>(
    app: &'a App,
    help_text: &'a str,
    is_focused: bool,
) -> Line<'a> {
    build_footer_line_with_focus_fitted(app, help_text, is_focused, usize::MAX)
}

pub fn build_footer_line_with_focus_fitted<'a>(
    app: &'a App,
    help_text: &'a str,
    is_focused: bool,
    width: usize,
) -> Line<'a> {
    build_footer_line_with_op_toast(app, None, help_text, is_focused, width)
}

/// Like [`build_footer_line_with_focus`], but a git operation's notice takes the
/// help text's place. That slot comes last, so the screen's other notices still
/// come first.
pub fn build_footer_line_with_op_toast<'a>(
    app: &'a App,
    op_toast: Option<&Toast>,
    help_text: &'a str,
    is_focused: bool,
    width: usize,
) -> Line<'a> {
    let badge = config_warning_span(app);
    let badge_width = badge
        .as_ref()
        .map(|span| span.content.width().saturating_add(2))
        .unwrap_or(0);
    let status = footer_status_line(
        app,
        op_toast,
        help_text,
        is_focused,
        width.saturating_sub(badge_width),
    );
    with_badge(badge, status)
}

fn footer_status_line<'a>(
    app: &'a App,
    op_toast: Option<&Toast>,
    help_text: &'a str,
    is_focused: bool,
    width: usize,
) -> Line<'a> {
    if is_focused {
        if let Some(line) = shell_input_line(app) {
            return line;
        }
    }
    if app.is_pending_approve_confirmation() {
        Line::from(Span::styled(
            app.approve_confirmation_footer_text(),
            app.theme.warning.style(),
        ))
    } else if app.is_submitting_comment()
        || app.is_issue_comment_submitting()
        || app.is_submitting_review()
    {
        Line::from(Span::styled(
            format!("{} Submitting...", app.spinner_char()),
            app.theme.warning.style(),
        ))
    } else if let Some(toast) = &app.cmt.submission_result {
        let mut spans = vec![toast_span(toast, app.theme)];
        // A reload that follows a result is still running. Its progress is shown
        // beside the result, so the wait is visible while the message is up.
        if comments_loading_visible(app) {
            spans.push(Span::raw("  "));
            spans.push(loading_comments_span(app));
        }
        Line::from(spans)
    } else if let Some(toast) = op_toast {
        // A fresh operation toast wins over a load failure, the same way a
        // submit result does. The failure returns when the toast clears.
        activity_line(app, toast_span(toast, app.theme))
    } else if let Some(failure) = screen_failure(app) {
        failure_status_line(app, &failure, help_text, width)
    } else if let Some(message) = app.pr_open_notice() {
        activity_line(app, Span::styled(message, app.theme.warning.style()))
    } else {
        activity_line(app, Span::raw(help_text))
    }
}

fn activity_line<'a>(app: &App, first: Span<'a>) -> Line<'a> {
    let mut spans = vec![first];
    if comments_loading_visible(app) {
        spans.push(Span::raw("  "));
        spans.push(loading_comments_span(app));
    }
    if app.symbol_search.is_searching() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{} Searching...", app.spinner_char()),
            app.theme.warning.style(),
        ));
    }
    Line::from(spans)
}

struct ShownFailure {
    head: String,
    reason: String,
    action: String,
    style: Style,
}

/// The load or fetch failure of the screen on display. It is derived each frame,
/// so it stays until a retry succeeds or the screen changes.
fn screen_failure(app: &App) -> Option<ShownFailure> {
    if !app.state.is_data_state_independent() {
        if let DataState::Error(failure) = &app.data_state {
            return Some(shown(app, failure, FailureSurface::FileList, "retry", 0));
        }
    }
    match app.state {
        AppState::PullRequestList => match &app.prs.pr_list {
            LoadState::Error(failure) => {
                Some(shown(app, failure, FailureSurface::PrList, "retry", 0))
            }
            _ => None,
        },
        AppState::IssueList => match app.issue_state.as_ref().map(|state| &state.issues) {
            Some(LoadState::Error(failure)) => {
                Some(shown(app, failure, FailureSurface::IssueList, "retry", 0))
            }
            _ => None,
        },
        AppState::Cockpit => cockpit_failure(app),
        AppState::DiffView | AppState::SplitViewDiff => comment_panel_failure(app),
        AppState::CommentList => comment_list_failure(app),
        _ => None,
    }
}

fn cockpit_failure(app: &App) -> Option<ShownFailure> {
    let cockpit = app.cockpit_state.as_ref()?;
    let mut failures = Vec::new();
    for state in [&cockpit.mentioned_issues_count, &cockpit.review_prs_count] {
        if let LoadState::Error(failure) = state {
            failures.push(failure);
        }
    }
    let first = *failures.first()?;
    let extra = failures.len().saturating_sub(1);
    Some(shown(
        app,
        first,
        FailureSurface::CockpitCounts,
        "retry",
        extra,
    ))
}

/// The footer is showing the comment failure, which already names `r`.
/// The hint line omits that key only in this case, so a reload that hides
/// the failure puts `r: reply` back.
pub(super) fn comment_failure_is_in_the_footer(app: &App) -> bool {
    comment_panel_failure(app).is_some()
}

fn comment_panel_failure(app: &App) -> Option<ShownFailure> {
    // A reload leaves the old error in place. While that reload is running the
    // spinner is the status, and the failure returns when the reload settles.
    if app.cmt.comments_loading {
        return None;
    }
    let message = app.cmt.comment_error.as_deref()?;
    let failure = GhFailure::from_message(message);
    // The label is what `r` does on this line. A line that still has comments
    // replies while the panel is open; an empty failed line retries.
    let label = if app.has_comment_at_current_line() {
        if app.cmt.comment_panel_open {
            "reply"
        } else {
            ""
        }
    } else {
        "retry"
    };
    Some(shown(app, &failure, FailureSurface::Comments, label, 0))
}

fn comment_list_failure(app: &App) -> Option<ShownFailure> {
    let message = match app.cmt.comment_tab {
        CommentTab::Review => {
            if app.cmt.comments_loading {
                return None;
            }
            app.cmt.comment_error.as_deref()
        }
        CommentTab::Discussion => {
            if app.cmt.discussion_comments_loading {
                return None;
            }
            app.cmt.discussion_error.as_deref()
        }
    }?;
    let failure = GhFailure::from_message(message);
    Some(shown(app, &failure, FailureSurface::Comments, "retry", 0))
}

fn shown(
    app: &App,
    failure: &GhFailure,
    surface: FailureSurface,
    label: &str,
    extra: usize,
) -> ShownFailure {
    let title = failure.title(surface);
    let extra = if extra > 0 {
        format!(" (+{extra})")
    } else {
        String::new()
    };
    let action = if label.is_empty() {
        String::new()
    } else {
        let key = if label == "reply" {
            app.config.keybindings.reply.display_primary()
        } else {
            app.config.keybindings.retry.display_primary()
        };
        format!("{key}: {label}")
    };
    ShownFailure {
        head: format!("{} {title}{extra}", failure_symbol(failure.severity())),
        reason: failure.reason(),
        action,
        style: failure_style(app.theme, failure.severity()),
    }
}

/// The label `r` has in the comment pane title. It is `retry` on an empty
/// failed line and `reply` otherwise, which is what that key does.
pub(super) fn comment_pane_title(app: &App) -> String {
    let retry = app.cmt.comment_error.is_some() && !app.has_comment_at_current_line();
    let (key, label) = if retry {
        (app.config.keybindings.retry.display_primary(), "retry")
    } else {
        (app.config.keybindings.reply.display_primary(), "reply")
    };
    format!("Comments (j/k/↑↓: scroll, c: comment, s: suggest, {key}: {label})")
}

/// `notice  symbol title · reason  key: retry  hints`, then the loading and
/// searching indicators. Their width is reserved before the hints or the
/// reason are cut. An 80-column terminal (78 columns inside the border) keeps
/// the help hint and the retry key, and still leaves the reason a few
/// characters when a notice is also showing.
fn failure_status_line(
    app: &App,
    failure: &ShownFailure,
    hints: &str,
    width: usize,
) -> Line<'static> {
    let notice = app.pr_open_notice().unwrap_or_default();
    let tail_width = activity_tail_width(app);
    // The spinner is cut before the retry key. Reserve only the tail columns
    // that remain after the title and that key.
    let action_keep = if failure.action.is_empty() {
        0
    } else {
        line_width("", &failure.head, "", &failure.action, "")
    };
    let tail_budget = if width == usize::MAX {
        tail_width
    } else {
        tail_width.min(width.saturating_sub(action_keep))
    };
    let fitted_width = if width == usize::MAX {
        usize::MAX
    } else {
        width.saturating_sub(tail_budget)
    };
    let (notice, head, reason, action, hints) = fit_failure_line(
        &notice,
        &failure.head,
        &failure.reason,
        &failure.action,
        hints,
        fitted_width,
    );
    let mut status = head;
    if !reason.is_empty() {
        status.push_str(" · ");
        status.push_str(&reason);
    }
    if !action.is_empty() {
        if !status.is_empty() {
            status.push_str("  ");
        }
        status.push_str(&action);
    }
    let mut spans = Vec::new();
    if !notice.is_empty() {
        spans.push(Span::styled(notice, app.theme.warning.style()));
        if !status.is_empty() || !hints.is_empty() {
            spans.push(Span::raw("  "));
        }
    }
    spans.push(Span::styled(status, failure.style));
    if !hints.is_empty() {
        spans.push(Span::raw(format!("  {hints}")));
    }
    push_activity_tail_within(app, &mut spans, tail_budget);
    Line::from(spans)
}

fn comments_loading_visible(app: &App) -> bool {
    app.cmt.comments_loading || app.cmt.discussion_comments_loading
}

fn activity_tail_width(app: &App) -> usize {
    let mut width = 0;
    if comments_loading_visible(app) {
        width += 2 + format!("{} Loading comments...", app.spinner_char()).width();
    }
    if app.symbol_search.is_searching() {
        width += 2 + format!("{} Searching...", app.spinner_char()).width();
    }
    width
}

fn push_activity_tail_within(app: &App, spans: &mut Vec<Span<'static>>, budget: usize) {
    let mut remaining = budget;
    if comments_loading_visible(app) {
        remaining = push_tail_piece(
            spans,
            &format!("{} Loading comments...", app.spinner_char()),
            app.theme.warning.style(),
            remaining,
        );
    }
    if app.symbol_search.is_searching() {
        push_tail_piece(
            spans,
            &format!("{} Searching...", app.spinner_char()),
            app.theme.warning.style(),
            remaining,
        );
    }
}

fn push_tail_piece(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    remaining: usize,
) -> usize {
    if remaining <= 2 {
        return remaining;
    }
    let shown = truncate_with_width(text, remaining - 2);
    if shown.is_empty() {
        return remaining;
    }
    let used = 2 + shown.width();
    spans.push(Span::raw("  "));
    spans.push(Span::styled(shown.into_owned(), style));
    remaining.saturating_sub(used)
}

/// Inner width of an 80-column terminal. The footer border takes the other two.
/// At this width and wider the help hint and the retry key stay.
const NORMAL_FOOTER_WIDTH: usize = 78;
/// A long reason keeps at least this many columns (`abcd…`) once the title,
/// the key, and a notice floor are reserved.
const REASON_MIN_WIDTH: usize = 6;

fn fit_failure_line(
    notice: &str,
    head: &str,
    reason: &str,
    action: &str,
    hints: &str,
    width: usize,
) -> (String, String, String, String, String) {
    if width == usize::MAX {
        return (
            notice.to_string(),
            head.to_string(),
            reason.to_string(),
            action.to_string(),
            hints.to_string(),
        );
    }
    if width == 0 {
        return (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        );
    }
    if line_width(notice, head, reason, action, hints) <= width {
        return (
            notice.to_string(),
            head.to_string(),
            reason.to_string(),
            action.to_string(),
            hints.to_string(),
        );
    }
    if width >= NORMAL_FOOTER_WIDTH || !notice.is_empty() {
        // A notice that is not whole takes the columns hints would have used.
        // Hints stay empty until the full notice, the reason, and the key fit.
        fit_failure_line_normal(notice, head, reason, action, hints, width)
    } else {
        let (head, reason, action, hints) = fit_failure(head, reason, action, hints, width);
        (String::new(), head, reason, action, hints)
    }
}

/// Keep the retry key. The full notice outranks the reason, and the reason
/// outranks every hint. Hints, including the `?` hint, are drawn only once the
/// full notice, the reason, and the key fit. Until then the freed columns grow
/// the notice toward its full text before the reason grows past its minimum.
/// With no notice, hints are cut before the reason.
fn fit_failure_line_normal(
    notice: &str,
    head: &str,
    reason: &str,
    action: &str,
    hints: &str,
    width: usize,
) -> (String, String, String, String, String) {
    let notice_locked = !notice.is_empty() && line_width(notice, head, reason, action, "") <= width;
    let notice_reserve = if notice_locked {
        notice.to_string()
    } else {
        notice_floor(notice)
    };
    let protected = if notice_locked {
        question_segment_that_fits(hints, notice, head, reason, action, width)
    } else if !notice.is_empty() {
        // The notice is still cut, so neither the `?` hint nor any other hint
        // may take a column.
        String::new()
    } else {
        question_hint_that_fits(hints, head, action, reason, notice, width)
    };
    let others_src = if hints.contains('?') {
        hints_without_question(hints)
    } else {
        hints.to_string()
    };
    let core_full = line_width(&notice_reserve, head, reason, action, &protected);
    // With no notice, other hints are shown only when the whole reason fits
    // beside the title, the key, and the `?` hint. A notice that is not whole
    // takes that room instead, and grows before the reason passes its minimum.
    let reason_locked = if notice.is_empty() {
        core_full <= width
    } else {
        notice_locked
    };
    let others = if (!notice.is_empty() && !notice_locked) || !reason_locked {
        String::new()
    } else if notice_locked {
        // The separator is measured on the combined line, so the notice is not
        // shortened to pay for it.
        let mut others = others_src.clone();
        loop {
            let combined = combine_hints(hints, &protected, &others);
            if others_src.is_empty()
                || line_width(&notice_reserve, head, reason, action, &combined) <= width
            {
                break;
            }
            let next = others.width().saturating_sub(1);
            if next <= 1 {
                others.clear();
                break;
            }
            others = truncate_with_width(&others, next).into_owned();
        }
        others
    } else {
        clip_plain_hints(&others_src, width - core_full)
    };
    let hints = combine_hints(hints, &protected, &others);

    let base = line_width("", head, "", action, &hints);
    if base > width {
        let (head, reason, action, hints) = fit_failure(head, reason, action, &hints, width);
        return (String::new(), head, reason, action, hints);
    }
    let (notice, reason) = share_notice_and_reason(notice, reason, width - base, reason_locked);
    (notice, head.to_string(), reason, action.to_string(), hints)
}

/// The `?` hint, kept whole, when it fits after the full notice, the reason,
/// and the key.
fn question_segment_that_fits(
    hints: &str,
    notice: &str,
    head: &str,
    reason: &str,
    action: &str,
    width: usize,
) -> String {
    let Some(segment) = question_segment(hints) else {
        return String::new();
    };
    if line_width(notice, head, reason, action, &segment) <= width {
        segment
    } else {
        String::new()
    }
}

/// The `?` hint, kept whole, when it fits with the title, the key, and the
/// reason's minimum. It wins over the hints that sit before it.
fn question_hint_that_fits(
    hints: &str,
    head: &str,
    action: &str,
    reason: &str,
    notice: &str,
    width: usize,
) -> String {
    let Some(segment) = question_segment(hints) else {
        return String::new();
    };
    let floor = reason_floor_text(reason);
    let notice = notice_floor(notice);
    if line_width(&notice, head, &floor, action, &segment) <= width {
        segment
    } else {
        String::new()
    }
}

fn question_segment(hints: &str) -> Option<String> {
    let question = hints.find('?')?;
    let start = segment_start(hints, question);
    let end = segment_end(hints, question);
    let segment = hints[start..end].trim().to_string();
    if segment.is_empty() {
        None
    } else {
        Some(segment)
    }
}

fn hints_without_question(hints: &str) -> String {
    let Some(question) = hints.find('?') else {
        return hints.to_string();
    };
    let start = segment_start(hints, question);
    let end = segment_end(hints, question);
    let sep = if hints.contains(" | ") { " | " } else { "  " };
    let left = hints[..start].trim_end_matches([' ', '|']);
    let right = hints[end..].trim_start_matches([' ', '|']);
    match (left.is_empty(), right.is_empty()) {
        (true, true) => String::new(),
        (false, true) => left.to_string(),
        (true, false) => right.to_string(),
        (false, false) => format!("{left}{sep}{right}"),
    }
}

fn segment_start(hints: &str, question: usize) -> usize {
    hints[..question]
        .rfind(" | ")
        .map(|index| index + 3)
        .or_else(|| hints[..question].rfind("  ").map(|index| index + 2))
        .unwrap_or(0)
}

fn segment_end(hints: &str, question: usize) -> usize {
    hints[question..]
        .find(" | ")
        .or_else(|| hints[question..].find("  "))
        .map(|index| question + index)
        .unwrap_or(hints.len())
}

fn combine_hints(original: &str, protected: &str, others: &str) -> String {
    if protected.is_empty() {
        return others.to_string();
    }
    if others.is_empty() {
        return protected.to_string();
    }
    let sep = if original.contains(" | ") {
        " | "
    } else {
        "  "
    };
    if original.trim_start().starts_with('?') {
        format!("{protected}{sep}{others}")
    } else {
        format!("{others}{sep}{protected}")
    }
}

fn clip_plain_hints(hints: &str, budget: usize) -> String {
    if hints.is_empty() || budget <= 2 {
        return String::new();
    }
    let max = budget - 2;
    if hints.width() <= max {
        hints.to_string()
    } else if max <= 1 {
        String::new()
    } else {
        truncate_with_width(hints, max).into_owned()
    }
}

fn reason_floor_text(reason: &str) -> String {
    if reason.is_empty() {
        String::new()
    } else {
        truncate_with_width(reason, REASON_MIN_WIDTH).into_owned()
    }
}

/// `PR #N inferred…`, the shortest form that still says the PR was inferred.
fn notice_floor(notice: &str) -> String {
    if notice.is_empty() {
        return String::new();
    }
    const MARK: &str = "inferred";
    if let Some(index) = notice.find(MARK) {
        let end = index + MARK.len();
        if end == notice.len() {
            return notice.to_string();
        }
        return format!("{}…", &notice[..end]);
    }
    truncate_with_width(notice, 17).into_owned()
}

fn share_notice_and_reason(
    notice: &str,
    reason: &str,
    rest: usize,
    lock_reason: bool,
) -> (String, String) {
    let reason_full = if reason.is_empty() {
        0
    } else {
        3 + reason.width()
    };
    if lock_reason {
        let reason_budget = reason_full.min(rest);
        let spare = rest - reason_budget;
        return (
            clip_leading(notice, spare),
            clip_reason(reason, reason_budget),
        );
    }
    let reason_min = reason_min_cost(reason);
    let notice_floor_cost = leading_cost(&notice_floor(notice));
    let notice_full = leading_cost(notice);

    let (mut notice_budget, mut reason_budget, mut spare) =
        if notice_floor_cost + reason_min <= rest {
            (
                notice_floor_cost,
                reason_min,
                rest - notice_floor_cost - reason_min,
            )
        } else if notice_floor_cost <= rest {
            (notice_floor_cost, 0, rest - notice_floor_cost)
        } else if reason_min <= rest {
            (0, reason_min, rest - reason_min)
        } else if notice.is_empty() {
            (0, rest.min(reason_full), 0)
        } else {
            (rest, 0, 0)
        };

    let notice_extra = spare.min(notice_full.saturating_sub(notice_budget));
    notice_budget += notice_extra;
    spare -= notice_extra;
    let reason_extra = spare.min(reason_full.saturating_sub(reason_budget));
    reason_budget += reason_extra;
    (
        clip_leading(notice, notice_budget),
        clip_reason(reason, reason_budget),
    )
}

fn reason_min_cost(reason: &str) -> usize {
    if reason.is_empty() {
        0
    } else {
        3 + reason.width().min(REASON_MIN_WIDTH)
    }
}

fn leading_cost(notice: &str) -> usize {
    if notice.is_empty() {
        0
    } else {
        notice.width() + 2
    }
}

fn clip_leading(text: &str, room: usize) -> String {
    if text.is_empty() || room <= 2 {
        String::new()
    } else {
        truncate_with_width(text, room - 2).into_owned()
    }
}

fn clip_reason(reason: &str, room: usize) -> String {
    if reason.is_empty() || room <= 3 {
        String::new()
    } else {
        truncate_with_width(reason, room - 3).into_owned()
    }
}

fn line_width(notice: &str, head: &str, reason: &str, action: &str, hints: &str) -> usize {
    let mut width = 0;
    if !notice.is_empty() {
        width += notice.width();
    }
    if !head.is_empty() {
        if width > 0 {
            width += 2;
        }
        width += head.width();
    }
    if !reason.is_empty() {
        width += 3 + reason.width();
    }
    if !action.is_empty() {
        if width > 0 {
            width += 2;
        }
        width += action.width();
    }
    if !hints.is_empty() {
        if width > 0 {
            width += 2;
        }
        width += hints.width();
    }
    width
}

fn fit_failure(
    head: &str,
    reason: &str,
    action: &str,
    hints: &str,
    width: usize,
) -> (String, String, String, String) {
    if width == usize::MAX {
        return (
            head.to_string(),
            reason.to_string(),
            action.to_string(),
            hints.to_string(),
        );
    }
    if width == 0 {
        return (String::new(), String::new(), String::new(), String::new());
    }

    let mut head = head.to_string();
    let mut reason = reason.to_string();
    let mut action = action.to_string();
    let mut hints = hints.to_string();

    let measure = |head: &str, reason: &str, action: &str, hints: &str| -> usize {
        let mut used = head.width();
        if !reason.is_empty() {
            used += 3 + reason.width();
        }
        if !action.is_empty() {
            used += 2 + action.width();
        }
        if !hints.is_empty() {
            used += 2 + hints.width();
        }
        used
    };

    if measure(&head, &reason, &action, &hints) <= width {
        return (head, reason, action, hints);
    }

    let without_hints = measure(&head, &reason, &action, "");
    if without_hints <= width {
        let hint_room = width - without_hints;
        if hint_room <= 2 {
            hints.clear();
        } else {
            hints = truncate_with_width(&hints, hint_room - 2).into_owned();
        }
        return (head, reason, action, hints);
    }
    hints.clear();

    let reserved = head.width()
        + if action.is_empty() {
            0
        } else {
            2 + action.width()
        };
    if reserved <= width {
        let reason_room = width - reserved;
        if reason.is_empty() || reason_room <= 3 {
            reason.clear();
        } else {
            reason = truncate_with_width(&reason, reason_room - 3).into_owned();
        }
        return (head, reason, action, hints);
    }

    reason.clear();
    if head.width() >= width {
        action.clear();
        head = truncate_with_width(&head, width).into_owned();
        return (head, reason, action, hints);
    }
    let action_room = width.saturating_sub(head.width() + 2);
    if action_room == 0 {
        action.clear();
    } else {
        action = truncate_with_width(&action, action_room).into_owned();
    }
    (head, reason, action, hints)
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
        .then(|| render_shell_input_line(&shell.input, shell.cursor, app.theme))
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
        app.theme.accent.style()
    } else if app.is_pending_approve_confirmation() {
        app.theme.warning.style()
    } else {
        base_style
    };
    Block::default().borders(Borders::ALL).border_style(style)
}

fn render_shell_input_line(input: &str, cursor: usize, theme: Theme) -> Line<'static> {
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
        Span::styled("! ", theme.warning.bold()),
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
    use crate::ui::test_buffer::{no_color_theme, theme_with, CUSTOM_ACCENT, CUSTOM_WARNING};
    use insta::assert_snapshot;
    use ratatui::style::Color;

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

    fn warning_theme() -> Theme {
        theme_with(ThemeColors {
            warning: Some(CUSTOM_WARNING),
            accent: Some(CUSTOM_ACCENT),
            ..ThemeColors::default()
        })
    }

    #[test]
    fn footer_notices_take_the_theme_warning_color() {
        use crate::app::SymbolSearchState;

        let mut app = App::new_for_test();
        app.theme = warning_theme();
        app.set_pending_approve_body_for_test(Some(String::new()));
        let approve = build_footer_line(&app, HELP);
        assert_eq!(approve.spans[0].style.fg, Some(CUSTOM_WARNING));
        assert!(approve.spans[0].content.contains("confirm approve"));

        app.set_pending_approve_body_for_test(None);
        app.set_submitting_for_test(true);
        let submitting = build_footer_line(&app, HELP);
        assert_eq!(submitting.spans[0].style.fg, Some(CUSTOM_WARNING));
        assert!(submitting.spans[0].content.contains("Submitting..."));

        app.set_submitting_for_test(false);
        app.state = AppState::FileList;
        app.pr_number = Some(123);
        app.pr_open_source = PrOpenSource::InferredCommitSubject {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };
        let notice = build_footer_line(&app, HELP);
        assert_eq!(notice.spans[0].style.fg, Some(CUSTOM_WARNING));
        assert!(notice.spans[0].content.contains("inferred"));

        app.pr_open_source = PrOpenSource::Standard;
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        app.symbol_search = SymbolSearchState::Searching {
            receiver: rx,
            origin_file_index: 0,
        };
        let searching = build_footer_line(&app, HELP);
        let search = searching
            .spans
            .iter()
            .find(|span| span.content.contains("Searching..."))
            .expect("search notice");
        assert_eq!(search.style.fg, Some(CUSTOM_WARNING));
    }

    #[test]
    fn footer_notices_keep_their_words_and_drop_color_under_no_color() {
        let mut app = App::new_for_test();
        app.theme = no_color_theme();
        app.set_pending_approve_body_for_test(Some(String::new()));
        let approve = build_footer_line(&app, HELP);
        assert_eq!(approve.spans[0].style.fg, None);
        assert!(!approve.spans[0].content.is_empty());

        app.set_pending_approve_body_for_test(None);
        app.set_submitting_for_test(true);
        let submitting = build_footer_line(&app, HELP);
        assert_eq!(submitting.spans[0].style.fg, None);
        assert!(submitting.spans[0].content.contains("Submitting..."));
    }

    #[test]
    fn shell_prompt_takes_the_theme_warning_color_and_stays_bold() {
        use crate::app::ShellState;

        let mut app = App::new_for_test();
        app.theme = warning_theme();
        app.shell_state = Some(ShellState {
            input: "ls".to_string(),
            cursor: 0,
            phase: ShellPhase::Input,
            scroll_offset: 0,
        });
        let line = build_footer_line(&app, HELP);
        assert_eq!(line.spans[0].content.as_ref(), "! ");
        assert_eq!(line.spans[0].style.fg, Some(CUSTOM_WARNING));
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));

        app.theme = no_color_theme();
        let plain = build_footer_line(&app, HELP);
        assert_eq!(plain.spans[0].content.as_ref(), "! ");
        assert_eq!(plain.spans[0].style.fg, None);
        assert!(plain.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    fn border_cell(block: ratatui::widgets::Block<'static>) -> ratatui::buffer::Cell {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(12, 3)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(block, frame.area()))
            .unwrap();
        terminal.backend().buffer()[(0, 0)].clone()
    }

    #[test]
    fn footer_border_takes_accent_for_shell_input_and_warning_for_approve() {
        use crate::app::ShellState;

        let mut app = App::new_for_test();
        app.theme = warning_theme();
        app.shell_state = Some(ShellState {
            input: String::new(),
            cursor: 0,
            phase: ShellPhase::Input,
            scroll_offset: 0,
        });
        let shell = border_cell(build_footer_block(&app));
        assert_eq!(shell.fg, CUSTOM_ACCENT);

        app.shell_state = None;
        app.set_pending_approve_body_for_test(Some(String::new()));
        let approve = border_cell(build_footer_block(&app));
        assert_eq!(approve.fg, CUSTOM_WARNING);

        app.theme = no_color_theme();
        app.shell_state = Some(ShellState {
            input: String::new(),
            cursor: 0,
            phase: ShellPhase::Input,
            scroll_offset: 0,
        });
        let plain = border_cell(build_footer_block(&app));
        assert_eq!(plain.fg, Color::Reset);
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
            usize::MAX,
        ));

        assert_eq!(
            text,
            "1 config warning (? for details)  \u{2717} Push failed: rejected"
        );
    }

    use crate::app::{CommentTab, DataState, LoadState};
    use crate::github::{GhFailure, GhFailureKind};

    fn line_of(app: &App, help: &str, width: usize) -> String {
        line_to_string(&build_footer_line_fitted(app, help, width))
    }

    fn pr_failure(kind: GhFailureKind, message: &str) -> App {
        let mut app = App::new_for_test();
        app.state = AppState::PullRequestList;
        app.prs.pr_list = LoadState::Error(GhFailure {
            kind,
            message: message.to_string(),
        });
        app
    }

    #[test]
    fn every_surface_puts_its_failure_in_the_footer() {
        let pr = pr_failure(GhFailureKind::Unauthenticated, "please run: gh auth login");
        let text = line_of(&pr, HELP, usize::MAX);
        assert!(text.starts_with("✕ gh is not authenticated · run: gh auth login  r: retry"));
        assert!(text.ends_with(HELP), "{text}");

        let mut issues = App::new_for_test();
        issues.state = AppState::IssueList;
        let mut issue_state = crate::app::IssueState::new();
        issue_state.issues = LoadState::Error(GhFailure::other("boom\nsecond"));
        issues.issue_state = Some(issue_state);
        let text = line_of(&issues, HELP, usize::MAX);
        assert!(
            text.contains("✕ Couldn't load issues · boom second  r: retry"),
            "{text}"
        );

        let mut files = App::new_for_test();
        files.state = AppState::FileList;
        files.data_state = DataState::Error(GhFailure::other("disk full"));
        let text = line_of(&files, "q: quit", usize::MAX);
        assert!(
            text.contains("✕ Couldn't load files · disk full  r: retry  q: quit"),
            "{text}"
        );

        let mut cockpit = App::new_cockpit("owner/repo", crate::config::Config::default(), true);
        cockpit
            .cockpit_state
            .as_mut()
            .unwrap()
            .mentioned_issues_count = LoadState::Error(GhFailure::other("counts broke"));
        let text = line_of(&cockpit, "q: quit  ?: help", usize::MAX);
        assert!(
            text.contains("✕ Couldn't load counts · counts broke  r: retry"),
            "{text}"
        );
        cockpit.cockpit_state.as_mut().unwrap().review_prs_count =
            LoadState::Error(GhFailure::other("beta broke"));
        let text = line_of(&cockpit, "q: quit  ?: help", usize::MAX);
        assert!(text.contains("(+1)"), "{text}");
        assert!(text.contains("counts broke"), "{text}");
        assert!(!text.contains("beta broke"), "{text}");

        let mut comments = App::new_for_test();
        comments.state = AppState::DiffView;
        comments.cmt.comment_panel_open = true;
        comments.cmt.comment_error = Some("comments failed".to_string());
        let text = line_of(&comments, "Esc: close", usize::MAX);
        assert!(
            text.contains("✕ Couldn't load comments · comments failed  r: retry  Esc: close"),
            "{text}"
        );

        comments.state = AppState::CommentList;
        comments.cmt.comment_tab = CommentTab::Discussion;
        comments.cmt.discussion_error = Some("discussion failed".to_string());
        comments.cmt.comment_error = None;
        let text = line_of(&comments, HELP, usize::MAX);
        assert!(
            text.contains("✕ Couldn't load comments · discussion failed  r: retry"),
            "{text}"
        );
    }

    #[test]
    fn higher_priority_footer_content_hides_the_failure_until_it_clears() {
        let mut app = pr_failure(GhFailureKind::Other, "boom");
        app.cmt.submission_result = Some(Toast::success("Submitted"));
        let text = line_of(&app, HELP, usize::MAX);
        assert!(text.contains("Submitted"), "{text}");
        assert!(!text.contains("Couldn't load pull requests"), "{text}");

        app.cmt.submission_result = Some(Toast::failure("Failed: save"));
        let text = line_of(&app, HELP, usize::MAX);
        assert!(text.contains("Failed: save"), "{text}");
        assert!(!text.contains("Couldn't load pull requests"), "{text}");

        app.cmt.submission_result = None;
        app.set_submitting_for_test(true);
        let text = line_of(&app, HELP, usize::MAX);
        assert!(text.contains("Submitting..."), "{text}");
        assert!(!text.contains("Couldn't load pull requests"), "{text}");

        app.set_submitting_for_test(false);
        app.set_pending_approve_body_for_test(Some(String::new()));
        let text = line_of(&app, HELP, usize::MAX);
        assert_eq!(text, app.approve_confirmation_footer_text());

        app.set_pending_approve_body_for_test(None);
        app.shell_state = Some(crate::app::ShellState {
            input: "ls".to_string(),
            cursor: 2,
            phase: ShellPhase::Input,
            scroll_offset: 0,
        });
        let text = line_of(&app, HELP, usize::MAX);
        assert!(text.contains('!'), "{text}");
        assert!(!text.contains("Couldn't load pull requests"), "{text}");

        app.shell_state = None;
        let toast = Toast::failure("Push failed");
        let text = line_to_string(&build_footer_line_with_op_toast(
            &app,
            Some(&toast),
            HELP,
            true,
            usize::MAX,
        ));
        assert!(text.contains("Push failed"), "{text}");
        assert!(!text.contains("Couldn't load pull requests"), "{text}");

        let text = line_of(&app, HELP, usize::MAX);
        assert!(
            text.contains("✕ Couldn't load pull requests · boom  r: retry"),
            "{text}"
        );
        assert!(text.contains(HELP), "{text}");
    }

    #[test]
    fn a_narrow_failure_drops_hints_before_the_reason_and_keeps_the_key() {
        let app = pr_failure(GhFailureKind::Other, "abcdefghijklmnopqrstuvwxyz");
        let hints = "j/k: move | q: quit | extra extra extra";
        let head = "✕ Couldn't load pull requests";
        let action = "r: retry";
        let reserved = head.chars().count() + 2 + action.chars().count();

        let reason_only = line_of(&app, hints, reserved + 3 + 10);
        assert!(reason_only.contains(head), "{reason_only}");
        assert!(reason_only.contains(action), "{reason_only}");
        assert!(reason_only.contains('…'), "{reason_only}");
        assert!(!reason_only.contains("j/k: move"), "{reason_only}");
        assert!(
            !reason_only.contains("abcdefghijklmnopqrstuvwxyz"),
            "{reason_only}"
        );

        let with_hints = line_of(&app, hints, reserved + 3 + 26 + 2 + 6);
        assert!(
            with_hints.contains("abcdefghijklmnopqrstuvwxyz"),
            "{with_hints}"
        );
        assert!(with_hints.contains(action), "{with_hints}");
        assert!(with_hints.contains('…'), "{with_hints}");
        assert!(!with_hints.contains("extra extra extra"), "{with_hints}");

        let tight = line_of(&app, hints, reserved);
        assert!(tight.contains(head), "{tight}");
        assert!(tight.contains(action), "{tight}");
        assert!(!tight.contains('…'), "{tight}");
        assert!(!tight.contains("j/k"), "{tight}");
    }

    #[test]
    fn no_color_failure_keeps_the_symbol() {
        let mut app = pr_failure(GhFailureKind::Offline, "dial tcp");
        app.theme = no_color_theme();
        let text = line_of(&app, HELP, usize::MAX);
        assert!(text.contains("⚠ Can't reach GitHub"), "{text}");

        let mut app = pr_failure(GhFailureKind::Unauthenticated, "gh auth login");
        app.theme = no_color_theme();
        let line = build_footer_line_fitted(&app, HELP, usize::MAX);
        assert!(line.spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert_eq!(line.spans[0].style.fg, None);
        assert!(line_to_string(&line).contains('✕'));
    }

    #[test]
    fn a_comment_failure_hides_while_comments_are_loading() {
        let mut app = App::new_for_test();
        app.state = AppState::DiffView;
        app.cmt.comment_panel_open = true;
        app.cmt.comment_error = Some("comments failed".to_string());
        app.cmt.comments_loading = true;
        let text = line_of(&app, "Esc: close", 98);
        assert!(text.contains("Loading comments..."), "{text}");
        assert!(!text.contains("Couldn't load comments"), "{text}");

        app.state = AppState::CommentList;
        app.cmt.comment_tab = CommentTab::Review;
        app.cmt.comment_error = Some("comments failed".to_string());
        let text = line_of(&app, HELP, 98);
        assert!(text.contains("Loading comments..."), "{text}");
        assert!(!text.contains("Couldn't load comments"), "{text}");

        app.cmt.comments_loading = false;
        app.cmt.comment_tab = CommentTab::Discussion;
        app.cmt.comment_error = None;
        app.cmt.discussion_error = Some("discussion failed".to_string());
        app.cmt.discussion_comments_loading = true;
        let text = line_of(&app, HELP, 98);
        assert!(text.contains("Loading comments..."), "{text}");
        assert!(!text.contains("Couldn't load comments"), "{text}");
    }

    #[test]
    fn a_failure_keeps_the_searching_spinner_and_a_loading_spinner() {
        let mut app = pr_failure(GhFailureKind::Other, "boom");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        app.symbol_search = crate::app::SymbolSearchState::Searching {
            receiver: rx,
            origin_file_index: 0,
        };
        let text = line_of(&app, HELP, 98);
        assert!(text.contains("Couldn't load pull requests"), "{text}");
        assert!(text.contains("r: retry"), "{text}");
        assert!(text.contains("Searching..."), "{text}");

        app.symbol_search = crate::app::SymbolSearchState::Idle;
        app.cmt.comments_loading = true;
        let text = line_of(&app, HELP, 98);
        assert!(text.contains("Couldn't load pull requests"), "{text}");
        assert!(text.contains("Loading comments..."), "{text}");
        assert!(text.contains("r: retry"), "{text}");
    }

    #[test]
    fn a_comment_failure_keeps_the_inferred_pr_notice() {
        use crate::app::PrOpenSource;

        let mut app = App::new_for_test();
        app.state = AppState::DiffView;
        app.pr_number = Some(123);
        app.pr_open_source = PrOpenSource::InferredCommitSubject {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };
        app.cmt.comment_panel_open = true;
        app.cmt.comment_error = Some("comments failed".to_string());
        let text = line_of(&app, "Esc: close", usize::MAX);
        assert!(
            text.contains("PR #123 inferred from commit subject; GitHub did not confirm it"),
            "{text}"
        );
        assert!(
            text.contains("✕ Couldn't load comments · comments failed  r: retry"),
            "{text}"
        );

        let fitted = line_of(&app, "Esc: close", 98);
        assert!(fitted.contains("from commit"), "{fitted}");
        assert!(fitted.contains("Couldn't load comments"), "{fitted}");
        assert!(fitted.contains("r: retry"), "{fitted}");
        assert!(
            fitted.contains("comme"),
            "the reason keeps its minimum beside the notice: {fitted}"
        );
        assert!(
            !fitted.contains("Esc: close"),
            "a hint was drawn beside a cut notice: {fitted}"
        );
    }

    #[test]
    fn a_sixty_column_footer_keeps_the_inferred_pr_notice() {
        use crate::app::PrOpenSource;

        let mut app = App::new_for_test();
        app.state = AppState::DiffView;
        app.pr_number = Some(123);
        app.pr_open_source = PrOpenSource::InferredCommitSubject {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };
        app.cmt.comment_panel_open = true;
        app.cmt.comment_error = Some("comments failed".to_string());
        for width in [58usize, 60] {
            let text = line_of(&app, "Esc: close", width);
            assert!(
                text.contains("PR #123 inferred"),
                "{width}: the notice was dropped: {text}"
            );
            assert!(text.contains("Couldn't load comments"), "{width}: {text}");
            assert!(text.contains("r: retry"), "{width}: {text}");
            assert!(
                !text.contains("Esc: close"),
                "{width}: hints were kept ahead of the notice: {text}"
            );
        }
    }

    #[test]
    fn a_long_reason_keeps_the_help_hint_at_normal_width() {
        let long = format!(
            "gh command failed: GraphQL: {}",
            "the issues connection failed while resolving mentionable users ".repeat(4)
        );
        let mut app = App::new_cockpit("owner/repo", crate::config::Config::default(), true);
        app.cockpit_state.as_mut().unwrap().mentioned_issues_count =
            LoadState::Error(GhFailure::other(long));
        let help = "q: quit  ?: help";
        for width in [78usize, 80, 98] {
            let text = line_of(&app, help, width);
            assert!(text.contains("Couldn't load counts"), "{width}: {text}");
            assert!(text.contains("r: retry"), "{width}: {text}");
            assert!(text.contains('?'), "{width}: {text}");
            assert!(text.contains('…'), "{width}: {text}");
            assert!(
                !text.contains("mentionable"),
                "the reason took the help hint's columns: {width}: {text}"
            );
            assert!(
                text.contains("?: help"),
                "the help hint was cut to a bare ?: {width}: {text}"
            );
        }
    }

    #[test]
    fn an_eighty_column_terminal_keeps_the_help_hint() {
        let long = format!("gh command failed: {}", "something unexpected ".repeat(8));
        let app = pr_failure(GhFailureKind::Other, &long);
        let hints = footer_hint_quit(&app.config.keybindings);
        let text = line_of(&app, &hints, 78);
        assert!(text.contains("Couldn't load pull requests"), "{text}");
        assert!(text.contains("r: retry"), "{text}");
        assert!(text.contains("? Help"), "{text}");
        assert!(text.contains('…'), "{text}");
    }

    #[test]
    fn a_question_hint_is_kept_whole_or_dropped() {
        let app = pr_failure(GhFailureKind::Other, "boom");
        let hints = "1234567890123456789012345678901234  ?: help";
        let text = line_of(&app, hints, 78);
        assert!(text.contains("Couldn't load pull requests"), "{text}");
        assert!(text.contains("r: retry"), "{text}");
        assert!(text.contains("?: help"), "{text}");
        assert!(
            !text.contains("1234567890123456789012345678901234"),
            "the hints before ? were kept and the help hint was not: {text}"
        );
        assert!(!text.trim_end().ends_with('?'), "{text}");
    }

    #[test]
    fn a_split_view_footer_keeps_the_reason_at_160_columns() {
        use crate::github::comment::CommentPublication;
        use crate::ui::test_buffer::{app_with_comment_on_added_line, comment_on_added_line};

        let mut app =
            app_with_comment_on_added_line(comment_on_added_line(CommentPublication::Published));
        app.state = AppState::SplitViewDiff;
        app.cmt.comment_error = Some("comments failed".to_string());
        let hints = "j/k/↑↓: scroll | n/N: jump | Tab: switch | c: comment | s: suggest | →/l: fullscreen | ←/h/q: close";
        for width in [160usize, 220] {
            let text = line_of(&app, hints, width);
            assert!(text.contains("Couldn't load comments"), "{width}: {text}");
            assert!(
                text.contains("comments failed"),
                "{width}: the reason was cut away for the hints: {text}"
            );
            assert!(text.contains("r: reply"), "{width}: {text}");
        }
    }

    #[test]
    fn a_cut_reason_has_already_cut_the_other_hints() {
        let long = format!(
            "gh command failed: {}",
            "something unexpected happened while loading ".repeat(6)
        );
        let mut comment = App::new_for_test();
        comment.state = AppState::DiffView;
        comment.cmt.comment_panel_open = true;
        comment.cmt.comment_error = Some(long.clone());
        let mut cockpit = App::new_cockpit("owner/repo", crate::config::Config::default(), true);
        cockpit
            .cockpit_state
            .as_mut()
            .unwrap()
            .mentioned_issues_count = LoadState::Error(GhFailure::other(long.clone()));
        let pr = pr_failure(GhFailureKind::Other, &long);

        let cases = [
            (
                &comment,
                "j/k/↑↓: scroll | n/N: jump | Tab: switch | c: comment | s: suggest | →/l: fullscreen | ←/h/q: close",
            ),
            (&pr, "? Help | ! Shell | q/Esc Quit"),
            (&cockpit, "q: quit  ?: help"),
            (&comment, "Esc: close"),
        ];
        for (app, hints) in cases {
            for width in [50usize, 78, 100, 160, 220] {
                let text = line_of(app, hints, width);
                assert_cut_reason_cut_the_other_hints(&text, &long, hints);
            }
        }
    }

    fn assert_cut_reason_cut_the_other_hints(text: &str, reason: &str, hints: &str) {
        if reason.is_empty() || text.contains(reason) {
            return;
        }
        for segment in hint_segments(hints) {
            if segment.contains('?') || segment.contains("retry") || segment.contains("reply") {
                continue;
            }
            assert!(
                !text.contains(segment),
                "hint `{segment}` is still whole while the reason is cut: {text}"
            );
        }
    }

    #[test]
    fn a_narrow_pr_list_keeps_retry_ahead_of_the_loading_spinner() {
        let mut app = pr_failure(GhFailureKind::Other, "boom");
        app.cmt.comments_loading = true;
        for width in [56usize, 58, 60] {
            let text = line_of(&app, HELP, width);
            assert!(
                text.contains("r: retry"),
                "{width}: the spinner took the retry key: {text}"
            );
            assert!(!text.contains("r…"), "{width}: {text}");
        }
    }

    #[test]
    fn a_cut_notice_never_keeps_a_hint() {
        let reasons = [
            "comments failed".to_string(),
            sized_reason(63),
            sized_reason(110),
        ];
        let closed_hints = footer_hint_back(&comment_app(false).config.keybindings);
        let split_hints = "j/k/↑↓: scroll | n/N: jump | Tab: switch | c: comment | s: suggest | →/l: fullscreen | ←/h/q: close";
        for reason in &reasons {
            let mut open = comment_app(true);
            let mut closed = comment_app(false);
            let mut files = file_list_app();
            let mut split = split_app();
            set_failure_reason(&mut open, reason);
            set_failure_reason(&mut closed, reason);
            set_failure_reason(&mut files, reason);
            set_failure_reason(&mut split, reason);
            assert_no_hint_beside_a_cut_notice(
                "diff open",
                &open,
                "Esc: close",
                "r: reply",
                "Esc",
                reason,
            );
            assert_no_hint_beside_a_cut_notice(
                "diff closed",
                &closed,
                &closed_hints,
                "r: retry",
                "?",
                reason,
            );
            assert_no_hint_beside_a_cut_notice(
                "file list",
                &files,
                "q: quit",
                "r: retry",
                "q",
                reason,
            );
            assert_no_hint_beside_a_cut_notice(
                "split",
                &split,
                split_hints,
                "r: reply",
                "j/k",
                reason,
            );
        }
    }

    fn assert_no_hint_beside_a_cut_notice(
        name: &str,
        app: &App,
        hints: &str,
        key: &str,
        marker: &str,
        reason: &str,
    ) {
        const FULL: &str = "PR #1 inferred from commit subject; GitHub did not confirm it";
        for width in 40..=260 {
            let text = line_of(app, hints, width);
            assert!(
                text.contains(key),
                "{name} reason {} width {width} dropped {key}: {text}",
                reason.len()
            );
            if !text.contains(FULL) {
                assert!(
                    !text.contains(marker),
                    "{name} reason {} width {width} kept a hint beside a cut notice: {text}",
                    reason.len()
                );
            }
        }
    }

    fn comment_app(panel_open: bool) -> App {
        use crate::github::comment::CommentPublication;
        use crate::ui::test_buffer::{app_with_comment_on_added_line, comment_on_added_line};

        let mut app =
            app_with_comment_on_added_line(comment_on_added_line(CommentPublication::Published));
        app.state = AppState::DiffView;
        if !panel_open {
            app.cmt.comment_panel_open = false;
            app.cmt.review_comments = Some(Vec::new());
            app.update_file_comment_positions();
        }
        app.pr_number = Some(1);
        app.pr_open_source = PrOpenSource::InferredCommitSubject {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };
        app
    }

    fn split_app() -> App {
        let mut app = comment_app(true);
        app.state = AppState::SplitViewDiff;
        app
    }

    fn file_list_app() -> App {
        let mut app = App::new_for_test();
        app.state = AppState::FileList;
        app.pr_number = Some(1);
        app.pr_open_source = PrOpenSource::InferredCommitSubject {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };
        app
    }

    fn set_failure_reason(app: &mut App, reason: &str) {
        use crate::app::DataState;
        use crate::github::GhFailure;

        if app.state == AppState::FileList {
            app.data_state = DataState::Error(GhFailure::other(reason));
        } else {
            app.cmt.comment_error = Some(reason.to_string());
        }
    }

    fn sized_reason(len: usize) -> String {
        let mut text = "gh command failed: ".to_string();
        while text.len() < len {
            text.push('x');
        }
        text.truncate(len);
        text
    }

    fn hint_segments(hints: &str) -> Vec<&str> {
        let parts: Vec<&str> = if hints.contains(" | ") {
            hints.split(" | ").collect()
        } else {
            hints.split("  ").collect()
        };
        parts
            .into_iter()
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect()
    }
}
