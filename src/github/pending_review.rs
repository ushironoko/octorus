//! Inline review comments go to the viewer's PENDING review on GitHub and are
//! published together when a review is submitted (issue #169).
//!
//! GitHub keeps one pending review per user and pull request. A new draft is
//! appended to that review when one exists; otherwise a pending review is
//! created with the draft as its first comment. Submitting publishes whatever
//! the pending review holds, and falls back to `gh pr review` when there is no
//! pending review.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::client::{check_graphql_errors, gh_api_paginate, gh_api_post_json};
use super::short_sha;
use crate::app::ReviewAction;

/// One inline comment the viewer is adding.
///
/// Lines are new-file numbers on the RIGHT side. The TUI only comments on added
/// and context lines, so every draft targets that side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineDraft {
    pub path: String,
    pub body: String,
    /// The line itself, or the last line of a multi-line range.
    pub line: u32,
    /// The first line of a multi-line range. `None` for a single-line comment.
    pub start_line: Option<u32>,
}

/// The viewer's PENDING review on one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingReview {
    /// REST id, used by the submit endpoint.
    pub id: u64,
    /// GraphQL node id, used to attach a thread to the review.
    pub node_id: String,
    /// The commit the review was started on.
    pub commit_id: Option<String>,
}

/// One entry of `GET /repos/{repo}/pulls/{n}/reviews`. Only the fields the
/// pending-review logic reads are kept.
#[derive(Debug, Deserialize)]
pub(crate) struct ReviewListEntry {
    pub id: u64,
    #[serde(default)]
    pub node_id: Option<String>,
    pub state: String,
    #[serde(default)]
    pub commit_id: Option<String>,
}

/// What adding one draft does, decided from the viewer's current pending review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DraftRoute {
    /// No pending review exists. Create one that holds the draft.
    CreatePending,
    /// Add the draft to the existing pending review.
    AppendTo { review_node_id: String },
    /// The pending review was started on an older commit than the PR head.
    /// Its line numbers resolve against the old file, so the draft is refused
    /// rather than placed on a shifted line.
    OlderCommit { review_commit: String },
}

/// How a review submit is carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubmitRoute {
    /// Publish the pending review and every draft in it.
    PublishPending { review_id: u64 },
    /// No pending review exists. Create and submit a review in one step.
    CreateAndSubmit,
}

/// Pick the PENDING review out of a review list. GitHub returns at most one per
/// viewer and pull request, so the first match is the one.
fn select_pending_review(entries: Vec<ReviewListEntry>) -> Result<Option<PendingReview>> {
    let Some(entry) = entries.into_iter().find(|entry| entry.state == "PENDING") else {
        return Ok(None);
    };
    let node_id = entry
        .node_id
        .context("pending review has no node id, so comments cannot be attached to it")?;
    Ok(Some(PendingReview {
        id: entry.id,
        node_id,
        commit_id: entry.commit_id,
    }))
}

pub(crate) fn draft_route(pending: Option<&PendingReview>, head_sha: &str) -> DraftRoute {
    match pending {
        None => DraftRoute::CreatePending,
        Some(review) => match review.commit_id.as_deref() {
            Some(commit) if commit != head_sha => DraftRoute::OlderCommit {
                review_commit: commit.to_string(),
            },
            _ => DraftRoute::AppendTo {
                review_node_id: review.node_id.clone(),
            },
        },
    }
}

pub(crate) fn submit_route(pending: Option<&PendingReview>) -> SubmitRoute {
    match pending {
        Some(review) => SubmitRoute::PublishPending {
            review_id: review.id,
        },
        None => SubmitRoute::CreateAndSubmit,
    }
}

/// The `event` value GitHub expects for a review action.
pub(crate) fn review_event(action: ReviewAction) -> &'static str {
    match action {
        ReviewAction::Approve => "APPROVE",
        ReviewAction::RequestChanges => "REQUEST_CHANGES",
        ReviewAction::Comment => "COMMENT",
    }
}

/// Body for `POST /repos/{repo}/pulls/{n}/reviews` with the `event` key left out.
/// GitHub keeps a review without an event PENDING.
pub(crate) fn create_pending_review_body(head_sha: &str, draft: &InlineDraft) -> Value {
    json!({
        "commit_id": head_sha,
        "comments": [draft_comment(draft)],
    })
}

/// One entry of a review's `comments` array. Positions are the new-file lines
/// on the RIGHT side, which the TUI always targets.
fn draft_comment(draft: &InlineDraft) -> Value {
    let mut comment = json!({
        "path": draft.path,
        "body": draft.body,
        "line": draft.line,
        "side": "RIGHT",
    });
    if let Some(start_line) = draft.start_line {
        comment["start_line"] = json!(start_line);
        comment["start_side"] = json!("RIGHT");
    }
    comment
}

/// Attaches one thread to a pending review. `pullRequestReviewId` names the
/// review that receives the thread, so the draft never lands on a review GitHub
/// picks by itself.
const ADD_THREAD_MUTATION: &str = r#"
mutation(
  $pullRequestId: ID!,
  $pullRequestReviewId: ID!,
  $path: String!,
  $body: String!,
  $line: Int!,
  $side: DiffSide!,
  $startLine: Int,
  $startSide: DiffSide
) {
  addPullRequestReviewThread(input: {
    pullRequestId: $pullRequestId,
    pullRequestReviewId: $pullRequestReviewId,
    path: $path,
    body: $body,
    line: $line,
    side: $side,
    startLine: $startLine,
    startSide: $startSide
  }) {
    thread { id }
  }
}
"#;

/// GraphQL request that attaches one thread to an existing pending review.
pub(crate) fn add_thread_request(
    pr_node_id: &str,
    review_node_id: &str,
    draft: &InlineDraft,
) -> Value {
    let mut variables = json!({
        "pullRequestId": pr_node_id,
        "pullRequestReviewId": review_node_id,
        "path": draft.path,
        "body": draft.body,
        "line": draft.line,
        "side": "RIGHT",
    });
    if let Some(start_line) = draft.start_line {
        variables["startLine"] = json!(start_line);
        variables["startSide"] = json!("RIGHT");
    }
    json!({ "query": ADD_THREAD_MUTATION, "variables": variables })
}

/// Body for `POST /repos/{repo}/pulls/{n}/reviews/{id}/events`.
pub(crate) fn submit_event_body(action: ReviewAction, body: &str) -> Value {
    json!({ "event": review_event(action), "body": body })
}

async fn list_reviews(repo: &str, pr_number: u32) -> Result<Vec<ReviewListEntry>> {
    let endpoint = format!("repos/{repo}/pulls/{pr_number}/reviews?per_page=100");
    let json = gh_api_paginate(&endpoint).await?;
    serde_json::from_value(json).context("Failed to parse reviews response")
}

/// The viewer's PENDING review on this pull request, if there is one.
pub(crate) async fn find_pending_review(
    repo: &str,
    pr_number: u32,
) -> Result<Option<PendingReview>> {
    select_pending_review(list_reviews(repo, pr_number).await?)
}

/// Add one inline draft to the viewer's pending review. Creates the review if
/// there is none.
pub async fn add_inline_draft(
    repo: &str,
    pr_number: u32,
    pr_node_id: Option<&str>,
    head_sha: &str,
    draft: &InlineDraft,
) -> Result<()> {
    let pending = find_pending_review(repo, pr_number).await?;
    match draft_route(pending.as_ref(), head_sha) {
        DraftRoute::CreatePending => {
            let endpoint = format!("repos/{repo}/pulls/{pr_number}/reviews");
            let created =
                gh_api_post_json(&endpoint, &create_pending_review_body(head_sha, draft)).await?;
            // The point of this path is that the comment is not published on its
            // own. Check that GitHub kept the review pending.
            if created.get("state").and_then(Value::as_str) != Some("PENDING") {
                anyhow::bail!("GitHub did not keep the comment in a pending review");
            }
            Ok(())
        }
        DraftRoute::AppendTo { review_node_id } => {
            let pr_node_id = pr_node_id.context(
                "pull request node id is unavailable, so the comment cannot join the pending review",
            )?;
            let request = add_thread_request(pr_node_id, &review_node_id, draft);
            let response = gh_api_post_json("graphql", &request).await?;
            check_graphql_errors(&response)?;
            if response
                .pointer("/data/addPullRequestReviewThread/thread/id")
                .is_none()
            {
                anyhow::bail!("GitHub did not confirm the comment joined the pending review");
            }
            Ok(())
        }
        DraftRoute::OlderCommit { review_commit } => anyhow::bail!(
            "Your pending review is on commit {} but the PR head is {}. Submit the pending review first, then comment again.",
            short_sha(&review_commit),
            short_sha(head_sha)
        ),
    }
}

/// Submit a review. Publishes the viewer's pending review with its drafts when
/// there is one. Otherwise creates and submits a review with `gh pr review`.
pub async fn submit_review_publishing_pending(
    repo: &str,
    pr_number: u32,
    action: ReviewAction,
    body: &str,
) -> Result<()> {
    let pending = find_pending_review(repo, pr_number).await?;
    match submit_route(pending.as_ref()) {
        SubmitRoute::PublishPending { review_id } => {
            let endpoint = format!("repos/{repo}/pulls/{pr_number}/reviews/{review_id}/events");
            gh_api_post_json(&endpoint, &submit_event_body(action, body)).await?;
            Ok(())
        }
        SubmitRoute::CreateAndSubmit => {
            super::pr::submit_review(repo, pr_number, action, body).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(value: Value) -> ReviewListEntry {
        serde_json::from_value(value).unwrap()
    }

    fn draft(line: u32, start_line: Option<u32>) -> InlineDraft {
        InlineDraft {
            path: "src/app.rs".to_string(),
            body: "Consider a match here.".to_string(),
            line,
            start_line,
        }
    }

    fn pending(commit: Option<&str>) -> PendingReview {
        PendingReview {
            id: 77,
            node_id: "PRR_pending".to_string(),
            commit_id: commit.map(str::to_string),
        }
    }

    #[test]
    fn select_pending_review_skips_submitted_reviews() {
        let entries = vec![
            entry(json!({"id": 1, "node_id": "PRR_1", "state": "APPROVED", "commit_id": "aaa"})),
            entry(json!({"id": 2, "node_id": "PRR_2", "state": "PENDING", "commit_id": "bbb"})),
        ];
        assert_eq!(
            select_pending_review(entries).unwrap(),
            Some(PendingReview {
                id: 2,
                node_id: "PRR_2".to_string(),
                commit_id: Some("bbb".to_string()),
            })
        );
    }

    #[test]
    fn select_pending_review_is_none_when_no_review_is_pending() {
        let entries = vec![entry(
            json!({"id": 1, "node_id": "PRR_1", "state": "COMMENTED", "commit_id": "aaa"}),
        )];
        assert_eq!(select_pending_review(entries).unwrap(), None);
        assert_eq!(select_pending_review(Vec::new()).unwrap(), None);
    }

    #[test]
    fn select_pending_review_errors_when_the_pending_review_has_no_node_id() {
        let entries = vec![entry(
            json!({"id": 2, "state": "PENDING", "commit_id": "bbb"}),
        )];
        let err = select_pending_review(entries).unwrap_err();
        assert!(err.to_string().contains("no node id"), "{err}");
    }

    #[test]
    fn draft_route_creates_a_pending_review_when_none_exists() {
        assert_eq!(draft_route(None, "head"), DraftRoute::CreatePending);
    }

    #[test]
    fn draft_route_appends_when_the_pending_review_is_on_the_head() {
        assert_eq!(
            draft_route(Some(&pending(Some("head"))), "head"),
            DraftRoute::AppendTo {
                review_node_id: "PRR_pending".to_string(),
            }
        );
    }

    #[test]
    fn draft_route_appends_when_the_pending_review_commit_is_unknown() {
        assert_eq!(
            draft_route(Some(&pending(None)), "head"),
            DraftRoute::AppendTo {
                review_node_id: "PRR_pending".to_string(),
            }
        );
    }

    #[test]
    fn draft_route_refuses_to_place_a_draft_on_a_newer_head() {
        assert_eq!(
            draft_route(Some(&pending(Some("old"))), "new"),
            DraftRoute::OlderCommit {
                review_commit: "old".to_string(),
            }
        );
    }

    #[test]
    fn submit_route_publishes_an_existing_pending_review_or_creates_one() {
        assert_eq!(
            submit_route(Some(&pending(Some("head")))),
            SubmitRoute::PublishPending { review_id: 77 }
        );
        assert_eq!(submit_route(None), SubmitRoute::CreateAndSubmit);
    }

    #[test]
    fn review_event_maps_each_action_to_its_api_value() {
        assert_eq!(review_event(ReviewAction::Approve), "APPROVE");
        assert_eq!(
            review_event(ReviewAction::RequestChanges),
            "REQUEST_CHANGES"
        );
        assert_eq!(review_event(ReviewAction::Comment), "COMMENT");
    }

    #[test]
    fn create_pending_review_body_leaves_out_the_event() {
        let body = create_pending_review_body("head", &draft(42, None));
        assert_eq!(
            body,
            json!({
                "commit_id": "head",
                "comments": [{
                    "path": "src/app.rs",
                    "body": "Consider a match here.",
                    "line": 42,
                    "side": "RIGHT",
                }],
            })
        );
        assert!(body.get("event").is_none());
    }

    #[test]
    fn create_pending_review_body_carries_the_range_for_multiline_drafts() {
        let body = create_pending_review_body("head", &draft(42, Some(40)));
        assert_eq!(body["comments"][0]["start_line"], json!(40));
        assert_eq!(body["comments"][0]["start_side"], json!("RIGHT"));
        assert_eq!(body["comments"][0]["line"], json!(42));
    }

    #[test]
    fn add_thread_request_targets_the_pending_review_by_node_id() {
        let request = add_thread_request("PR_node", "PRR_pending", &draft(42, None));
        let variables = &request["variables"];
        assert_eq!(variables["pullRequestId"], json!("PR_node"));
        assert_eq!(variables["pullRequestReviewId"], json!("PRR_pending"));
        assert_eq!(variables["path"], json!("src/app.rs"));
        assert_eq!(variables["line"], json!(42));
        assert_eq!(variables["side"], json!("RIGHT"));
        assert!(variables.get("startLine").is_none());
        assert!(variables.get("startSide").is_none());
        assert!(request["query"]
            .as_str()
            .unwrap()
            .contains("addPullRequestReviewThread"));
    }

    #[test]
    fn add_thread_request_sends_the_start_of_a_multiline_range() {
        let request = add_thread_request("PR_node", "PRR_pending", &draft(42, Some(40)));
        assert_eq!(request["variables"]["startLine"], json!(40));
        assert_eq!(request["variables"]["startSide"], json!("RIGHT"));
    }

    #[test]
    fn request_bodies_keep_gh_looking_text_verbatim() {
        // `gh api -F` would read `@notes.txt` as a file. The JSON body must not.
        let tricky = InlineDraft {
            path: "a b.rs".to_string(),
            body: "@notes.txt\n日本語 `x` a=b".to_string(),
            line: 1,
            start_line: None,
        };
        let expected = json!("@notes.txt\n日本語 `x` a=b");
        assert_eq!(
            create_pending_review_body("head", &tricky)["comments"][0]["body"],
            expected
        );
        assert_eq!(
            add_thread_request("PR_node", "PRR_pending", &tricky)["variables"]["body"],
            expected
        );
    }

    #[test]
    fn submit_event_body_names_the_action_and_keeps_the_body() {
        assert_eq!(
            submit_event_body(ReviewAction::Approve, ""),
            json!({"event": "APPROVE", "body": ""})
        );
        assert_eq!(
            submit_event_body(ReviewAction::RequestChanges, "Fix it"),
            json!({"event": "REQUEST_CHANGES", "body": "Fix it"})
        );
    }
}

/// The two entry points run against the `gh` test double in
/// `tests/fixtures/fake-gh`, so each test checks which endpoint a branch writes to.
#[cfg(all(test, unix))]
mod fake_gh_tests {
    use super::*;
    use serial_test::serial;

    const FAKE_GH_VARS: [&str; 6] = [
        "FAKE_GH_LOG",
        "FAKE_GH_REVIEWS",
        "FAKE_GH_CREATE",
        "FAKE_GH_GRAPHQL",
        "FAKE_GH_PUBLISH",
        "FAKE_GH_PR_REVIEW",
    ];

    /// Puts the test double first on PATH and sets its replies. Dropping it
    /// restores PATH and clears the variables.
    struct FakeGh {
        _dir: tempfile::TempDir,
        log: std::path::PathBuf,
        old_path: Option<std::ffi::OsString>,
    }

    impl FakeGh {
        fn install(reviews: &str, create: &str, graphql: &str, publish: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("calls.log");
            let old_path = std::env::var_os("PATH");
            let mut path = std::ffi::OsString::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/fake-gh"
            ));
            path.push(":");
            path.push(old_path.clone().unwrap_or_default());
            std::env::set_var("PATH", path);
            std::env::set_var("FAKE_GH_LOG", &log);
            std::env::set_var("FAKE_GH_REVIEWS", reviews);
            std::env::set_var("FAKE_GH_CREATE", create);
            std::env::set_var("FAKE_GH_GRAPHQL", graphql);
            std::env::set_var("FAKE_GH_PUBLISH", publish);
            std::env::set_var("FAKE_GH_PR_REVIEW", "");
            Self {
                _dir: dir,
                log,
                old_path,
            }
        }

        /// Every call made so far, as logged by the test double.
        fn calls(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    impl Drop for FakeGh {
        fn drop(&mut self) {
            match &self.old_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
            for name in FAKE_GH_VARS {
                std::env::remove_var(name);
            }
        }
    }

    fn draft() -> InlineDraft {
        InlineDraft {
            path: "src/app.rs".to_string(),
            body: "Consider a match here.".to_string(),
            line: 42,
            start_line: None,
        }
    }

    #[tokio::test]
    #[serial]
    async fn add_opens_a_pending_review_when_none_exists() {
        let gh = FakeGh::install("[[]]", r#"{"id":9,"state":"PENDING"}"#, "", "");

        add_inline_draft("o/r", 7, Some("PR_node"), "head", &draft())
            .await
            .unwrap();

        let calls = gh.calls();
        assert!(
            calls.contains("CALL api --method POST repos/o/r/pulls/7/reviews --header"),
            "{calls}"
        );
        assert!(calls.contains(r#""commit_id":"head""#), "{calls}");
        assert!(!calls.contains(r#""event""#), "{calls}");
    }

    #[tokio::test]
    #[serial]
    async fn add_appends_to_the_pending_review_on_the_head() {
        let gh = FakeGh::install(
            r#"[[{"id":77,"node_id":"PRR_pending","state":"PENDING","commit_id":"head"}]]"#,
            "",
            r#"{"data":{"addPullRequestReviewThread":{"thread":{"id":"PRRT_1"}}}}"#,
            "",
        );

        add_inline_draft("o/r", 7, Some("PR_node"), "head", &draft())
            .await
            .unwrap();

        let calls = gh.calls();
        assert!(
            calls.contains("CALL api --method POST graphql --header"),
            "{calls}"
        );
        assert!(
            calls.contains(r#""pullRequestReviewId":"PRR_pending""#),
            "{calls}"
        );
        assert!(calls.contains(r#""pullRequestId":"PR_node""#), "{calls}");
        assert!(
            !calls.contains("/pulls/7/reviews --header"),
            "a second pending review must not be created: {calls}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn add_is_refused_without_a_write_when_the_pending_review_is_older() {
        let gh = FakeGh::install(
            r#"[[{"id":77,"node_id":"PRR_pending","state":"PENDING","commit_id":"old1234567"}]]"#,
            "",
            "",
            "",
        );

        let err = add_inline_draft("o/r", 7, Some("PR_node"), "new7654321", &draft())
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("pending review is on commit old"),
            "{err}"
        );
        let calls = gh.calls();
        assert!(
            !calls.contains("--method POST"),
            "nothing may be written: {calls}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn submit_publishes_the_pending_review_by_its_id() {
        let gh = FakeGh::install(
            r#"[[{"id":77,"node_id":"PRR_pending","state":"PENDING","commit_id":"head"}]]"#,
            "",
            "",
            r#"{"id":77,"state":"APPROVED"}"#,
        );

        submit_review_publishing_pending("o/r", 7, ReviewAction::Approve, "Looks good")
            .await
            .unwrap();

        let calls = gh.calls();
        assert!(
            calls.contains("CALL api --method POST repos/o/r/pulls/7/reviews/77/events --header"),
            "{calls}"
        );
        assert!(calls.contains(r#""event":"APPROVE""#), "{calls}");
        assert!(calls.contains(r#""body":"Looks good""#), "{calls}");
        assert!(!calls.contains("CALL pr review"), "{calls}");
    }

    #[tokio::test]
    #[serial]
    async fn submit_falls_back_to_gh_pr_review_when_nothing_is_pending() {
        let gh = FakeGh::install(
            r#"[[{"id":1,"node_id":"PRR_1","state":"APPROVED","commit_id":"head"}]]"#,
            "",
            "",
            "",
        );

        submit_review_publishing_pending("o/r", 7, ReviewAction::RequestChanges, "Fix it")
            .await
            .unwrap();

        let calls = gh.calls();
        assert!(
            calls.contains("CALL pr review 7 --request-changes -b Fix it -R o/r"),
            "{calls}"
        );
        assert!(!calls.contains("/events"), "{calls}");
    }
}
