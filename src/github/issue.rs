use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::client::{gh_api_graphql, gh_api_post, gh_command, FieldValue};
use super::pr::{Label, User};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueComment {
    pub id: String,
    pub body: String,
    pub author: User,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "authorAssociation", default)]
    pub author_association: String,
    #[serde(default)]
    pub url: String,
}

pub fn parse_issue_comments(raw: &[serde_json::Value]) -> Vec<IssueComment> {
    raw.iter()
        .filter_map(
            |v| match serde_json::from_value::<IssueComment>(v.clone()) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("Failed to parse issue comment: {}", e);
                    None
                }
            },
        )
        .collect()
}

/// Issue にコメントを投稿する（REST API）
pub async fn create_issue_comment(
    repo: &str,
    issue_number: u32,
    body: &str,
) -> Result<IssueComment> {
    let endpoint = format!("repos/{}/issues/{}/comments", repo, issue_number);
    let json = gh_api_post(&endpoint, &[("body", FieldValue::String(body))]).await?;
    parse_rest_issue_comment(&json)
}

/// REST API レスポンスから IssueComment を構築する。
///
/// REST API は snake_case + `id: u64` + `user` キーを返すが、
/// 既存の `IssueComment` は GraphQL 由来の camelCase + `id: String` + `author` キー。
/// 両形式に対応するためフォールバック付きで手動変換する。
fn parse_rest_issue_comment(json: &serde_json::Value) -> Result<IssueComment> {
    let id = json
        .get("id")
        .and_then(|v| {
            v.as_u64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(String::from))
        })
        .context("missing id")?;
    let body = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let login = json
        .pointer("/user/login")
        .or_else(|| json.pointer("/author/login"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let created_at = json
        .get("created_at")
        .or_else(|| json.get("createdAt"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let author_association = json
        .get("author_association")
        .or_else(|| json.get("authorAssociation"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let url = json
        .get("html_url")
        .or_else(|| json.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(IssueComment {
        id,
        body,
        author: User { login },
        created_at,
        author_association,
        url,
    })
}

/// Issue状態フィルタ（型安全）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IssueStateFilter {
    #[default]
    Open,
    Closed,
    All,
}

impl IssueStateFilter {
    pub fn as_gh_arg(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::All,
            Self::All => Self::Open,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueSummary {
    pub number: u32,
    pub title: String,
    pub state: String,
    pub author: User,
    pub labels: Vec<Label>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    /// gh CLI は comments をオブジェクト配列で返すため Vec で受ける
    #[serde(default)]
    pub comments: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueDetail {
    pub number: u32,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub author: User,
    pub labels: Vec<Label>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default)]
    pub comments: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedPr {
    pub number: u32,
    pub title: String,
    pub state: String,
    /// リポジトリ (owner/repo)。None の場合は現在のリポと同じ
    pub repo: Option<String>,
}

pub struct IssueListPage {
    pub items: Vec<IssueSummary>,
    pub has_more: bool,
}

/// Issue一覧取得（limit+1件取得してhas_moreを判定）
pub async fn fetch_issue_list(
    repo: &str,
    state: IssueStateFilter,
    limit: u32,
) -> Result<IssueListPage> {
    let output = gh_command(&[
        "issue",
        "list",
        "-R",
        repo,
        "-s",
        state.as_gh_arg(),
        "--json",
        "number,title,state,author,labels,updatedAt,comments",
        "--limit",
        &(limit + 1).to_string(),
    ])
    .await?;

    let mut items: Vec<IssueSummary> =
        serde_json::from_str(&output).context("Failed to parse issue list response")?;
    let has_more = items.len() > limit as usize;
    items.truncate(limit as usize);

    Ok(IssueListPage { items, has_more })
}

/// Issue一覧取得（オフセット付き、追加ロード用）
pub async fn fetch_issue_list_with_offset(
    repo: &str,
    state: IssueStateFilter,
    offset: u32,
    limit: u32,
) -> Result<IssueListPage> {
    let fetch_count = offset + limit + 1;
    let output = gh_command(&[
        "issue",
        "list",
        "-R",
        repo,
        "-s",
        state.as_gh_arg(),
        "--json",
        "number,title,state,author,labels,updatedAt,comments",
        "--limit",
        &fetch_count.to_string(),
    ])
    .await?;

    let all_items: Vec<IssueSummary> =
        serde_json::from_str(&output).context("Failed to parse issue list response")?;

    let has_more = all_items.len() > (offset + limit) as usize;
    let items: Vec<IssueSummary> = all_items
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();

    Ok(IssueListPage { items, has_more })
}

/// Issue詳細取得
pub async fn fetch_issue_detail(repo: &str, issue_number: u32) -> Result<IssueDetail> {
    let output = gh_command(&[
        "issue",
        "view",
        &issue_number.to_string(),
        "-R",
        repo,
        "--json",
        "number,title,body,state,author,labels,createdAt,updatedAt,comments",
    ])
    .await?;

    serde_json::from_str(&output).context("Failed to parse issue detail response")
}

/// GraphQL レスポンスからlinked PRを抽出する純粋関数
fn parse_linked_prs_from_graphql(data: &serde_json::Value, current_repo: &str) -> Vec<LinkedPr> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    let Some(nodes) = data
        .pointer("/data/repository/issue/timelineItems/nodes")
        .and_then(|v| v.as_array())
    else {
        return result;
    };

    for node in nodes {
        let typename = node.get("__typename").and_then(|v| v.as_str());

        let pr_data = match typename {
            Some("CrossReferencedEvent") => node.pointer("/source"),
            Some("ConnectedEvent") => node.pointer("/subject"),
            _ => continue,
        };

        let Some(pr) = pr_data else { continue };

        // PullRequest のみ扱う（Issue の cross-reference は除外）
        if pr.get("__typename").and_then(|v| v.as_str()) != Some("PullRequest") {
            continue;
        }

        let Some(number) = pr.get("number").and_then(|v| v.as_u64()) else {
            continue;
        };
        let number = number as u32;

        let title = pr
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let state = pr
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("OPEN")
            .to_string();

        let repo = pr
            .pointer("/repository/nameWithOwner")
            .and_then(|v| v.as_str())
            .and_then(|name| {
                if name == current_repo {
                    None
                } else {
                    Some(name.to_string())
                }
            });

        let key = (repo.clone(), number);
        if seen.insert(key) {
            result.push(LinkedPr {
                number,
                title,
                state,
                repo,
            });
        }
    }

    result
}

const LINKED_PRS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      timelineItems(first: 100, itemTypes: [CONNECTED_EVENT, CROSS_REFERENCED_EVENT]) {
        nodes {
          __typename
          ... on ConnectedEvent {
            subject {
              __typename
              ... on PullRequest {
                number
                title
                state
                repository { nameWithOwner }
              }
            }
          }
          ... on CrossReferencedEvent {
            source {
              __typename
              ... on PullRequest {
                number
                title
                state
                repository { nameWithOwner }
              }
            }
          }
        }
      }
    }
  }
}
"#;

/// Issue に紐づく PR を GraphQL timeline API で取得
///
/// `ConnectedEvent`（Development sidebar リンク）と `CrossReferencedEvent`
/// （PR body/コミットメッセージからの参照）の両方を取得する。
pub async fn fetch_linked_prs(repo: &str, issue_number: u32) -> Result<Vec<LinkedPr>> {
    let Some((owner, name)) = repo.split_once('/') else {
        anyhow::bail!("Invalid repo format: expected 'owner/repo', got '{}'", repo);
    };

    let data = gh_api_graphql(
        LINKED_PRS_QUERY,
        &[
            ("owner", FieldValue::String(owner)),
            ("name", FieldValue::String(name)),
            ("number", FieldValue::Raw(&issue_number.to_string())),
        ],
    )
    .await?;

    Ok(parse_linked_prs_from_graphql(&data, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_issue_comment_deserialize() {
        let json = r#"{"id":"IC_kwDOTest","body":"Hello world","author":{"login":"user1"},"createdAt":"2026-01-01T00:00:00Z","authorAssociation":"OWNER","url":"https://github.com/test/repo/issues/1#issuecomment-1"}"#;
        let comment: IssueComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.id, "IC_kwDOTest");
        assert_eq!(comment.body, "Hello world");
        assert_eq!(comment.author.login, "user1");
        assert_eq!(comment.created_at, "2026-01-01T00:00:00Z");
        assert_eq!(comment.author_association, "OWNER");
        assert_eq!(
            comment.url,
            "https://github.com/test/repo/issues/1#issuecomment-1"
        );
    }

    #[test]
    fn test_parse_issue_comments_from_gh_cli_output() {
        let raw = vec![
            serde_json::json!({
                "id": "IC_1",
                "body": "First comment",
                "author": {"login": "user1"},
                "createdAt": "2026-01-01T00:00:00Z",
                "authorAssociation": "OWNER",
                "url": "https://example.com/1"
            }),
            serde_json::json!({
                "id": "IC_2",
                "body": "Second comment",
                "author": {"login": "user2"},
                "createdAt": "2026-01-02T00:00:00Z",
                "authorAssociation": "CONTRIBUTOR",
                "url": "https://example.com/2"
            }),
        ];
        let comments = parse_issue_comments(&raw);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, "IC_1");
        assert_eq!(comments[1].author.login, "user2");
    }

    #[test]
    fn test_parse_issue_comments_empty() {
        let raw: Vec<serde_json::Value> = vec![];
        let comments = parse_issue_comments(&raw);
        assert!(comments.is_empty());
    }

    #[test]
    fn test_parse_issue_comments_malformed_entry_skipped() {
        let raw = vec![
            serde_json::json!({
                "id": "IC_1",
                "body": "Valid",
                "author": {"login": "user1"},
                "createdAt": "2026-01-01T00:00:00Z"
            }),
            // Missing required fields (id, body, author, createdAt)
            serde_json::json!({"bad": "data"}),
            serde_json::json!({
                "id": "IC_3",
                "body": "Also valid",
                "author": {"login": "user3"},
                "createdAt": "2026-01-03T00:00:00Z"
            }),
        ];
        let comments = parse_issue_comments(&raw);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, "IC_1");
        assert_eq!(comments[1].id, "IC_3");
    }

    #[test]
    fn test_parse_rest_issue_comment() {
        let json = serde_json::json!({
            "id": 123456789,
            "body": "Hello from REST",
            "user": {"login": "testuser"},
            "created_at": "2026-01-01T00:00:00Z",
            "author_association": "OWNER",
            "html_url": "https://github.com/test/repo/issues/1#issuecomment-123456789"
        });
        let comment = parse_rest_issue_comment(&json).unwrap();
        assert_eq!(comment.id, "123456789");
        assert_eq!(comment.body, "Hello from REST");
        assert_eq!(comment.author.login, "testuser");
        assert_eq!(comment.created_at, "2026-01-01T00:00:00Z");
        assert_eq!(comment.author_association, "OWNER");
        assert_eq!(
            comment.url,
            "https://github.com/test/repo/issues/1#issuecomment-123456789"
        );
    }

    #[test]
    fn test_parse_rest_issue_comment_graphql_fallback() {
        // GraphQL 形式のフィールド名でもフォールバック動作する
        let json = serde_json::json!({
            "id": "IC_kwDOTest",
            "body": "Hello from GraphQL",
            "author": {"login": "gqluser"},
            "createdAt": "2026-02-01T00:00:00Z",
            "authorAssociation": "CONTRIBUTOR",
            "url": "https://github.com/test/repo/issues/2#issuecomment-2"
        });
        let comment = parse_rest_issue_comment(&json).unwrap();
        assert_eq!(comment.id, "IC_kwDOTest");
        assert_eq!(comment.author.login, "gqluser");
        assert_eq!(comment.created_at, "2026-02-01T00:00:00Z");
        assert_eq!(comment.author_association, "CONTRIBUTOR");
    }

    #[test]
    fn test_issue_state_filter_as_gh_arg() {
        assert_eq!(IssueStateFilter::Open.as_gh_arg(), "open");
        assert_eq!(IssueStateFilter::Closed.as_gh_arg(), "closed");
        assert_eq!(IssueStateFilter::All.as_gh_arg(), "all");
    }

    #[test]
    fn test_issue_state_filter_display_name() {
        assert_eq!(IssueStateFilter::Open.display_name(), "open");
        assert_eq!(IssueStateFilter::Closed.display_name(), "closed");
        assert_eq!(IssueStateFilter::All.display_name(), "all");
    }

    #[test]
    fn test_issue_state_filter_next_cycles() {
        assert_eq!(IssueStateFilter::Open.next(), IssueStateFilter::Closed);
        assert_eq!(IssueStateFilter::Closed.next(), IssueStateFilter::All);
        assert_eq!(IssueStateFilter::All.next(), IssueStateFilter::Open);
    }

    #[test]
    fn test_issue_summary_deserialize() {
        let json = r#"{"number":42,"title":"Bug report","state":"OPEN","author":{"login":"user1"},"labels":[],"updatedAt":"2026-01-01T00:00:00Z","comments":[{"body":"hello"},{"body":"world"}]}"#;
        let summary: IssueSummary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.number, 42);
        assert_eq!(summary.title, "Bug report");
        assert_eq!(summary.state, "OPEN");
        assert_eq!(summary.author.login, "user1");
        assert!(summary.labels.is_empty());
        assert_eq!(summary.comments.len(), 2);
    }

    #[test]
    fn test_issue_summary_deserialize_no_comments() {
        let json = r#"{"number":1,"title":"T","state":"OPEN","author":{"login":"u"},"labels":[],"updatedAt":"2026-01-01T00:00:00Z"}"#;
        let summary: IssueSummary = serde_json::from_str(json).unwrap();
        assert!(summary.comments.is_empty());
    }

    #[test]
    fn test_issue_detail_deserialize() {
        let json = r#"{"number":42,"title":"Bug","body":"description","state":"OPEN","author":{"login":"user1"},"labels":[{"name":"bug"}],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","comments":[{"body":"c1"},{"body":"c2"},{"body":"c3"}]}"#;
        let detail: IssueDetail = serde_json::from_str(json).unwrap();
        assert_eq!(detail.number, 42);
        assert_eq!(detail.body.as_deref(), Some("description"));
        assert_eq!(detail.created_at, "2026-01-01T00:00:00Z");
        assert_eq!(detail.updated_at, "2026-01-02T00:00:00Z");
        assert_eq!(detail.labels.len(), 1);
        assert_eq!(detail.labels[0].name, "bug");
        assert_eq!(detail.comments.len(), 3);
    }

    #[test]
    fn test_issue_detail_deserialize_null_body() {
        let json = r#"{"number":1,"title":"T","body":null,"state":"OPEN","author":{"login":"u"},"labels":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","comments":[]}"#;
        let detail: IssueDetail = serde_json::from_str(json).unwrap();
        assert!(detail.body.is_none());
    }

    #[test]
    fn test_linked_pr_deserialize() {
        let json = r#"{"number":45,"title":"Fix bug","state":"open","repo":null}"#;
        let pr: LinkedPr = serde_json::from_str(json).unwrap();
        assert_eq!(pr.number, 45);
        assert_eq!(pr.title, "Fix bug");
        assert_eq!(pr.state, "open");
        assert!(pr.repo.is_none());
    }

    #[test]
    fn test_linked_pr_deserialize_with_repo() {
        let json = r#"{"number":45,"title":"Fix bug","state":"open","repo":"other/repo"}"#;
        let pr: LinkedPr = serde_json::from_str(json).unwrap();
        assert_eq!(pr.repo.as_deref(), Some("other/repo"));
    }

    /// GraphQL レスポンスのヘルパー
    fn graphql_response(nodes: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "data": {
                "repository": {
                    "issue": {
                        "timelineItems": {
                            "nodes": nodes
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn test_empty_graphql_response_returns_empty_vec() {
        let data = graphql_response(vec![]);
        let result = parse_linked_prs_from_graphql(&data, "owner/repo");
        assert!(result.is_empty());
    }

    #[test]
    fn test_cross_referenced_pr_is_extracted() {
        let node = serde_json::json!({
            "__typename": "CrossReferencedEvent",
            "source": {
                "__typename": "PullRequest",
                "number": 45,
                "title": "Fix bug",
                "state": "OPEN",
                "repository": { "nameWithOwner": "owner/repo" }
            }
        });
        let data = graphql_response(vec![node]);
        let result = parse_linked_prs_from_graphql(&data, "owner/repo");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].number, 45);
        assert_eq!(result[0].title, "Fix bug");
        assert_eq!(result[0].state, "OPEN");
        assert!(result[0].repo.is_none()); // 同一リポ
    }

    #[test]
    fn test_connected_pr_is_extracted() {
        let node = serde_json::json!({
            "__typename": "ConnectedEvent",
            "subject": {
                "__typename": "PullRequest",
                "number": 50,
                "title": "Linked via sidebar",
                "state": "MERGED",
                "repository": { "nameWithOwner": "owner/repo" }
            }
        });
        let data = graphql_response(vec![node]);
        let result = parse_linked_prs_from_graphql(&data, "owner/repo");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].number, 50);
        assert_eq!(result[0].title, "Linked via sidebar");
        assert_eq!(result[0].state, "MERGED");
    }

    #[test]
    fn test_cross_referenced_issue_is_ignored() {
        let node = serde_json::json!({
            "__typename": "CrossReferencedEvent",
            "source": {
                "__typename": "Issue",
                "number": 99,
                "title": "Related issue",
                "state": "OPEN"
            }
        });
        let data = graphql_response(vec![node]);
        let result = parse_linked_prs_from_graphql(&data, "owner/repo");
        assert!(result.is_empty());
    }

    #[test]
    fn test_unknown_event_type_is_ignored() {
        let node = serde_json::json!({
            "__typename": "LabeledEvent",
            "label": {"name": "bug"}
        });
        let data = graphql_response(vec![node]);
        let result = parse_linked_prs_from_graphql(&data, "owner/repo");
        assert!(result.is_empty());
    }

    #[test]
    fn test_duplicate_prs_are_deduplicated() {
        let node = serde_json::json!({
            "__typename": "CrossReferencedEvent",
            "source": {
                "__typename": "PullRequest",
                "number": 45,
                "title": "Fix",
                "state": "OPEN",
                "repository": { "nameWithOwner": "owner/repo" }
            }
        });
        let data = graphql_response(vec![node.clone(), node]);
        let result = parse_linked_prs_from_graphql(&data, "owner/repo");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_cross_repo_pr_has_repo_field() {
        let node = serde_json::json!({
            "__typename": "CrossReferencedEvent",
            "source": {
                "__typename": "PullRequest",
                "number": 10,
                "title": "Cross-repo fix",
                "state": "OPEN",
                "repository": { "nameWithOwner": "other/repo" }
            }
        });
        let data = graphql_response(vec![node]);
        let result = parse_linked_prs_from_graphql(&data, "owner/repo");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].repo.as_deref(), Some("other/repo"));
    }

    #[test]
    fn test_mixed_connected_and_cross_referenced() {
        let nodes = vec![
            serde_json::json!({
                "__typename": "CrossReferencedEvent",
                "source": {
                    "__typename": "PullRequest",
                    "number": 10,
                    "title": "PR via reference",
                    "state": "OPEN",
                    "repository": { "nameWithOwner": "owner/repo" }
                }
            }),
            serde_json::json!({
                "__typename": "ConnectedEvent",
                "subject": {
                    "__typename": "PullRequest",
                    "number": 20,
                    "title": "PR via sidebar",
                    "state": "MERGED",
                    "repository": { "nameWithOwner": "owner/repo" }
                }
            }),
        ];
        let data = graphql_response(nodes);
        let result = parse_linked_prs_from_graphql(&data, "owner/repo");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].number, 10);
        assert_eq!(result[1].number, 20);
    }
}
