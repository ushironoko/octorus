use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::client::{gh_api_paginate, gh_command};
use super::pr::{Label, User};

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
    #[serde(default)]
    pub comments: u32,
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
    pub comments: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedPr {
    pub number: u32,
    pub title: String,
    pub state: String,
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

/// timeline APIレスポンスの中間型
#[derive(Debug, Deserialize)]
struct TimelineEvent {
    event: Option<String>,
    source: Option<TimelineSource>,
}

#[derive(Debug, Deserialize)]
struct TimelineSource {
    issue: Option<TimelineIssue>,
}

#[derive(Debug, Deserialize)]
struct TimelineIssue {
    number: u32,
    title: String,
    state: String,
    pull_request: Option<serde_json::Value>,
}

/// timeline APIレスポンスからlinked PRを抽出する純粋関数
fn parse_linked_prs_from_timeline(events: &[serde_json::Value]) -> Vec<LinkedPr> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for event_value in events {
        let Ok(event) = serde_json::from_value::<TimelineEvent>(event_value.clone()) else {
            continue;
        };

        if event.event.as_deref() != Some("cross-referenced") {
            continue;
        }

        let Some(source) = event.source else {
            continue;
        };
        let Some(issue) = source.issue else {
            continue;
        };

        // pull_request フィールドが存在する場合のみPRとして扱う
        if issue.pull_request.is_none() {
            continue;
        }

        if seen.insert(issue.number) {
            result.push(LinkedPr {
                number: issue.number,
                title: issue.title,
                state: issue.state,
            });
        }
    }

    result
}

/// Issue に紐づく PR を timeline API で取得
pub async fn fetch_linked_prs(repo: &str, issue_number: u32) -> Result<Vec<LinkedPr>> {
    let Some((owner, name)) = repo.split_once('/') else {
        anyhow::bail!("Invalid repo format: expected 'owner/repo', got '{}'", repo);
    };

    let endpoint = format!("repos/{}/{}/issues/{}/timeline", owner, name, issue_number);
    let json = gh_api_paginate(&endpoint).await?;

    let events: Vec<serde_json::Value> = match json {
        serde_json::Value::Array(arr) => arr,
        _ => Vec::new(),
    };

    Ok(parse_linked_prs_from_timeline(&events))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let json = r#"{"number":42,"title":"Bug report","state":"OPEN","author":{"login":"user1"},"labels":[],"updatedAt":"2026-01-01T00:00:00Z","comments":5}"#;
        let summary: IssueSummary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.number, 42);
        assert_eq!(summary.title, "Bug report");
        assert_eq!(summary.state, "OPEN");
        assert_eq!(summary.author.login, "user1");
        assert!(summary.labels.is_empty());
        assert_eq!(summary.comments, 5);
    }

    #[test]
    fn test_issue_detail_deserialize() {
        let json = r#"{"number":42,"title":"Bug","body":"description","state":"OPEN","author":{"login":"user1"},"labels":[{"name":"bug"}],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","comments":3}"#;
        let detail: IssueDetail = serde_json::from_str(json).unwrap();
        assert_eq!(detail.number, 42);
        assert_eq!(detail.body.as_deref(), Some("description"));
        assert_eq!(detail.created_at, "2026-01-01T00:00:00Z");
        assert_eq!(detail.updated_at, "2026-01-02T00:00:00Z");
        assert_eq!(detail.labels.len(), 1);
        assert_eq!(detail.labels[0].name, "bug");
    }

    #[test]
    fn test_issue_detail_deserialize_null_body() {
        let json = r#"{"number":1,"title":"T","body":null,"state":"OPEN","author":{"login":"u"},"labels":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","comments":0}"#;
        let detail: IssueDetail = serde_json::from_str(json).unwrap();
        assert!(detail.body.is_none());
    }

    #[test]
    fn test_linked_pr_deserialize() {
        let json = r#"{"number":45,"title":"Fix bug","state":"open"}"#;
        let pr: LinkedPr = serde_json::from_str(json).unwrap();
        assert_eq!(pr.number, 45);
        assert_eq!(pr.title, "Fix bug");
        assert_eq!(pr.state, "open");
    }

    #[test]
    fn test_empty_timeline_returns_empty_vec() {
        let events: Vec<serde_json::Value> = vec![];
        let result = parse_linked_prs_from_timeline(&events);
        assert!(result.is_empty());
    }

    #[test]
    fn test_cross_referenced_with_pr_is_extracted() {
        let event = serde_json::json!({
            "event": "cross-referenced",
            "source": {
                "issue": {
                    "number": 45,
                    "title": "Fix bug",
                    "state": "open",
                    "pull_request": {"url": "https://api.github.com/repos/owner/repo/pulls/45"}
                }
            }
        });
        let result = parse_linked_prs_from_timeline(&[event]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].number, 45);
        assert_eq!(result[0].title, "Fix bug");
        assert_eq!(result[0].state, "open");
    }

    #[test]
    fn test_cross_referenced_without_pr_is_ignored() {
        let event = serde_json::json!({
            "event": "cross-referenced",
            "source": {
                "issue": {
                    "number": 99,
                    "title": "Related issue",
                    "state": "open"
                }
            }
        });
        let result = parse_linked_prs_from_timeline(&[event]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_non_cross_referenced_events_are_ignored() {
        let event = serde_json::json!({
            "event": "labeled",
            "label": {"name": "bug"}
        });
        let result = parse_linked_prs_from_timeline(&[event]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_duplicate_prs_are_deduplicated() {
        let event = serde_json::json!({
            "event": "cross-referenced",
            "source": {
                "issue": {
                    "number": 45,
                    "title": "Fix",
                    "state": "open",
                    "pull_request": {"url": "..."}
                }
            }
        });
        let result = parse_linked_prs_from_timeline(&[event.clone(), event]);
        assert_eq!(result.len(), 1);
    }
}
