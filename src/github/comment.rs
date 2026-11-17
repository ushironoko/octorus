use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::client::{gh_api_paginate, gh_api_post, FieldValue};
use super::pr::User;
use crate::diff::SideLine;

/// ジェネリックなfetch & parse関数（ページネーション対応）
async fn fetch_and_parse<T: DeserializeOwned>(
    endpoint: &str,
    error_context: &'static str,
) -> Result<T> {
    let json = gh_api_paginate(endpoint).await?;
    serde_json::from_value(json).context(error_context)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewCommentLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_position: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_type: Option<String>,
    /// Set by the TUI when it reads the viewer's pending review. GitHub's JSON
    /// does not carry it, so it is never serialized or deserialized.
    #[serde(skip)]
    pub publication: CommentPublication,
}

/// Whether a review comment is published, or a draft in the viewer's pending
/// review (issue #169). A draft is published only when that review is submitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommentPublication {
    #[default]
    Published,
    PendingDraft,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub path: String,
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    pub body: String,
    pub user: User,
    pub created_at: String,
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    #[serde(flatten)]
    pub location: ReviewCommentLocation,
}

impl ReviewComment {
    /// Whether this comment is a draft in the viewer's pending review.
    pub fn is_pending_draft(&self) -> bool {
        self.location.publication == CommentPublication::PendingDraft
    }

    /// File lines this comment covers, as `12` or `10-12`.
    ///
    /// `start_line` is the first line of a multi-line comment. A start that is
    /// missing, or not above the end line, is a single line. `None` when the end
    /// line itself is unknown.
    pub fn line_span(&self) -> Option<String> {
        let end = self.line?;
        match self.start_line {
            Some(start) if start < end => Some(format!("{start}-{end}")),
            _ => Some(end.to_string()),
        }
    }

    /// The panel's `(line …)` label. A comment with no line shows `0`, which is
    /// what the panel showed before a span existed.
    pub fn line_label(&self) -> String {
        self.line_span().unwrap_or_else(|| "0".to_string())
    }
}

pub async fn fetch_review_comments(repo: &str, pr_number: u32) -> Result<Vec<ReviewComment>> {
    fetch_and_parse(
        &format!("repos/{}/pulls/{}/comments?per_page=100", repo, pr_number),
        "Failed to parse review comments response",
    )
    .await
}

/// ディスカッションコメント（PRの会話タブのコメント）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscussionComment {
    pub id: u64,
    pub body: String,
    pub user: User,
    pub created_at: String,
}

pub async fn fetch_discussion_comments(
    repo: &str,
    pr_number: u32,
) -> Result<Vec<DiscussionComment>> {
    fetch_and_parse(
        &format!("repos/{}/issues/{}/comments?per_page=100", repo, pr_number),
        "Failed to parse discussion comments response",
    )
    .await
}

/// PR レビュー（全体コメント）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub id: u64,
    pub body: Option<String>,
    pub state: String,
    pub user: User,
    pub submitted_at: Option<String>,
}

pub async fn fetch_reviews(repo: &str, pr_number: u32) -> Result<Vec<Review>> {
    fetch_and_parse(
        &format!("repos/{}/pulls/{}/reviews?per_page=100", repo, pr_number),
        "Failed to parse reviews response",
    )
    .await
}

/// One field of `POST /repos/{owner}/{repo}/pulls/{n}/comments`.
///
/// `raw` is `gh api -F` (a number). Otherwise the field is `gh api -f` (a string).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiField {
    key: &'static str,
    value: String,
    raw: bool,
}

impl ApiField {
    fn string(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: value.into(),
            raw: false,
        }
    }

    fn raw_number(key: &'static str, value: u32) -> Self {
        Self {
            key,
            value: value.to_string(),
            raw: true,
        }
    }
}

/// `commit_id`, `path`, `body`, `line`, `side` for one line.
///
/// `line.side` becomes GitHub's `RIGHT` (new file: added or context) or `LEFT`
/// (old file: deleted) here, where the request is built. `subject_type` and the
/// deprecated `position` are not included.
fn single_line_review_comment_fields(
    commit_id: &str,
    path: &str,
    body: &str,
    line: SideLine,
) -> Vec<ApiField> {
    vec![
        ApiField::string("body", body),
        ApiField::string("commit_id", commit_id),
        ApiField::string("path", path),
        ApiField::raw_number("line", line.line),
        ApiField::string("side", line.side.as_github_side()),
    ]
}

/// `start_line`/`start_side` plus `line`/`side` for a range.
///
/// Same keys `create_multiline_review_comment` has always posted. Each end's
/// side is converted to `LEFT`/`RIGHT` here. `subject_type` and `position` are
/// not included.
fn multiline_review_comment_fields(
    commit_id: &str,
    path: &str,
    body: &str,
    start: SideLine,
    end: SideLine,
) -> Vec<ApiField> {
    vec![
        ApiField::string("body", body),
        ApiField::string("commit_id", commit_id),
        ApiField::string("path", path),
        ApiField::raw_number("start_line", start.line),
        ApiField::raw_number("line", end.line),
        ApiField::string("start_side", start.side.as_github_side()),
        ApiField::string("side", end.side.as_github_side()),
    ]
}

async fn post_pull_comment(
    repo: &str,
    pr_number: u32,
    fields: &[ApiField],
    error_context: &'static str,
) -> Result<ReviewComment> {
    let endpoint = format!("repos/{repo}/pulls/{pr_number}/comments");
    let gh_fields: Vec<(&str, FieldValue<'_>)> = fields
        .iter()
        .map(|field| {
            let value = if field.raw {
                FieldValue::Raw(field.value.as_str())
            } else {
                FieldValue::String(field.value.as_str())
            };
            (field.key, value)
        })
        .collect();
    let json = gh_api_post(&endpoint, &gh_fields).await?;
    serde_json::from_value(json).context(error_context)
}

/// 単一行のレビューコメントを作成する。
///
/// `POST /repos/{owner}/{repo}/pulls/{n}/comments` に `commit_id`, `path`,
/// `body`, `line`, `side` を送る。`line` はそのファイルの行番号で、side が
/// 右なら新しいファイル（追加行・コンテキスト行）、左なら古いファイル（削除行）
/// の番号を渡す。`LEFT`/`RIGHT` への変換はリクエストを組むときだけ行う。
///
/// NOTE: `subject_type` は送らない。`line`/`side` と一緒に送ると oneOf で
/// `"subject_type" is not a permitted key` の 422 になる（2026-11-17 に確認）。
/// `subject_type` を付けなければ 201 で、応答の `subject_type` は API が `line` にする。
/// 複数行の [`create_multiline_review_comment`] も同じ規則。非推奨の `position` は送らない。
pub async fn create_review_comment(
    repo: &str,
    pr_number: u32,
    commit_id: &str,
    path: &str,
    line: SideLine,
    body: &str,
) -> Result<ReviewComment> {
    let fields = single_line_review_comment_fields(commit_id, path, body, line);
    post_pull_comment(
        repo,
        pr_number,
        &fields,
        "Failed to parse created comment response",
    )
    .await
}

/// 複数行レビューコメントを作成する。
///
/// GitHub API の `line`/`start_line`/`side`/`start_side` パラメータを使用。
/// `start` は範囲の先頭、`end` は末尾で、それぞれの `side` を `LEFT`/`RIGHT` に
/// 変換して送る。`start.line` < `end.line` であること。単一行の場合は
/// [`create_review_comment`] を使用。
///
/// NOTE: `subject_type` は送らない。`line`/`start_line`/`side`/`start_side` と
/// 一緒に送ると oneOf で `"subject_type" is not a permitted key` の 422 になる
/// （2026-11-17 に確認）。付けなければ 201 で、API は line-level コメントとして扱う
/// （応答の `subject_type` は `line`）。単一行の [`create_review_comment`] と同じ規則。
/// 非推奨の `position` は送らない。
pub async fn create_multiline_review_comment(
    repo: &str,
    pr_number: u32,
    commit_id: &str,
    path: &str,
    start: SideLine,
    end: SideLine,
    body: &str,
) -> Result<ReviewComment> {
    let fields = multiline_review_comment_fields(commit_id, path, body, start, end);
    post_pull_comment(
        repo,
        pr_number,
        &fields,
        "Failed to parse created multiline comment response",
    )
    .await
}

pub async fn create_reply_comment(
    repo: &str,
    pr_number: u32,
    comment_id: u64,
    body: &str,
) -> Result<ReviewComment> {
    let endpoint = format!(
        "repos/{}/pulls/{}/comments/{}/replies",
        repo, pr_number, comment_id
    );
    let json = gh_api_post(&endpoint, &[("body", FieldValue::String(body))]).await?;
    serde_json::from_value(json).context("Failed to parse reply comment response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    #[test]
    fn test_review_comment_deserialize() {
        let json = serde_json::json!({
            "id": 12345,
            "path": "src/main.rs",
            "line": 42,
            "body": "Consider using a match expression here.",
            "user": { "login": "reviewer1" },
            "created_at": "2025-01-15T10:30:00Z"
        });
        let comment: ReviewComment = serde_json::from_value(json).unwrap();
        assert_eq!(comment.id, 12345);
        assert_eq!(comment.path, "src/main.rs");
        assert_eq!(comment.line, Some(42));
        assert_eq!(comment.body, "Consider using a match expression here.");
        assert_eq!(comment.user.login, "reviewer1");
        assert_eq!(comment.created_at, "2025-01-15T10:30:00Z");
        assert_eq!(comment.location.original_line, None);
    }

    #[test]
    fn review_comment_deserializes_complete_rest_position_tolerantly() {
        let json = serde_json::json!({
            "id": 12346,
            "path": "src/old.rs",
            "line": null,
            "start_line": null,
            "original_line": 42,
            "original_start_line": 40,
            "side": "RIGHT",
            "start_side": "RIGHT",
            "position": 9,
            "original_position": 7,
            "commit_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "original_commit_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "subject_type": "line",
            "body": "Outdated range",
            "user": { "login": "reviewer1" },
            "created_at": "2025-01-15T10:30:00Z"
        });

        let comment: ReviewComment = serde_json::from_value(json).unwrap();
        assert_eq!(comment.location.original_line, Some(42));
        assert_eq!(comment.location.original_start_line, Some(40));
        assert_eq!(comment.location.side.as_deref(), Some("RIGHT"));
        assert_eq!(comment.location.start_side.as_deref(), Some("RIGHT"));
        assert_eq!(comment.location.position, Some(9));
        assert_eq!(comment.location.original_position, Some(7));
        assert_eq!(
            comment.location.original_commit_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            comment.location.commit_id.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(comment.location.subject_type.as_deref(), Some("line"));
    }

    #[test]
    fn test_review_comment_optional_line_null() {
        let json = serde_json::json!({
            "id": 99,
            "path": "README.md",
            "line": null,
            "body": "Top-level comment",
            "user": { "login": "user1" },
            "created_at": "2025-02-01T00:00:00Z"
        });
        let comment: ReviewComment = serde_json::from_value(json).unwrap();
        assert_eq!(comment.line, None);
        assert_eq!(comment.path, "README.md");
    }

    #[test]
    fn test_review_comment_roundtrip() {
        let original = ReviewComment {
            id: 1,
            path: "lib.rs".to_string(),
            line: Some(10),
            start_line: None,
            body: "LGTM".to_string(),
            user: User {
                login: "dev".to_string(),
            },
            created_at: "2025-03-01T12:00:00Z".to_string(),
            in_reply_to_id: None,
            location: ReviewCommentLocation::default(),
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: ReviewComment = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, original.id);
        assert_eq!(deserialized.path, original.path);
        assert_eq!(deserialized.line, original.line);
        assert_eq!(deserialized.body, original.body);
    }

    #[test]
    fn test_discussion_comment_deserialize() {
        let json = serde_json::json!({
            "id": 5678,
            "body": "Thanks for the fix!",
            "user": { "login": "commenter" },
            "created_at": "2025-01-20T08:00:00Z"
        });
        let comment: DiscussionComment = serde_json::from_value(json).unwrap();
        assert_eq!(comment.id, 5678);
        assert_eq!(comment.body, "Thanks for the fix!");
        assert_eq!(comment.user.login, "commenter");
    }

    #[test]
    fn test_discussion_comment_empty_body() {
        let json = serde_json::json!({
            "id": 100,
            "body": "",
            "user": { "login": "bot" },
            "created_at": "2025-01-01T00:00:00Z"
        });
        let comment: DiscussionComment = serde_json::from_value(json).unwrap();
        assert_eq!(comment.body, "");
    }

    #[test]
    fn test_discussion_comment_roundtrip() {
        let original = DiscussionComment {
            id: 42,
            body: "Multi\nline\nbody".to_string(),
            user: User {
                login: "author".to_string(),
            },
            created_at: "2025-06-01T00:00:00Z".to_string(),
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: DiscussionComment = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, original.id);
        assert_eq!(deserialized.body, original.body);
    }

    #[test]
    fn test_review_deserialize() {
        let json = serde_json::json!({
            "id": 9999,
            "body": "Approved with minor suggestions.",
            "state": "APPROVED",
            "user": { "login": "lead" },
            "submitted_at": "2025-03-10T14:00:00Z"
        });
        let review: Review = serde_json::from_value(json).unwrap();
        assert_eq!(review.id, 9999);
        assert_eq!(
            review.body,
            Some("Approved with minor suggestions.".to_string())
        );
        assert_eq!(review.state, "APPROVED");
        assert_eq!(review.user.login, "lead");
        assert_eq!(
            review.submitted_at,
            Some("2025-03-10T14:00:00Z".to_string())
        );
    }

    #[test]
    fn test_review_optional_fields_null() {
        let json = serde_json::json!({
            "id": 1,
            "body": null,
            "state": "COMMENTED",
            "user": { "login": "reviewer" },
            "submitted_at": null
        });
        let review: Review = serde_json::from_value(json).unwrap();
        assert_eq!(review.body, None);
        assert_eq!(review.submitted_at, None);
    }

    #[test]
    fn test_review_roundtrip() {
        let original = Review {
            id: 7,
            body: Some("Changes requested".to_string()),
            state: "CHANGES_REQUESTED".to_string(),
            user: User {
                login: "reviewer".to_string(),
            },
            submitted_at: Some("2025-04-01T09:00:00Z".to_string()),
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: Review = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, original.id);
        assert_eq!(deserialized.body, original.body);
        assert_eq!(deserialized.state, original.state);
        assert_eq!(deserialized.submitted_at, original.submitted_at);
    }

    #[test]
    fn test_review_comment_snapshot() {
        let json = serde_json::json!({
            "id": 555,
            "path": "src/app.rs",
            "line": 100,
            "body": "Snapshot test body",
            "user": { "login": "snapshot_user" },
            "created_at": "2025-01-01T00:00:00Z"
        });
        let comment: ReviewComment = serde_json::from_value(json).unwrap();
        assert_snapshot!(format!("{:?}", comment), @r#"ReviewComment { id: 555, path: "src/app.rs", line: Some(100), start_line: None, body: "Snapshot test body", user: User { login: "snapshot_user" }, created_at: "2025-01-01T00:00:00Z", in_reply_to_id: None, location: ReviewCommentLocation { original_line: None, original_start_line: None, side: None, start_side: None, position: None, original_position: None, commit_id: None, original_commit_id: None, subject_type: None, publication: Published } }"#);
    }

    #[test]
    fn test_review_comment_snapshot_with_start_line() {
        let comment = ReviewComment {
            id: 10,
            path: "src/app.rs".to_string(),
            line: Some(50),
            start_line: Some(45),
            body: "Multiline review".to_string(),
            user: User {
                login: "dacuna".to_string(),
            },
            created_at: "2026-03-25T00:00:00Z".to_string(),
            in_reply_to_id: None,
            location: ReviewCommentLocation::default(),
        };
        assert_snapshot!(format!("{:?}", comment), @r#"ReviewComment { id: 10, path: "src/app.rs", line: Some(50), start_line: Some(45), body: "Multiline review", user: User { login: "dacuna" }, created_at: "2026-03-25T00:00:00Z", in_reply_to_id: None, location: ReviewCommentLocation { original_line: None, original_start_line: None, side: None, start_side: None, position: None, original_position: None, commit_id: None, original_commit_id: None, subject_type: None, publication: Published } }"#);
    }

    fn request_object(fields: &[ApiField]) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        for field in fields {
            let value = if field.raw {
                serde_json::Value::Number(field.value.parse::<u64>().unwrap().into())
            } else {
                serde_json::Value::String(field.value.clone())
            };
            assert!(
                object.insert(field.key.to_string(), value).is_none(),
                "duplicate field {}",
                field.key
            );
        }
        serde_json::Value::Object(object)
    }

    fn assert_number_field(fields: &[ApiField], key: &str) {
        let field = fields.iter().find(|field| field.key == key).unwrap();
        assert!(field.raw, "{key} is a line number, sent with -F");
    }

    fn assert_string_field(fields: &[ApiField], key: &str) {
        let field = fields.iter().find(|field| field.key == key).unwrap();
        assert!(!field.raw, "{key} is a string, sent with -f");
    }

    #[test]
    fn single_line_right_request_is_the_new_file_line_and_side() {
        let fields =
            single_line_review_comment_fields("abc", "src/app.rs", "nit", SideLine::right(42));
        assert_eq!(
            fields.iter().map(|field| field.key).collect::<Vec<_>>(),
            ["body", "commit_id", "path", "line", "side"]
        );
        assert_number_field(&fields, "line");
        assert_string_field(&fields, "side");
        assert_eq!(
            request_object(&fields),
            serde_json::json!({
                "body": "nit",
                "commit_id": "abc",
                "path": "src/app.rs",
                "line": 42,
                "side": "RIGHT",
            })
        );
    }

    #[test]
    fn single_line_left_request_is_the_old_file_line() {
        let fields =
            single_line_review_comment_fields("abc", "src/app.rs", "gone", SideLine::left(11));
        assert_number_field(&fields, "line");
        assert_string_field(&fields, "side");
        assert_eq!(
            request_object(&fields),
            serde_json::json!({
                "body": "gone",
                "commit_id": "abc",
                "path": "src/app.rs",
                "line": 11,
                "side": "LEFT",
            })
        );
    }

    #[test]
    fn multiline_request_sends_the_range_on_one_side() {
        let fields = multiline_review_comment_fields(
            "abc",
            "src/app.rs",
            "span",
            SideLine::right(10),
            SideLine::right(12),
        );
        assert_eq!(
            fields.iter().map(|field| field.key).collect::<Vec<_>>(),
            [
                "body",
                "commit_id",
                "path",
                "start_line",
                "line",
                "start_side",
                "side"
            ]
        );
        assert_number_field(&fields, "start_line");
        assert_number_field(&fields, "line");
        assert_string_field(&fields, "start_side");
        assert_string_field(&fields, "side");
        assert_eq!(
            request_object(&fields),
            serde_json::json!({
                "body": "span",
                "commit_id": "abc",
                "path": "src/app.rs",
                "start_line": 10,
                "line": 12,
                "start_side": "RIGHT",
                "side": "RIGHT",
            })
        );
    }

    #[test]
    fn multiline_left_request_uses_the_old_file_for_both_ends() {
        let fields = multiline_review_comment_fields(
            "abc",
            "src/app.rs",
            "gone",
            SideLine::left(11),
            SideLine::left(12),
        );
        assert_eq!(
            request_object(&fields),
            serde_json::json!({
                "body": "gone",
                "commit_id": "abc",
                "path": "src/app.rs",
                "start_line": 11,
                "line": 12,
                "start_side": "LEFT",
                "side": "LEFT",
            })
        );
    }

    #[test]
    fn multiline_request_sends_each_end_side() {
        let fields = multiline_review_comment_fields(
            "abc",
            "src/app.rs",
            "span",
            SideLine::left(11),
            SideLine::right(12),
        );
        assert_string_field(&fields, "start_side");
        assert_string_field(&fields, "side");
        assert_eq!(
            request_object(&fields),
            serde_json::json!({
                "body": "span",
                "commit_id": "abc",
                "path": "src/app.rs",
                "start_line": 11,
                "line": 12,
                "start_side": "LEFT",
                "side": "RIGHT",
            })
        );
    }
}

/// The comment posts run against the `gh` test double, so the logged argv is the
/// request GitHub receives: `line` and `side`, and neither `subject_type` nor
/// the deprecated `position`.
#[cfg(all(test, unix))]
mod fake_gh_tests {
    use super::{create_multiline_review_comment, create_review_comment};
    use crate::diff::SideLine;
    use crate::github::fake_gh::FakeGh;
    use serial_test::serial;

    fn created(line: u32, side: &str) -> String {
        format!(
            r#"{{"id":1,"path":"src/app.rs","line":{line},"side":"{side}","body":"nit","user":{{"login":"me"}},"created_at":"2026-11-17T00:00:00Z"}}"#
        )
    }

    fn assert_posted_without_subject_type_or_position(calls: &str) {
        assert!(
            calls.contains("CALL api --method POST repos/o/r/pulls/7/comments "),
            "{calls}"
        );
        assert!(!calls.contains("subject_type"), "{calls}");
        assert!(!calls.contains("position"), "{calls}");
    }

    #[tokio::test]
    #[serial]
    async fn create_review_comment_posts_line_and_right_side() {
        let gh = FakeGh::install("", &created(42, "RIGHT"), "", "");

        let comment =
            create_review_comment("o/r", 7, "abc", "src/app.rs", SideLine::right(42), "nit")
                .await
                .unwrap();

        let calls = gh.calls();
        assert_posted_without_subject_type_or_position(&calls);
        assert!(calls.contains("-f body=nit"), "{calls}");
        assert!(calls.contains("-f commit_id=abc"), "{calls}");
        assert!(calls.contains("-f path=src/app.rs"), "{calls}");
        assert!(calls.contains("-F line=42"), "{calls}");
        assert!(calls.contains("-f side=RIGHT"), "{calls}");
        assert_eq!(comment.line, Some(42));
        assert_eq!(comment.location.side.as_deref(), Some("RIGHT"));
    }

    #[tokio::test]
    #[serial]
    async fn create_review_comment_posts_left_for_an_old_file_line() {
        let gh = FakeGh::install("", &created(11, "LEFT"), "", "");

        create_review_comment("o/r", 7, "abc", "src/app.rs", SideLine::left(11), "gone")
            .await
            .unwrap();

        let calls = gh.calls();
        assert_posted_without_subject_type_or_position(&calls);
        assert!(calls.contains("-F line=11"), "{calls}");
        assert!(calls.contains("-f side=LEFT"), "{calls}");
        assert!(!calls.contains("RIGHT"), "{calls}");
    }

    #[tokio::test]
    #[serial]
    async fn create_multiline_review_comment_posts_the_range_without_subject_type() {
        let gh = FakeGh::install("", &created(12, "RIGHT"), "", "");

        create_multiline_review_comment(
            "o/r",
            7,
            "abc",
            "src/app.rs",
            SideLine::right(10),
            SideLine::right(12),
            "span",
        )
        .await
        .unwrap();

        let calls = gh.calls();
        assert_posted_without_subject_type_or_position(&calls);
        assert!(calls.contains("-F start_line=10"), "{calls}");
        assert!(calls.contains("-F line=12"), "{calls}");
        assert!(calls.contains("-f start_side=RIGHT"), "{calls}");
        assert!(calls.contains("-f side=RIGHT"), "{calls}");
        assert!(calls.contains("-f body=span"), "{calls}");
    }

    #[tokio::test]
    #[serial]
    async fn create_multiline_review_comment_posts_left_for_both_ends() {
        let gh = FakeGh::install("", &created(12, "LEFT"), "", "");

        create_multiline_review_comment(
            "o/r",
            7,
            "abc",
            "src/app.rs",
            SideLine::left(11),
            SideLine::left(12),
            "gone",
        )
        .await
        .unwrap();

        let calls = gh.calls();
        assert_posted_without_subject_type_or_position(&calls);
        assert!(calls.contains("-F start_line=11"), "{calls}");
        assert!(calls.contains("-F line=12"), "{calls}");
        assert!(calls.contains("-f start_side=LEFT"), "{calls}");
        assert!(calls.contains("-f side=LEFT"), "{calls}");
        assert!(!calls.contains("RIGHT"), "{calls}");
    }

    #[tokio::test]
    #[serial]
    async fn create_multiline_review_comment_posts_each_end_side() {
        let gh = FakeGh::install("", &created(12, "RIGHT"), "", "");

        create_multiline_review_comment(
            "o/r",
            7,
            "abc",
            "src/app.rs",
            SideLine::left(11),
            SideLine::right(12),
            "span",
        )
        .await
        .unwrap();

        let calls = gh.calls();
        assert_posted_without_subject_type_or_position(&calls);
        assert!(calls.contains("-F start_line=11"), "{calls}");
        assert!(calls.contains("-F line=12"), "{calls}");
        assert!(calls.contains("-f start_side=LEFT"), "{calls}");
        assert!(calls.contains("-f side=RIGHT"), "{calls}");
        assert!(calls.contains("-f body=span"), "{calls}");
    }
}
