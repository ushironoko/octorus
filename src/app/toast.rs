//! The footer toast: one line of feedback after an action, `✓` in green for a
//! success and `✗` in red for a failure (#250).
//!
//! The rules live here and nowhere else. A success leaves after 5 seconds, or
//! at the next key, whichever comes first. A failure has no timer. It leaves at
//! a key pressed more than 2 seconds after it appeared, so keys mashed around
//! the moment it appears do not clear it. While a failure is on screen, Esc
//! only dismisses it: Esc does not go back or quit.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};

use super::{App, AppState, ShellPhase};

/// How long a success toast stays when no key comes first.
const SUCCESS_LIFETIME: Duration = Duration::from_secs(5);

/// Keys pressed within this long after a failure appears leave it up. Esc is
/// the exception: it dismisses a failure on screen at once.
const FAILURE_KEY_GUARD: Duration = Duration::from_secs(2);

/// Whether a toast reports a success or a failure. The kind decides how long
/// the toast stays on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    /// The action finished.
    Success,
    /// The action failed, or could not be done.
    Failure,
}

/// One footer notice and the moment it appeared. A new toast replaces the old
/// one and starts its own clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    kind: ToastKind,
    message: String,
    shown_at: Instant,
}

impl Toast {
    /// A success toast, shown now.
    pub fn success(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Success, message)
    }

    /// A failure toast, shown now.
    pub fn failure(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Failure, message)
    }

    pub(crate) fn new(kind: ToastKind, message: impl Into<String>) -> Self {
        Self::at(kind, message, Instant::now())
    }

    /// A toast that appeared at `shown_at`.
    pub(crate) fn at(kind: ToastKind, message: impl Into<String>, shown_at: Instant) -> Self {
        Self {
            kind,
            message: message.into(),
            shown_at,
        }
    }

    pub fn kind(&self) -> ToastKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.shown_at)
    }
}

/// Clears `slot` when its success toast has run its 5 seconds by `now`.
/// Failures have no timer.
pub(crate) fn expire(slot: &mut Option<Toast>, now: Instant) {
    let timed_out = slot
        .as_ref()
        .is_some_and(|toast| toast.kind == ToastKind::Success && toast.age(now) > SUCCESS_LIFETIME);
    if timed_out {
        *slot = None;
    }
}

/// Applies one key press to `slot` at `now`. `drawn` says the footer on this
/// screen shows the slot.
///
/// Returns true when the key is spent: Esc on a failure that is drawn. The
/// failure is dismissed and the key must go no further, so it cannot Back or Quit.
pub(crate) fn apply_key(slot: &mut Option<Toast>, drawn: bool, esc: bool, now: Instant) -> bool {
    let Some(toast) = slot.as_ref() else {
        return false;
    };
    match toast.kind {
        ToastKind::Success => {
            *slot = None;
            false
        }
        ToastKind::Failure => {
            if esc && drawn {
                *slot = None;
                return true;
            }
            if toast.age(now) > FAILURE_KEY_GUARD {
                *slot = None;
            }
            false
        }
    }
}

impl App {
    /// Settles the footer notices against a key press, before the key is
    /// handled. Returns true when the key is spent on a failure notice (Esc),
    /// in which case it must not be handled at all.
    pub(crate) fn settle_toasts_on_key(&mut self, key: &KeyEvent) -> bool {
        let now = Instant::now();
        let esc = key.code == KeyCode::Esc;
        let submission_drawn = self.submission_toast_drawn();
        let op_drawn = self.git_op_toast_drawn();
        let submission_spent =
            apply_key(&mut self.cmt.submission_result, submission_drawn, esc, now);
        let op_spent = self
            .git_ops_state
            .as_mut()
            .is_some_and(|ops| apply_key(&mut ops.op_message, op_drawn, esc, now));
        submission_spent || op_spent
    }

    /// Whether the footer on this screen shows the submission notice.
    fn submission_toast_drawn(&self) -> bool {
        self.state.draws_submission_toast() && !self.footer_taken_over()
    }

    /// Whether the git ops tree footer shows its notice. A submission notice
    /// takes that footer first, and a push or a pending confirmation replaces it.
    fn git_op_toast_drawn(&self) -> bool {
        let Some(ops) = self.git_ops_state.as_ref() else {
            return false;
        };
        matches!(
            self.state,
            AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff
        ) && !self.footer_taken_over()
            && !ops.pushing
            && ops.pending_confirm.is_none()
            && !(self.cmt.submission_result.is_some() && self.submission_toast_drawn())
    }

    /// Overlays that take over the footer: the shell prompt, the approve
    /// prompt, and a comment or review submit in flight.
    fn footer_taken_over(&self) -> bool {
        self.shell_state
            .as_ref()
            .is_some_and(|shell| matches!(shell.phase, ShellPhase::Input))
            || self.is_pending_approve_confirmation()
            || self.is_submitting_comment()
            || self.is_issue_comment_submitting()
            || self.is_submitting_review()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aged(kind: ToastKind, age: Duration) -> Option<Toast> {
        let shown_at = Instant::now()
            .checked_sub(age)
            .expect("clock is old enough");
        Some(Toast::at(kind, "message", shown_at))
    }

    #[test]
    fn a_success_expires_only_after_its_lifetime() {
        let now = Instant::now();
        let mut slot = Some(Toast::at(ToastKind::Success, "ok", now));
        expire(&mut slot, now + Duration::from_secs(5));
        assert!(slot.is_some(), "exactly 5 seconds is still up");
        expire(&mut slot, now + Duration::from_millis(5001));
        assert!(slot.is_none());
    }

    #[test]
    fn a_failure_never_expires_on_its_own() {
        let now = Instant::now();
        let mut slot = Some(Toast::at(ToastKind::Failure, "boom", now));
        expire(&mut slot, now + Duration::from_secs(3600));
        assert!(slot.is_some());
    }

    #[test]
    fn a_key_on_a_success_clears_it_whether_or_not_drawn() {
        let mut slot = aged(ToastKind::Success, Duration::ZERO);
        assert!(!apply_key(&mut slot, false, false, Instant::now()));
        assert!(slot.is_none());
    }

    #[test]
    fn a_key_on_a_failure_waits_out_the_guard() {
        let mut slot = aged(ToastKind::Failure, Duration::from_secs(2));
        assert!(!apply_key(&mut slot, true, false, Instant::now()));
        assert!(slot.is_none(), "2 seconds is past the guard");

        let mut slot = aged(ToastKind::Failure, Duration::from_millis(1990));
        assert!(!apply_key(&mut slot, true, false, Instant::now()));
        assert!(slot.is_some());
    }

    #[test]
    fn esc_spends_a_drawn_failure_at_once() {
        let mut slot = aged(ToastKind::Failure, Duration::ZERO);
        assert!(apply_key(&mut slot, true, true, Instant::now()));
        assert!(slot.is_none());
    }

    #[test]
    fn esc_does_not_spend_a_failure_that_is_not_drawn() {
        let mut slot = aged(ToastKind::Failure, Duration::ZERO);
        assert!(!apply_key(&mut slot, false, true, Instant::now()));
        assert!(slot.is_some());
    }
}
