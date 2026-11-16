//! The screen after a batched viewed-mark: flags follow the paths GitHub kept.

use std::time::Duration;

use serial_test::serial;

use super::{App, DataState, ToastKind};
use crate::github::viewed::fake::ViewedFakeGh;
use crate::github::{Branch, ChangedFile, PullRequest, User};

fn file(name: &str, viewed: bool) -> ChangedFile {
    ChangedFile {
        filename: name.to_string(),
        status: "modified".to_string(),
        additions: 1,
        deletions: 0,
        patch: None,
        viewed,
    }
}

fn loaded(pr_node_id: &str, files: Vec<ChangedFile>) -> App {
    let mut app = App::new_for_test();
    app.pr_number = Some(193);
    app.data_state = DataState::Loaded {
        pr: Box::new(PullRequest {
            number: 193,
            node_id: Some(pr_node_id.to_string()),
            title: "Test".to_string(),
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
                login: "user".to_string(),
            },
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }),
        files,
    };
    app
}

fn viewed_flags(app: &App) -> Vec<bool> {
    match &app.data_state {
        DataState::Loaded { files, .. } => files.iter().map(|file| file.viewed).collect(),
        DataState::Loading => panic!("expected loaded files"),
        DataState::Error(message) => panic!("expected loaded files, got error {message}"),
    }
}

async fn finish_mark(app: &mut App) {
    for _ in 0..200 {
        app.poll_mark_viewed_updates();
        if app.mark_viewed_receiver.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for viewed marks");
}

#[tokio::test]
#[serial]
async fn marking_three_files_updates_every_flag_in_one_gh_call() {
    let pr = "PR_viewed_ui_three_193";
    let gh = ViewedFakeGh::install(pr);
    let mut app = loaded(
        pr,
        vec![
            file("src/a.rs", false),
            file("src/b.rs", false),
            file("src/c.rs", false),
        ],
    );

    app.start_mark_paths_as_viewed(
        vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
        ],
        true,
    );
    finish_mark(&mut app).await;

    assert_eq!(viewed_flags(&app), vec![true, true, true]);
    let toast = app.cmt.submission_result.as_ref().expect("toast");
    assert_eq!(toast.kind(), ToastKind::Success);
    assert_eq!(toast.message(), "Marked 3 file(s) as viewed");
    assert_eq!(
        gh.scoped_calls().len(),
        1,
        "main issued 3 gh calls for these files"
    );
}

#[tokio::test]
#[serial]
async fn a_partial_batch_shows_only_the_files_that_stay_viewed() {
    let pr = "PR_viewed_ui_partial_193";
    let gh = ViewedFakeGh::install(pr);
    gh.fail_path("src/b.rs");
    let mut app = loaded(
        pr,
        vec![
            file("src/a.rs", false),
            file("src/b.rs", false),
            file("src/c.rs", false),
        ],
    );

    app.start_mark_paths_as_viewed(
        vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
        ],
        true,
    );
    finish_mark(&mut app).await;

    // src/c.rs was applied by the alias batch and then undone. The failed file
    // stays unviewed. main would have marked only src/a.rs; the screen matches.
    assert_eq!(viewed_flags(&app), vec![true, false, false]);
    let toast = app.cmt.submission_result.as_ref().expect("toast");
    assert_eq!(toast.kind(), ToastKind::Failure);
    assert_eq!(
        toast.message(),
        "Marked 1/3 files as viewed, then failed: src/b.rs: File not changed"
    );
    assert_eq!(
        gh.scoped_calls().len(),
        2,
        "main=2 (stop at src/b.rs); batch mark + undo of src/c.rs"
    );
}

#[tokio::test]
#[serial]
async fn a_file_whose_undo_fails_stays_viewed_on_screen() {
    let pr = "PR_viewed_ui_undo_193";
    let gh = ViewedFakeGh::install(pr);
    gh.fail_path("src/b.rs");
    gh.fail_revert();
    let mut app = loaded(
        pr,
        vec![
            file("src/a.rs", false),
            file("src/b.rs", false),
            file("src/c.rs", false),
        ],
    );

    app.start_mark_paths_as_viewed(
        vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
        ],
        true,
    );
    finish_mark(&mut app).await;

    assert_eq!(viewed_flags(&app), vec![true, false, true]);
    let toast = app.cmt.submission_result.as_ref().expect("toast");
    assert_eq!(toast.kind(), ToastKind::Failure);
    assert_eq!(
        toast.message(),
        "Marked 2/3 files as viewed, then failed: src/b.rs: File not changed"
    );
    assert_eq!(
        gh.scoped_calls().len(),
        2,
        "one mark batch and one refused undo; main would have stopped after 2 calls"
    );
}

#[tokio::test]
#[serial]
async fn unviewing_one_file_still_clears_that_flag() {
    let pr = "PR_viewed_ui_unview_193";
    let gh = ViewedFakeGh::install(pr);
    let mut app = loaded(pr, vec![file("src/a.rs", true), file("src/b.rs", true)]);

    app.start_mark_paths_as_viewed(vec!["src/a.rs".to_string()], false);
    finish_mark(&mut app).await;

    assert_eq!(viewed_flags(&app), vec![false, true]);
    let toast = app.cmt.submission_result.as_ref().expect("toast");
    assert_eq!(toast.kind(), ToastKind::Success);
    assert_eq!(toast.message(), "Marked 1 file(s) as unviewed");
    let calls = gh.scoped_calls();
    assert_eq!(calls.len(), 1, "main issued 1 gh call to unview one file");
    assert!(calls[0].contains("unmarkFileAsViewed"), "{}", calls[0]);
    assert!(calls[0].contains("path=src/a.rs"), "{}", calls[0]);
}
