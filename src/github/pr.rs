use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::client::{
    check_graphql_errors, gh_api, gh_api_graphql, gh_api_paginate, gh_command,
    gh_command_allow_exit_codes, FieldValue,
};
use crate::app::ReviewAction;

define_state_filter!(PrStateFilter);

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
                    // gh returns conclusion: "" for in-progress/queued checks
                    None | Some("") => {
                        has_pending = true;
                    }
                    Some(_) => return Self::Failure,
                },
                "StatusContext" => match item.state.as_deref() {
                    Some("SUCCESS") => {}
                    // gh returns state: "" for in-progress StatusContext entries
                    Some("PENDING") | Some("EXPECTED") | Some("") => has_pending = true,
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

/// Pull request metadata plus the live tip of its base branch.
///
/// `pr.base.sha` is the base SHA GitHub stored on the pull request. It does not
/// follow the base branch, so the file list can go stale while that SHA, the
/// head SHA, and `updated_at` stay put. `base_tip_sha` is `baseRef.target.oid`
/// from the same request. `None` means the ref is gone or the field was absent.
pub struct PrSnapshot {
    pub pr: PullRequest,
    pub base_tip_sha: Option<String>,
}

const PR_SNAPSHOT_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){id number title body state updatedAt headRefName headRefOid baseRefName baseRefOid author{__typename login} baseRef{target{oid}}}}}";

/// One metadata read: the pull request, and the current tip of its base branch.
pub async fn fetch_pr_snapshot(repo: &str, pr_number: u32) -> Result<PrSnapshot> {
    let Some((owner, name)) = repo.split_once('/') else {
        anyhow::bail!("repository {repo} is not owner/name");
    };
    if owner.is_empty() || name.is_empty() {
        anyhow::bail!("repository {repo} is not owner/name");
    }
    let number = pr_number.to_string();
    let response = gh_api_graphql(
        PR_SNAPSHOT_QUERY,
        &[
            ("owner", FieldValue::String(owner)),
            ("name", FieldValue::String(name)),
            ("number", FieldValue::Raw(&number)),
        ],
    )
    .await?;
    parse_pr_snapshot(response)
}

pub(crate) fn parse_pr_snapshot(response: serde_json::Value) -> Result<PrSnapshot> {
    // A `baseRef` resolution error still carries the pull request. The tip is
    // then missing, and the caller refetches the file list. Any other error
    // fails the read.
    let tip_unreadable = base_ref_errors_only(&response);
    if !tip_unreadable {
        check_graphql_errors(&response)?;
    }
    let parsed: GqlPrSnapshot =
        serde_json::from_value(response).context("Failed to parse pull request metadata")?;
    let pull = parsed
        .data
        .and_then(|data| data.repository)
        .and_then(|repo| repo.pull_request)
        .context("Pull request metadata was not in the response")?;
    let base_tip_sha = if tip_unreadable {
        None
    } else {
        pull.base_ref.and_then(|base_ref| {
            let oid = base_ref.target?.oid;
            if oid.is_empty() {
                None
            } else {
                Some(oid)
            }
        })
    };
    Ok(PrSnapshot {
        pr: PullRequest {
            number: pull.number,
            node_id: pull.id.filter(|id| !id.is_empty()),
            title: pull.title,
            body: pull.body,
            state: rest_state(&pull.state),
            head: Branch {
                ref_name: pull.head_ref_name,
                sha: pull.head_ref_oid,
            },
            base: Branch {
                ref_name: pull.base_ref_name,
                sha: pull.base_ref_oid,
            },
            user: User {
                login: rest_login(pull.author),
            },
            updated_at: pull.updated_at,
        },
        base_tip_sha,
    })
}

/// REST `state` is `open` or `closed`. GraphQL adds `MERGED` for a pull
/// request whose REST `state` is `closed`.
fn rest_state(state: &str) -> String {
    match state {
        "MERGED" => "closed".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// REST `user.login` for a bot is `{login}[bot]`. GraphQL `author.login` omits
/// that suffix. A deleted account is `author: null` here and `ghost` on REST.
fn rest_login(author: Option<GqlAuthor>) -> String {
    match author {
        Some(author) if author.type_name == "Bot" && !author.login.ends_with("[bot]") => {
            format!("{}[bot]", author.login)
        }
        Some(author) => author.login,
        None => "ghost".to_string(),
    }
}

fn base_ref_errors_only(response: &serde_json::Value) -> bool {
    let Some(errors) = response.get("errors").and_then(|errors| errors.as_array()) else {
        return false;
    };
    !errors.is_empty()
        && errors.iter().all(|error| {
            error
                .get("path")
                .and_then(|path| path.as_array())
                .is_some_and(|path| {
                    path.iter()
                        .any(|segment| segment.as_str() == Some("baseRef"))
                })
        })
}

#[derive(Deserialize)]
struct GqlPrSnapshot {
    data: Option<GqlPrSnapshotData>,
}

#[derive(Deserialize)]
struct GqlPrSnapshotData {
    repository: Option<GqlPrSnapshotRepo>,
}

#[derive(Deserialize)]
struct GqlPrSnapshotRepo {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GqlPrSnapshotPull>,
}

#[derive(Deserialize)]
struct GqlPrSnapshotPull {
    #[serde(default)]
    id: Option<String>,
    number: u32,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    author: Option<GqlAuthor>,
    #[serde(default, rename = "baseRef")]
    base_ref: Option<GqlBaseRef>,
}

#[derive(Deserialize)]
struct GqlAuthor {
    #[serde(default, rename = "__typename")]
    type_name: String,
    login: String,
}

#[derive(Deserialize)]
struct GqlBaseRef {
    target: Option<GqlOid>,
}

#[derive(Deserialize)]
struct GqlOid {
    oid: String,
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
    // One line so the test double logs the node id on the same CALL line as the query.
    let query = "query($pullRequestId:ID!,$after:String){node(id:$pullRequestId){... on PullRequest{files(first:100,after:$after){nodes{path viewerViewedState}pageInfo{hasNextPage endCursor}}}}}";

    let mut viewed_state = HashMap::new();
    let mut after: Option<String> = None;

    loop {
        let mut fields = vec![("pullRequestId", FieldValue::String(pr_node_id))];
        if let Some(cursor) = after.as_deref() {
            fields.push(("after", FieldValue::String(cursor)));
        }

        let response = gh_api_graphql(query, &fields).await?;

        check_graphql_errors(&response)?;

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

const MARK_VIEWED_QUERY: &str = r#"
mutation($pullRequestId: ID!, $path: String!) {
  markFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path }) {
    clientMutationId
  }
}
"#;

const UNMARK_VIEWED_QUERY: &str = r#"
mutation($pullRequestId: ID!, $path: String!) {
  unmarkFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path }) {
    clientMutationId
  }
}
"#;

pub async fn set_file_viewed(
    _repo: &str,
    pr_node_id: &str,
    path: &str,
    viewed: bool,
) -> Result<()> {
    let query = if viewed {
        MARK_VIEWED_QUERY
    } else {
        UNMARK_VIEWED_QUERY
    };

    let response = gh_api_graphql(
        query,
        &[
            ("pullRequestId", FieldValue::String(pr_node_id)),
            ("path", FieldValue::String(path)),
        ],
    )
    .await?;

    check_graphql_errors(&response)?;

    Ok(())
}

/// ページネーション結果
pub struct PrListPage {
    pub items: Vec<PullRequestSummary>,
    pub has_more: bool,
}

pub async fn fetch_pr_list(repo: &str, state: PrStateFilter, limit: u32) -> Result<PrListPage> {
    fetch_pr_list_with_offset(repo, state, 0, limit).await
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

    fn snapshot_pull(base_ref: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "id": "PR_node",
                        "number": 7,
                        "title": "Title",
                        "body": "body",
                        "state": "OPEN",
                        "updatedAt": "2024-01-01T00:00:00Z",
                        "headRefName": "feature",
                        "headRefOid": "abc",
                        "baseRefName": "main",
                        "baseRefOid": "def",
                        "author": { "login": "octocat" },
                        "baseRef": base_ref,
                    }
                }
            }
        })
    }

    #[test]
    fn parse_pr_snapshot_reads_the_base_branch_tip() {
        let snapshot = parse_pr_snapshot(snapshot_pull(serde_json::json!({
            "target": { "oid": "cafebabe" }
        })))
        .unwrap();
        assert_eq!(snapshot.base_tip_sha.as_deref(), Some("cafebabe"));
        assert_eq!(snapshot.pr.state, "open");
        assert_eq!(snapshot.pr.base.sha, "def");
        assert_eq!(snapshot.pr.head.sha, "abc");
        assert_eq!(snapshot.pr.node_id.as_deref(), Some("PR_node"));
        assert_eq!(snapshot.pr.user.login, "octocat");
    }

    #[test]
    fn parse_pr_snapshot_bot_author_uses_the_rest_login() {
        let mut body = snapshot_pull(serde_json::json!({ "target": { "oid": "cafebabe" } }));
        body["data"]["repository"]["pullRequest"]["author"] = serde_json::json!({
            "__typename": "Bot",
            "login": "dependabot"
        });
        let snapshot = parse_pr_snapshot(body).unwrap();
        assert_eq!(snapshot.pr.user.login, "dependabot[bot]");
    }

    #[test]
    fn parse_pr_snapshot_null_author_is_ghost() {
        let mut body = snapshot_pull(serde_json::json!({ "target": { "oid": "cafebabe" } }));
        body["data"]["repository"]["pullRequest"]["author"] = serde_json::Value::Null;
        let snapshot = parse_pr_snapshot(body).unwrap();
        assert_eq!(snapshot.pr.user.login, "ghost");
    }

    #[test]
    fn parse_pr_snapshot_merged_state_is_closed() {
        let mut body = snapshot_pull(serde_json::json!({ "target": { "oid": "cafebabe" } }));
        body["data"]["repository"]["pullRequest"]["state"] = serde_json::json!("MERGED");
        let snapshot = parse_pr_snapshot(body).unwrap();
        assert_eq!(snapshot.pr.state, "closed");
    }

    #[test]
    fn parse_pr_snapshot_matches_the_rest_pull_request() {
        let rest: PullRequest = serde_json::from_value(serde_json::json!({
            "number": 7,
            "node_id": "PR_node",
            "title": "Title",
            "body": "body",
            "state": "open",
            "updated_at": "2024-01-01T00:00:00Z",
            "head": {"ref": "feature", "sha": "abc"},
            "base": {"ref": "main", "sha": "def"},
            "user": {"login": "octocat"},
            "draft": true,
            "created_at": "2023-12-01T00:00:00Z",
            "labels": [{"name": "bug"}]
        }))
        .unwrap();
        let snapshot = parse_pr_snapshot(snapshot_pull(serde_json::json!({
            "target": { "oid": "cafebabe" }
        })))
        .unwrap();

        assert_eq!(snapshot.pr.number, rest.number);
        assert_eq!(snapshot.pr.node_id, rest.node_id);
        assert_eq!(snapshot.pr.title, rest.title);
        assert_eq!(snapshot.pr.body, rest.body);
        assert_eq!(snapshot.pr.state, rest.state);
        assert_eq!(snapshot.pr.user.login, rest.user.login);
        assert_eq!(snapshot.pr.head.ref_name, rest.head.ref_name);
        assert_eq!(snapshot.pr.head.sha, rest.head.sha);
        assert_eq!(snapshot.pr.base.ref_name, rest.base.ref_name);
        assert_eq!(snapshot.pr.base.sha, rest.base.sha);
        assert_eq!(snapshot.pr.updated_at, rest.updated_at);
    }

    #[test]
    fn parse_pr_snapshot_missing_base_ref_has_no_tip() {
        let snapshot = parse_pr_snapshot(snapshot_pull(serde_json::Value::Null)).unwrap();
        assert!(snapshot.base_tip_sha.is_none());
        assert_eq!(snapshot.pr.updated_at, "2024-01-01T00:00:00Z");
    }

    #[test]
    fn parse_pr_snapshot_null_target_has_no_tip() {
        let snapshot =
            parse_pr_snapshot(snapshot_pull(serde_json::json!({ "target": null }))).unwrap();
        assert!(snapshot.base_tip_sha.is_none());
    }

    #[test]
    fn parse_pr_snapshot_base_ref_error_keeps_the_pull_request_without_a_tip() {
        let mut body = snapshot_pull(serde_json::Value::Null);
        body["errors"] = serde_json::json!([{
            "message": "Could not resolve baseRef",
            "path": ["repository", "pullRequest", "baseRef"]
        }]);
        let snapshot = parse_pr_snapshot(body).unwrap();
        assert!(snapshot.base_tip_sha.is_none());
        assert_eq!(snapshot.pr.title, "Title");
    }

    #[test]
    fn parse_pr_snapshot_other_graphql_error_fails() {
        let mut body = snapshot_pull(serde_json::json!({ "target": { "oid": "cafebabe" } }));
        body["errors"] = serde_json::json!([{
            "message": "nope",
            "path": ["repository", "pullRequest", "title"]
        }]);
        assert!(parse_pr_snapshot(body).is_err());
    }

    #[test]
    fn parse_pr_snapshot_graphql_error_fails() {
        let error = parse_pr_snapshot(serde_json::json!({
            "errors": [{ "message": "base ref failed" }]
        }));
        assert!(error.is_err());
    }

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
    fn test_ci_status_from_rollup_empty_conclusion_is_pending() {
        // gh returns conclusion: "" (not null) for in-progress/queued checks
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
                status: Some("IN_PROGRESS".to_string()),
                conclusion: Some("".to_string()),
                context: None,
                state: None,
            },
        ];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Pending);
    }

    #[test]
    fn test_ci_status_from_rollup_empty_state_is_pending() {
        // gh may return state: "" for in-progress StatusContext entries,
        // analogous to conclusion: "" for CheckRun.
        let items = vec![StatusCheckRollupItem {
            type_name: "StatusContext".to_string(),
            name: None,
            status: None,
            conclusion: None,
            context: Some("ci/in-progress".to_string()),
            state: Some("".to_string()),
        }];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Pending);
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
