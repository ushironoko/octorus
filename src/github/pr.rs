use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::client::{
    gh_api, gh_api_graphql, gh_api_paginate, gh_command, gh_command_allow_exit_codes, FieldValue,
};
use crate::app::ReviewAction;

/// PR状態フィルタ（型安全）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PrStateFilter {
    #[default]
    Open,
    Closed,
    All,
}

impl PrStateFilter {
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
pub struct StatusCheckRollupItem {
    #[serde(rename = "__typename")]
    pub type_name: String,
    pub name: Option<String>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub context: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    Success,
    Failure,
    Pending,
    None,
}

impl CiStatus {
    pub fn from_rollup(items: &[StatusCheckRollupItem]) -> Self {
        if items.is_empty() {
            return Self::None;
        }
        let mut has_pending = false;
        for item in items {
            match item.type_name.as_str() {
                "CheckRun" => match item.conclusion.as_deref() {
                    Some("SUCCESS") | Some("NEUTRAL") | Some("SKIPPED") => {}
                    Some(_) => return Self::Failure,
                    None => {
                        has_pending = true;
                    }
                },
                "StatusContext" => match item.state.as_deref() {
                    Some("SUCCESS") => {}
                    Some("PENDING") | Some("EXPECTED") => has_pending = true,
                    Some(_) => return Self::Failure,
                    None => {}
                },
                _ => {}
            }
        }
        if has_pending {
            Self::Pending
        } else {
            Self::Success
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckItem {
    pub name: String,
    pub state: String,
    pub bucket: Option<String>,
    pub link: Option<String>,
    #[serde(default)]
    pub workflow: String,
    pub description: Option<String>,
    #[serde(rename = "startedAt")]
    pub started_at: Option<String>,
    #[serde(rename = "completedAt")]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestSummary {
    pub number: u32,
    pub title: String,
    pub state: String,
    pub author: User,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    pub labels: Vec<Label>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default, rename = "statusCheckRollup")]
    pub status_check_rollup: Vec<StatusCheckRollupItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u32,
    #[serde(default, rename = "node_id")]
    pub node_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub head: Branch,
    pub base: Branch,
    pub user: User,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub patch: Option<String>,
    #[serde(default)]
    pub viewed: bool,
}

pub async fn fetch_pr(repo: &str, pr_number: u32) -> Result<PullRequest> {
    let endpoint = format!("repos/{}/pulls/{}", repo, pr_number);
    let json = gh_api(&endpoint).await?;
    serde_json::from_value(json).context("Failed to parse PR response")
}

pub async fn fetch_changed_files(repo: &str, pr_number: u32) -> Result<Vec<ChangedFile>> {
    let endpoint = format!("repos/{}/pulls/{}/files?per_page=100", repo, pr_number);
    let json = gh_api_paginate(&endpoint).await?;
    serde_json::from_value(json).context("Failed to parse changed files response")
}

pub async fn submit_review(
    repo: &str,
    pr_number: u32,
    action: ReviewAction,
    body: &str,
) -> Result<()> {
    let action_flag = match action {
        ReviewAction::Approve => "--approve",
        ReviewAction::RequestChanges => "--request-changes",
        ReviewAction::Comment => "--comment",
    };

    gh_command(&[
        "pr",
        "review",
        &pr_number.to_string(),
        action_flag,
        "-b",
        body,
        "-R",
        repo,
    ])
    .await?;

    Ok(())
}

/// Fetch the raw diff for a PR using `gh pr diff`
pub async fn fetch_pr_diff(repo: &str, pr_number: u32) -> Result<String> {
    gh_command(&["pr", "diff", &pr_number.to_string(), "-R", repo]).await
}

#[derive(Debug, Deserialize)]
struct GraphqlPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrFileNode {
    path: String,
    #[serde(rename = "viewerViewedState")]
    viewer_viewed_state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrFilesConnection {
    nodes: Vec<GraphqlPrFileNode>,
    #[serde(rename = "pageInfo")]
    page_info: GraphqlPageInfo,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrNode {
    files: GraphqlPrFilesConnection,
}

#[derive(Debug, Deserialize)]
struct GraphqlFilesViewedStateData {
    node: Option<GraphqlPrNode>,
}

#[derive(Debug, Deserialize)]
struct GraphqlFilesViewedStateResponse {
    data: Option<GraphqlFilesViewedStateData>,
}

pub async fn fetch_files_viewed_state(
    _repo: &str,
    pr_node_id: &str,
) -> Result<HashMap<String, bool>> {
    let query = r#"
query($pullRequestId: ID!, $after: String) {
  node(id: $pullRequestId) {
    ... on PullRequest {
      files(first: 100, after: $after) {
        nodes {
          path
          viewerViewedState
        }
        pageInfo {
          hasNextPage
          endCursor
        }
      }
    }
  }
}
"#;

    let mut viewed_state = HashMap::new();
    let mut after: Option<String> = None;

    loop {
        let mut fields = vec![("pullRequestId", FieldValue::String(pr_node_id))];
        if let Some(cursor) = after.as_deref() {
            fields.push(("after", FieldValue::String(cursor)));
        }

        let response = gh_api_graphql(query, &fields).await?;

        if let Some(errors) = response.get("errors") {
            anyhow::bail!("GitHub GraphQL returned errors: {}", errors);
        }

        let parsed: GraphqlFilesViewedStateResponse = serde_json::from_value(response)
            .context("Failed to parse files viewed-state GraphQL response")?;
        let Some(data) = parsed.data else {
            anyhow::bail!("GitHub GraphQL response missing data");
        };
        let Some(node) = data.node else {
            anyhow::bail!("Pull request node not found for viewed-state query");
        };

        for file in node.files.nodes {
            viewed_state.insert(
                file.path,
                matches!(file.viewer_viewed_state.as_deref(), Some("VIEWED")),
            );
        }

        if node.files.page_info.has_next_page {
            let Some(next_cursor) = node.files.page_info.end_cursor else {
                anyhow::bail!("GitHub GraphQL pageInfo missing endCursor");
            };
            after = Some(next_cursor);
        } else {
            break;
        }
    }

    Ok(viewed_state)
}

pub async fn mark_file_as_viewed(_repo: &str, pr_node_id: &str, path: &str) -> Result<()> {
    let query = r#"
mutation($pullRequestId: ID!, $path: String!) {
  markFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path }) {
    clientMutationId
  }
}
"#;

    let response = gh_api_graphql(
        query,
        &[
            ("pullRequestId", FieldValue::String(pr_node_id)),
            ("path", FieldValue::String(path)),
        ],
    )
    .await?;

    if let Some(errors) = response.get("errors") {
        anyhow::bail!("GitHub GraphQL returned errors: {}", errors);
    }

    Ok(())
}

pub async fn unmark_file_as_viewed(_repo: &str, pr_node_id: &str, path: &str) -> Result<()> {
    let query = r#"
mutation($pullRequestId: ID!, $path: String!) {
  unmarkFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path }) {
    clientMutationId
  }
}
"#;

    let response = gh_api_graphql(
        query,
        &[
            ("pullRequestId", FieldValue::String(pr_node_id)),
            ("path", FieldValue::String(path)),
        ],
    )
    .await?;

    if let Some(errors) = response.get("errors") {
        anyhow::bail!("GitHub GraphQL returned errors: {}", errors);
    }

    Ok(())
}

/// ページネーション結果
pub struct PrListPage {
    pub items: Vec<PullRequestSummary>,
    pub has_more: bool,
}

/// PR一覧取得（limit+1件取得してhas_moreを判定）
pub async fn fetch_pr_list(repo: &str, state: PrStateFilter, limit: u32) -> Result<PrListPage> {
    let output = gh_command(&[
        "pr",
        "list",
        "-R",
        repo,
        "-s",
        state.as_gh_arg(),
        "--json",
        "number,title,state,author,isDraft,labels,updatedAt,statusCheckRollup",
        "--limit",
        &(limit + 1).to_string(),
    ])
    .await?;

    let mut items: Vec<PullRequestSummary> =
        serde_json::from_str(&output).context("Failed to parse PR list response")?;
    let has_more = items.len() > limit as usize;
    items.truncate(limit as usize);

    Ok(PrListPage { items, has_more })
}

/// PR一覧取得（オフセット付き、追加ロード用）
pub async fn fetch_pr_list_with_offset(
    repo: &str,
    state: PrStateFilter,
    offset: u32,
    limit: u32,
) -> Result<PrListPage> {
    // gh pr list doesn't support offset directly, so we fetch offset+limit+1 and skip
    let fetch_count = offset + limit + 1;
    let output = gh_command(&[
        "pr",
        "list",
        "-R",
        repo,
        "-s",
        state.as_gh_arg(),
        "--json",
        "number,title,state,author,isDraft,labels,updatedAt,statusCheckRollup",
        "--limit",
        &fetch_count.to_string(),
    ])
    .await?;

    let all_items: Vec<PullRequestSummary> =
        serde_json::from_str(&output).context("Failed to parse PR list response")?;

    // Check if there are more items beyond what we're returning
    let has_more = all_items.len() > (offset + limit) as usize;

    // Skip the offset items and take limit items
    let items: Vec<PullRequestSummary> = all_items
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();

    Ok(PrListPage { items, has_more })
}

pub async fn fetch_pr_checks(repo: &str, pr_number: u32) -> Result<Vec<CheckItem>> {
    let output = gh_command_allow_exit_codes(
        &[
            "pr",
            "checks",
            &pr_number.to_string(),
            "-R",
            repo,
            "--json",
            "name,state,bucket,link,workflow,description,startedAt,completedAt",
        ],
        &[8], // exit code 8 = checks pending
    )
    .await?;
    serde_json::from_str(&output).context("Failed to parse PR checks response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pr_state_filter_as_gh_arg() {
        assert_eq!(PrStateFilter::Open.as_gh_arg(), "open");
        assert_eq!(PrStateFilter::Closed.as_gh_arg(), "closed");
        assert_eq!(PrStateFilter::All.as_gh_arg(), "all");
    }

    #[test]
    fn test_pr_state_filter_display_name() {
        assert_eq!(PrStateFilter::Open.display_name(), "open");
        assert_eq!(PrStateFilter::Closed.display_name(), "closed");
        assert_eq!(PrStateFilter::All.display_name(), "all");
    }

    #[test]
    fn test_pr_state_filter_next_cycle() {
        assert_eq!(PrStateFilter::Open.next(), PrStateFilter::Closed);
        assert_eq!(PrStateFilter::Closed.next(), PrStateFilter::All);
        assert_eq!(PrStateFilter::All.next(), PrStateFilter::Open);
    }

    #[test]
    fn test_ci_status_from_rollup_empty() {
        assert_eq!(CiStatus::from_rollup(&[]), CiStatus::None);
    }

    #[test]
    fn test_ci_status_from_rollup_all_success() {
        let items = vec![
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("build".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("SUCCESS".to_string()),
                context: None,
                state: None,
            },
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("lint".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("NEUTRAL".to_string()),
                context: None,
                state: None,
            },
        ];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Success);
    }

    #[test]
    fn test_ci_status_from_rollup_failure() {
        let items = vec![
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("build".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("SUCCESS".to_string()),
                context: None,
                state: None,
            },
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("test".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("FAILURE".to_string()),
                context: None,
                state: None,
            },
        ];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Failure);
    }

    #[test]
    fn test_ci_status_from_rollup_pending() {
        let items = vec![
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("build".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("SUCCESS".to_string()),
                context: None,
                state: None,
            },
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("deploy".to_string()),
                status: Some("IN_PROGRESS".to_string()),
                conclusion: None,
                context: None,
                state: None,
            },
        ];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Pending);
    }

    #[test]
    fn test_ci_status_from_rollup_status_context() {
        let items = vec![StatusCheckRollupItem {
            type_name: "StatusContext".to_string(),
            name: None,
            status: None,
            conclusion: None,
            context: Some("ci/test".to_string()),
            state: Some("PENDING".to_string()),
        }];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Pending);
    }

    #[test]
    fn test_ci_status_from_rollup_skipped() {
        let items = vec![StatusCheckRollupItem {
            type_name: "CheckRun".to_string(),
            name: Some("skip-check".to_string()),
            status: Some("COMPLETED".to_string()),
            conclusion: Some("SKIPPED".to_string()),
            context: None,
            state: None,
        }];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Success);
    }

    #[test]
    fn test_check_item_deserialize() {
        let json = r#"{
            "name": "build",
            "state": "SUCCESS",
            "bucket": "pass",
            "link": "https://example.com/run/1",
            "workflow": "CI",
            "description": "Build succeeded",
            "startedAt": "2024-01-01T00:00:00Z",
            "completedAt": "2024-01-01T00:05:00Z"
        }"#;
        let item: CheckItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.name, "build");
        assert_eq!(item.bucket.as_deref(), Some("pass"));
        assert_eq!(item.link.as_deref(), Some("https://example.com/run/1"));
    }

    #[test]
    fn test_check_item_deserialize_minimal() {
        let json = r#"{
            "name": "test",
            "state": "PENDING",
            "workflow": ""
        }"#;
        let item: CheckItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.name, "test");
        assert!(item.bucket.is_none());
        assert!(item.link.is_none());
    }

    #[test]
    fn test_status_check_rollup_item_deserialize() {
        let json = r#"{
            "__typename": "CheckRun",
            "name": "build",
            "status": "COMPLETED",
            "conclusion": "SUCCESS"
        }"#;
        let item: StatusCheckRollupItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.type_name, "CheckRun");
        assert_eq!(item.name.as_deref(), Some("build"));
        assert_eq!(item.conclusion.as_deref(), Some("SUCCESS"));
    }
}
