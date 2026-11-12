//! Footer toasts (#250), driven the way the event loop drives them: keys go
//! through `handle_key_event`, and the clocks run through the poll functions.
//! The rules themselves are documented in `toast.rs`.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use tokio::sync::mpsc;

use super::tests::{make_app_with_files, test_terminal};
use super::{App, AppState, GitOpsState, ShellPhase, ShellState, Toast, ToastKind};

const FAILURE: &str = "Failed: network error";

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// A toast that appeared `age` ago.
fn aged(kind: ToastKind, message: &str, age: Duration) -> Toast {
    let shown_at = Instant::now()
        .checked_sub(age)
        .expect("the monotonic clock has run longer than the test ages");
    Toast::at(kind, message, shown_at)
}

/// The PR file list, with `toast` as the comment and review footer notice.
fn file_list_showing(toast: Toast) -> App {
    let mut app = make_app_with_files(&["a.rs", "b.rs"]);
    app.cmt.submission_result = Some(toast);
    app
}

/// The git ops screen, with `notice` as the tree footer's notice.
fn git_ops_showing(notice: Option<Toast>) -> App {
    let mut app = App::new_for_test();
    let mut ops = GitOpsState::new(Vec::new());
    ops.op_message = notice;
    app.git_ops_state = Some(ops);
    app.state = AppState::GitOpsSplitTree;
    app
}

fn footer_notice(app: &App) -> Option<&Toast> {
    app.cmt.submission_result.as_ref()
}

fn git_ops_notice(app: &App) -> Option<&Toast> {
    app.git_ops_state
        .as_ref()
        .and_then(|ops| ops.op_message.as_ref())
}

// ---- success: 5 seconds, or the next key -------------------------------

#[test]
fn a_success_toast_is_still_up_at_4_9_seconds() {
    let mut app = file_list_showing(aged(
        ToastKind::Success,
        "Submitted",
        Duration::from_millis(4900),
    ));

    app.poll_comment_submit_updates();

    assert!(footer_notice(&app).is_some());
}

#[test]
fn a_success_toast_is_gone_after_5_seconds() {
    let mut app = file_list_showing(aged(
        ToastKind::Success,
        "Submitted",
        Duration::from_millis(5100),
    ));

    app.poll_comment_submit_updates();

    assert!(footer_notice(&app).is_none());
}

#[tokio::test]
async fn a_success_toast_is_cleared_by_the_next_key_and_the_key_still_runs() {
    let mut app = file_list_showing(Toast::success("Submitted"));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Char('?')), &mut terminal)
        .await
        .unwrap();

    assert!(footer_notice(&app).is_none());
    assert_eq!(app.state, AppState::Help);
}

#[tokio::test]
async fn esc_clears_a_success_toast_and_is_handled_as_usual() {
    let mut app = file_list_showing(Toast::success("Submitted"));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Esc), &mut terminal)
        .await
        .unwrap();

    assert!(footer_notice(&app).is_none());
    assert!(app.should_quit, "Esc on the file list still quits");
}

// ---- failure: no timer; a key after 2 seconds; Esc only dismisses -----

#[test]
fn a_failure_toast_is_not_removed_by_time() {
    let mut app = file_list_showing(aged(ToastKind::Failure, FAILURE, Duration::from_secs(60)));

    app.poll_comment_submit_updates();

    assert!(footer_notice(&app).is_some());
}

#[tokio::test]
async fn a_key_within_two_seconds_leaves_a_failure_toast_up_and_still_runs() {
    let mut app = file_list_showing(aged(ToastKind::Failure, FAILURE, Duration::from_secs(1)));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Char('?')), &mut terminal)
        .await
        .unwrap();

    assert!(footer_notice(&app).is_some());
    assert_eq!(app.state, AppState::Help);
}

#[tokio::test]
async fn a_key_after_two_seconds_clears_a_failure_toast_and_still_runs() {
    let mut app = file_list_showing(aged(
        ToastKind::Failure,
        FAILURE,
        Duration::from_millis(2500),
    ));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Char('?')), &mut terminal)
        .await
        .unwrap();

    assert!(footer_notice(&app).is_none());
    assert_eq!(app.state, AppState::Help);
}

#[tokio::test]
async fn esc_on_a_failure_toast_only_dismisses_it() {
    let mut app = file_list_showing(Toast::failure(FAILURE));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Esc), &mut terminal)
        .await
        .unwrap();

    assert!(footer_notice(&app).is_none());
    assert_eq!(app.state, AppState::FileList, "Esc must not go back");
    assert!(!app.should_quit, "Esc must not quit");
}

#[tokio::test]
async fn esc_on_a_failure_notice_does_not_leave_the_git_ops_screen() {
    let mut app = git_ops_showing(Some(Toast::failure("Nothing to undo")));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Esc), &mut terminal)
        .await
        .unwrap();

    assert_eq!(app.state, AppState::GitOpsSplitTree);
    assert!(git_ops_notice(&app).is_none());
}

#[tokio::test]
async fn esc_still_goes_back_on_a_screen_that_does_not_draw_the_toast() {
    let mut app = file_list_showing(Toast::failure(FAILURE));
    app.state = AppState::ChecksList;
    app.chk.checks_return_state = AppState::FileList;
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Esc), &mut terminal)
        .await
        .unwrap();

    assert_eq!(app.state, AppState::FileList);
}

// ---- a new toast replaces the old one and counts again by its own kind --

#[test]
fn a_new_failure_replaces_an_old_success_without_the_old_timer() {
    let mut app = file_list_showing(aged(
        ToastKind::Success,
        "Submitted",
        Duration::from_millis(4900),
    ));
    app.cmt.submission_result = Some(Toast::failure(FAILURE));

    app.poll_comment_submit_updates();

    let notice = footer_notice(&app).expect("the failure stays up");
    assert_eq!(notice.kind(), ToastKind::Failure);
    assert_eq!(notice.message(), FAILURE);
}

#[tokio::test]
async fn a_new_success_replaces_an_old_failure_and_the_next_key_clears_it() {
    let mut app = file_list_showing(aged(ToastKind::Failure, FAILURE, Duration::from_secs(10)));
    app.cmt.submission_result = Some(Toast::success("Submitted"));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Char('?')), &mut terminal)
        .await
        .unwrap();

    assert!(footer_notice(&app).is_none());
}

#[tokio::test]
async fn a_new_failure_restarts_the_two_second_guard() {
    let mut app = file_list_showing(aged(
        ToastKind::Failure,
        "old failure",
        Duration::from_secs(10),
    ));
    app.cmt.submission_result = Some(Toast::failure("new failure"));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Char('?')), &mut terminal)
        .await
        .unwrap();

    let notice = footer_notice(&app).expect("the new failure ignores a key in its first 2 seconds");
    assert_eq!(notice.message(), "new failure");
}

// ---- git ops notices follow the same rules -----------------------------

#[tokio::test]
async fn a_git_op_failure_is_not_removed_by_time() {
    let mut app = git_ops_showing(Some(aged(
        ToastKind::Failure,
        "Commit cancelled or failed",
        Duration::from_secs(60),
    )));

    app.poll_git_ops_updates();

    assert!(git_ops_notice(&app).is_some());
}

#[tokio::test]
async fn a_git_op_success_is_still_up_at_4_9_seconds() {
    let mut app = git_ops_showing(Some(aged(
        ToastKind::Success,
        "Commit created",
        Duration::from_millis(4900),
    )));

    app.poll_git_ops_updates();

    assert!(git_ops_notice(&app).is_some());
}

#[tokio::test]
async fn a_git_op_success_is_gone_after_5_seconds() {
    let mut app = git_ops_showing(Some(aged(
        ToastKind::Success,
        "Commit created",
        Duration::from_millis(5100),
    )));

    app.poll_git_ops_updates();

    assert!(git_ops_notice(&app).is_none());
}

#[tokio::test]
async fn a_git_op_success_is_cleared_by_the_next_key() {
    let mut app = git_ops_showing(Some(Toast::success("Staged all files")));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Char('j')), &mut terminal)
        .await
        .unwrap();

    assert!(git_ops_notice(&app).is_none());
}

#[tokio::test]
async fn a_git_op_failure_survives_a_key_within_two_seconds() {
    let mut app = git_ops_showing(Some(Toast::failure("Nothing to undo")));
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Char('j')), &mut terminal)
        .await
        .unwrap();

    assert!(git_ops_notice(&app).is_some());
}

// ---- which notice a git operation leaves behind -------------------------

#[tokio::test]
async fn a_finished_git_op_leaves_a_success_and_a_rejected_one_an_error() {
    let mut app = git_ops_showing(None);
    let (tx, rx) = mpsc::channel(1);
    app.git_ops_state.as_mut().unwrap().op_receiver = Some(rx);
    tx.send(Ok("Staged 2 file(s)".to_string())).await.unwrap();

    app.poll_git_ops_updates();

    let notice = git_ops_notice(&app).expect("the finished op leaves a notice");
    assert_eq!(notice.kind(), ToastKind::Success);
    assert_eq!(notice.message(), "Staged 2 file(s)");

    let (tx, rx) = mpsc::channel(1);
    app.git_ops_state.as_mut().unwrap().op_receiver = Some(rx);
    tx.send(Err("index.lock exists".to_string())).await.unwrap();

    app.poll_git_ops_updates();

    let notice = git_ops_notice(&app).expect("the rejected op leaves a notice");
    assert_eq!(notice.kind(), ToastKind::Failure);
    assert_eq!(notice.message(), "Error: index.lock exists");
}

#[tokio::test]
async fn committing_with_nothing_staged_is_a_failure_notice() {
    let mut app = git_ops_showing(None);
    let mut terminal = test_terminal();

    app.handle_key_event(press(KeyCode::Char('c')), &mut terminal)
        .await
        .unwrap();

    let notice = git_ops_notice(&app).expect("the refused commit says why");
    assert_eq!(notice.kind(), ToastKind::Failure);
    assert_eq!(notice.message(), "No staged files to commit");
}

#[test]
fn a_cancelled_shell_command_is_a_failure_notice() {
    let mut app = App::new_for_test();
    app.shell_state = Some(ShellState {
        input: "sleep 30".to_string(),
        cursor: 8,
        phase: ShellPhase::Cancelling,
        scroll_offset: 0,
    });

    app.poll_shell_result();

    let notice = footer_notice(&app).expect("the cancel says so");
    assert_eq!(notice.kind(), ToastKind::Failure);
    assert_eq!(notice.message(), "Shell command cancelled");
}
