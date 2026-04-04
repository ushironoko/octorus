use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::client::{gh_api_paginate, gh_api_post, FieldValue};
use super::pr::User;

/// ジェネリックなfetch & parse関数（ページネーション対応）
async fn fetch_and_parse<T: DeserializeOwned>(
    endpoint: &str,
    error_context: &'static str,
) -> Result<T> {
    let json = gh_api_paginate(endpoint).await?;
    serde_json::from_value(json).context(error_context)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub path: String,
    pub line: Option<u32>,
    pub body: String,
    pub user: User,
    pub created_at: String,
    #[serde(default)]
    pub is_resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
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

pub async fn create_review_comment(
    repo: &str,
    pr_number: u32,
    commit_id: &str,
    path: &str,
    position: u32,
    body: &str,
) -> Result<ReviewComment> {
    let endpoint = format!("repos/{}/pulls/{}/comments", repo, pr_number);
    let position_str = position.to_string();
    // NOTE: line/side/subject_type は Pull Request Review の一部としてのみ有効。
    // 単体コメント API (POST /pulls/{n}/comments) では oneOf スキーマに合致せず 422 になる。
    // position パラメータ（patch 内オフセット）を使用する。
    let json = gh_api_post(
        &endpoint,
        &[
            ("body", FieldValue::String(body)),
            ("commit_id", FieldValue::String(commit_id)),
            ("path", FieldValue::String(path)),
            ("position", FieldValue::Raw(&position_str)),
        ],
    )
    .await?;
    serde_json::from_value(json).context("Failed to parse created comment response")
}

/// 複数行レビューコメントを作成する。
///
/// GitHub API の `line`/`start_line`/`side`/`start_side` パラメータを使用。
/// `start_line` < `line` であること。単一行の場合は `create_review_comment` を使用。
///
/// NOTE: `subject_type` は送信しない。GitHub API の oneOf スキーマで
/// positioning パラメータと競合し 422 を返すため。`line`/`side` が存在すれば
/// API は自動的に line-level コメントとして扱う。
#[allow(clippy::too_many_arguments)]
pub async fn create_multiline_review_comment(
    repo: &str,
    pr_number: u32,
    commit_id: &str,
    path: &str,
    start_line: u32,
    end_line: u32,
    side: &str,
    body: &str,
) -> Result<ReviewComment> {
    let endpoint = format!("repos/{}/pulls/{}/comments", repo, pr_number);
    let start_line_str = start_line.to_string();
    let end_line_str = end_line.to_string();
    let json = gh_api_post(
        &endpoint,
        &[
            ("body", FieldValue::String(body)),
            ("commit_id", FieldValue::String(commit_id)),
            ("path", FieldValue::String(path)),
            ("start_line", FieldValue::Raw(&start_line_str)),
            ("line", FieldValue::Raw(&end_line_str)),
            ("start_side", FieldValue::String(side)),
            ("side", FieldValue::String(side)),
        ],
    )
    .await?;
    serde_json::from_value(json).context("Failed to parse created multiline comment response")
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
            body: "LGTM".to_string(),
            user: User { login: "dev".to_string() },
            created_at: "2025-03-01T12:00:00Z".to_string(),
            is_resolved: false,
            resolved_at: None,
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
            user: User { login: "author".to_string() },
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
        assert_eq!(review.body, Some("Approved with minor suggestions.".to_string()));
        assert_eq!(review.state, "APPROVED");
        assert_eq!(review.user.login, "lead");
        assert_eq!(review.submitted_at, Some("2025-03-10T14:00:00Z".to_string()));
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
            user: User { login: "reviewer".to_string() },
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
        assert_snapshot!(format!("{:?}", comment), @r#"ReviewComment { id: 555, path: "src/app.rs", line: Some(100), body: "Snapshot test body", user: User { login: "snapshot_user" }, created_at: "2025-01-01T00:00:00Z", is_resolved: false, resolved_at: None }"#);
    }
}
