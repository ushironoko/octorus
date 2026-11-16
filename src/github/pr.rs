use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::client::{
    check_graphql_errors, format_gh_error, gh_api, gh_api_graphql, gh_api_paginate, gh_command,
    gh_command_allow_exit_codes, gh_command_output, FieldValue,
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
            // GraphQL returns "" where REST returns null. An empty body must
            // stay None so the reviewer prompt still uses its placeholder.
            body: pull.body.filter(|body| !body.is_empty()),
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

/// One page of the pull request list.
#[derive(Debug)]
pub struct PrListPage {
    pub items: Vec<PullRequestSummary>,
    pub has_more: bool,
    /// Cursor for the next page. `None` when [`Self::has_more`] is false.
    pub end_cursor: Option<String>,
}

/// GitHub rejects a connection `first` above this. A larger `limit` is several
/// queries of at most this many rows, not a shorter list.
const PR_LIST_GRAPHQL_PAGE_MAX: u32 = 100;

/// Same order and state set as `gh pr list`: `CREATED_AT` descending, and
/// `closed` includes merged. The screen reads the title, draft flag, author,
/// label names, and the CI mark. Checks and labels stop at 100, which is the
/// window `gh pr list --json` already used.
const PR_LIST_QUERY: &str = "query($owner:String!,$name:String!,$limit:Int!,$states:[PullRequestState!],$after:String){repository(owner:$owner,name:$name){pullRequests(states:$states,first:$limit,after:$after,orderBy:{field:CREATED_AT,direction:DESC}){nodes{number title state isDraft updatedAt author{__typename login} labels(first:100){nodes{name}} commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{__typename ...on CheckRun{status conclusion} ...on StatusContext{state}}}}}}}} pageInfo{hasNextPage endCursor}}}}";

pub async fn fetch_pr_list(repo: &str, state: PrStateFilter, limit: u32) -> Result<PrListPage> {
    fetch_pr_list_after(repo, state, None, limit).await
}

/// One page, starting after `after`. `None` reads the first page.
///
/// Each query asks for `limit` rows (at most 100, which is GraphQL's maximum
/// for `first`) and uses `pageInfo.hasNextPage` instead of fetching the next
/// row and dropping it. A `limit` above 100 continues from the cursor until
/// that many rows are collected or the list ends.
pub async fn fetch_pr_list_after(
    repo: &str,
    state: PrStateFilter,
    after: Option<&str>,
    limit: u32,
) -> Result<PrListPage> {
    let mut cursor = after
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string);
    if limit == 0 {
        return Ok(PrListPage {
            items: Vec::new(),
            has_more: false,
            end_cursor: cursor,
        });
    }

    let mut items = Vec::new();
    let mut remaining = limit;
    let mut has_more = false;
    let mut end_cursor = cursor.clone();

    while remaining > 0 {
        let page_size = remaining.min(PR_LIST_GRAPHQL_PAGE_MAX);
        let page = request_pr_list_page(repo, state, cursor.as_deref(), page_size).await?;
        let got = u32::try_from(page.items.len()).unwrap_or(page_size);
        has_more = page.has_more;
        end_cursor.clone_from(&page.end_cursor);
        items.extend(page.items);
        if !has_more || got < page_size {
            break;
        }
        remaining = remaining.saturating_sub(got);
        if remaining == 0 {
            break;
        }
        if page.end_cursor.as_deref().is_none_or(str::is_empty) {
            anyhow::bail!("GitHub GraphQL pageInfo missing endCursor");
        }
        cursor = page.end_cursor;
    }

    if has_more && end_cursor.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!("GitHub GraphQL pageInfo missing endCursor");
    }

    Ok(PrListPage {
        items,
        has_more,
        end_cursor,
    })
}

async fn request_pr_list_page(
    repo: &str,
    state: PrStateFilter,
    after: Option<&str>,
    limit: u32,
) -> Result<PrListPage> {
    let args = pr_list_command_args(repo, state, after, limit)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = gh_command_output(&arg_refs).await?;
    // `gh api graphql` exits 1 and prints `gh: <message>` plus the JSON body.
    // `gh pr list` prints `GraphQL: <message> (<path>)` and no body. The footer
    // reads that wording, so surface the errors array the same way.
    if let Some(message) = graphql_failure_message(output.stdout.trim()) {
        anyhow::bail!("gh command failed: {message}");
    }
    if !output.success {
        anyhow::bail!(
            "{}",
            format_gh_error(output.stderr.trim(), output.stdout.trim())
        );
    }
    let body: serde_json::Value =
        serde_json::from_str(&output.stdout).context("Failed to parse PR list response")?;
    check_graphql_errors(&body)?;
    parse_pr_list_payload(body, repo)
}

/// `gh pr list`'s GraphQL error line: `GraphQL: <message> (<path>)`, with
/// further errors joined by `, `. `None` when stdout is not that error body,
/// so a network or HTTP failure keeps the stderr `gh` already printed.
fn graphql_failure_message(stdout: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let errors = body.get("errors")?.as_array()?;
    if errors.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for error in errors {
        let Some(message) = error.get("message").and_then(|message| message.as_str()) else {
            continue;
        };
        let message = message.trim();
        if message.is_empty() {
            continue;
        }
        let path = error
            .get("path")
            .and_then(|path| path.as_array())
            .map(|segments| {
                segments
                    .iter()
                    .filter_map(|segment| match segment {
                        serde_json::Value::String(text) => Some(text.clone()),
                        serde_json::Value::Number(number) => Some(number.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .filter(|path| !path.is_empty());
        match path {
            Some(path) => parts.push(format!("{message} ({path})")),
            None => parts.push(message.to_string()),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("GraphQL: {}", parts.join(", ")))
    }
}

fn pr_list_state_values(state: PrStateFilter) -> &'static [&'static str] {
    match state {
        // `gh pr list -s closed` asks for CLOSED and MERGED. `all` adds OPEN.
        PrStateFilter::Open => &["OPEN"],
        PrStateFilter::Closed => &["CLOSED", "MERGED"],
        PrStateFilter::All => &["OPEN", "CLOSED", "MERGED"],
    }
}

struct RepoName<'a> {
    host: Option<&'a str>,
    owner: &'a str,
    name: &'a str,
}

/// `owner/repo`, or `host/owner/repo` as `gh pr list -R` accepts it.
/// A two-part name keeps gh's default host, including `GH_HOST`.
fn split_repo_name(repo: &str) -> Result<RepoName<'_>> {
    let mut parts = repo.split('/');
    let first = parts.next().unwrap_or("");
    let second = parts.next().unwrap_or("");
    let third = parts.next();
    let fourth = parts.next();
    match (first, second, third, fourth) {
        (owner, name, None, None) if !owner.is_empty() && !name.is_empty() => Ok(RepoName {
            host: None,
            owner,
            name,
        }),
        (host, owner, Some(name), None)
            if !host.is_empty() && !owner.is_empty() && !name.is_empty() =>
        {
            Ok(RepoName {
                host: Some(host),
                owner,
                name,
            })
        }
        _ => anyhow::bail!("Invalid repo format: expected 'owner/repo', got '{repo}'"),
    }
}

fn pr_list_command_args(
    repo: &str,
    state: PrStateFilter,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<String>> {
    let repo = split_repo_name(repo)?;
    let mut args = vec!["api".to_string()];
    if let Some(host) = repo.host {
        args.push("--hostname".to_string());
        args.push(host.to_string());
    }
    args.push("graphql".to_string());
    args.push("-f".to_string());
    args.push(format!("query={PR_LIST_QUERY}"));
    args.push("-f".to_string());
    args.push(format!("owner={}", repo.owner));
    args.push("-f".to_string());
    args.push(format!("name={}", repo.name));
    args.push("-F".to_string());
    args.push(format!("limit={limit}"));
    for value in pr_list_state_values(state) {
        args.push("-F".to_string());
        args.push(format!("states[]={value}"));
    }
    if let Some(cursor) = after.filter(|cursor| !cursor.is_empty()) {
        args.push("-f".to_string());
        args.push(format!("after={cursor}"));
    }
    Ok(args)
}

#[derive(Debug, Deserialize)]
struct GqlPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GqlNamed {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GqlActor {
    #[serde(default, rename = "__typename")]
    type_name: Option<String>,
    login: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GqlCheckNode {
    #[serde(rename = "__typename")]
    type_name: String,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct GqlNodes<T> {
    #[serde(default)]
    nodes: Vec<Option<T>>,
}

#[derive(Debug, Deserialize)]
struct GqlRollup {
    contexts: Option<GqlNodes<GqlCheckNode>>,
}

#[derive(Debug, Deserialize)]
struct GqlCommit {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<GqlRollup>,
}

#[derive(Debug, Deserialize)]
struct GqlCommitNode {
    commit: Option<GqlCommit>,
}

#[derive(Debug, Deserialize)]
struct GqlPrNode {
    number: u32,
    title: String,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    author: Option<GqlActor>,
    labels: Option<GqlNodes<GqlNamed>>,
    commits: Option<GqlNodes<GqlCommitNode>>,
}

#[derive(Debug, Deserialize)]
struct GqlPrConnection {
    #[serde(default)]
    nodes: Vec<Option<GqlPrNode>>,
    #[serde(rename = "pageInfo")]
    page_info: GqlPageInfo,
}

#[derive(Debug, Deserialize)]
struct GqlPrListRepository {
    #[serde(rename = "pullRequests")]
    pull_requests: GqlPrConnection,
}

#[derive(Debug, Deserialize)]
struct GqlPrListData {
    repository: Option<GqlPrListRepository>,
}

#[derive(Debug, Deserialize)]
struct GqlPrListPayload {
    data: Option<GqlPrListData>,
}

fn parse_pr_list_payload(body: serde_json::Value, repo: &str) -> Result<PrListPage> {
    let parsed: GqlPrListPayload =
        serde_json::from_value(body).context("Failed to parse PR list response")?;
    let Some(data) = parsed.data else {
        anyhow::bail!("GitHub GraphQL response missing data");
    };
    let Some(repository) = data.repository else {
        anyhow::bail!("Could not resolve to a Repository with the name '{repo}'");
    };
    let connection = repository.pull_requests;
    let items = connection
        .nodes
        .into_iter()
        .flatten()
        .map(summary_from_node)
        .collect();
    let end_cursor = connection
        .page_info
        .end_cursor
        .filter(|cursor| !cursor.is_empty());
    Ok(PrListPage {
        items,
        has_more: connection.page_info.has_next_page,
        end_cursor,
    })
}

fn summary_from_node(node: GqlPrNode) -> PullRequestSummary {
    let login = list_author_login(node.author);
    let labels = node
        .labels
        .map(|labels| {
            labels
                .nodes
                .into_iter()
                .flatten()
                .map(|label| Label { name: label.name })
                .collect()
        })
        .unwrap_or_default();
    PullRequestSummary {
        number: node.number,
        title: node.title,
        state: node.state,
        author: User { login },
        is_draft: node.is_draft,
        labels,
        updated_at: node.updated_at,
        status_check_rollup: rollup_from_commits(node.commits),
    }
}

/// `gh pr list --json author` is `Author.MarshalJSON` (`api/queries_issue.go`).
/// The list field is `author{login,...on User{id,name}}` (`api/query_builder.go`),
/// so `id` is set only for a User. An empty id is written as `app/` plus the
/// login: every other actor, and a null author (`app/` + ""). A User keeps
/// `login`. The same object sets `is_bot`; the list stores `login` only, which
/// is what the row and the text filter read.
fn list_author_login(author: Option<GqlActor>) -> String {
    match author {
        Some(author) if author.type_name.as_deref() == Some("User") => {
            author.login.unwrap_or_default()
        }
        Some(author) => format!("app/{}", author.login.unwrap_or_default()),
        None => "app/".to_string(),
    }
}

fn rollup_from_commits(commits: Option<GqlNodes<GqlCommitNode>>) -> Vec<StatusCheckRollupItem> {
    let Some(commits) = commits else {
        return Vec::new();
    };
    commits
        .nodes
        .into_iter()
        .flatten()
        .filter_map(|node| node.commit)
        .filter_map(|commit| commit.status_check_rollup)
        .filter_map(|rollup| rollup.contexts)
        .flat_map(|contexts| contexts.nodes)
        .flatten()
        .map(|check| StatusCheckRollupItem {
            type_name: check.type_name,
            name: None,
            status: check.status,
            conclusion: check.conclusion,
            context: None,
            state: check.state,
        })
        .collect()
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
    #[cfg(unix)]
    use serial_test::serial;

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

        let rest_without_body: PullRequest = serde_json::from_value(serde_json::json!({
            "number": 7,
            "node_id": "PR_node",
            "title": "Title",
            "body": null,
            "state": "open",
            "updated_at": "2024-01-01T00:00:00Z",
            "head": {"ref": "feature", "sha": "abc"},
            "base": {"ref": "main", "sha": "def"},
            "user": {"login": "octocat"}
        }))
        .unwrap();
        for graphql_body in [serde_json::json!(""), serde_json::Value::Null] {
            let mut response = snapshot_pull(serde_json::json!({
                "target": { "oid": "cafebabe" }
            }));
            response["data"]["repository"]["pullRequest"]["body"] = graphql_body;
            let parsed = parse_pr_snapshot(response).unwrap();
            assert_eq!(parsed.pr.body, rest_without_body.body);
            assert!(parsed.pr.body.is_none());
        }
    }

    #[test]
    fn parse_pr_snapshot_empty_body_is_none() {
        let mut body = snapshot_pull(serde_json::json!({ "target": { "oid": "cafebabe" } }));
        body["data"]["repository"]["pullRequest"]["body"] = serde_json::json!("");
        let snapshot = parse_pr_snapshot(body).unwrap();
        assert_eq!(snapshot.pr.body, None);
    }

    #[test]
    fn empty_body_snapshot_keeps_the_reviewer_placeholder() {
        let mut body = snapshot_pull(serde_json::json!({ "target": { "oid": "cafebabe" } }));
        body["data"]["repository"]["pullRequest"]["body"] = serde_json::json!("");
        let snapshot = parse_pr_snapshot(body).unwrap();
        let context = crate::ai::Context {
            repo: "owner/repo".to_string(),
            pr_number: snapshot.pr.number,
            pr_title: snapshot.pr.title.clone(),
            pr_body: snapshot.pr.body.clone(),
            diff: String::new(),
            working_dir: None,
            head_sha: snapshot.pr.head.sha.clone(),
            base_branch: snapshot.pr.base.ref_name.clone(),
            external_comments: Vec::new(),
            local_mode: false,
            file_patches: Vec::new(),
        };
        let prompt = crate::ai::PromptLoader::new(
            &crate::config::AiConfig::default(),
            std::path::Path::new("/tmp"),
        )
        .load_reviewer_prompt(&context, 1);
        assert!(
            prompt.contains("(No description provided)"),
            "an empty body uses the placeholder, got {prompt}"
        );
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

    fn arg_is(args: &[String], expected: &str) -> bool {
        args.iter().any(|arg| arg == expected)
    }

    #[test]
    fn pr_list_query_matches_gh_pr_list_order_states_and_fields() {
        let open =
            super::pr_list_command_args("owner/repo", PrStateFilter::Open, None, 30).unwrap();
        assert!(arg_is(&open, "limit=30"));
        assert!(arg_is(&open, "owner=owner"));
        assert!(arg_is(&open, "name=repo"));
        assert!(arg_is(&open, "states[]=OPEN"));
        assert!(!arg_is(&open, "states[]=CLOSED"));
        assert!(!arg_is(&open, "--hostname"));
        assert!(!open.iter().any(|arg| arg.starts_with("after=")));
        let query = open.iter().find(|arg| arg.starts_with("query=")).unwrap();
        assert!(query.contains("orderBy:{field:CREATED_AT,direction:DESC}"));
        assert!(query.contains("labels(first:100)"));
        assert!(query.contains("contexts(first:100)"));
        assert!(query.contains("isDraft"));
        assert!(query.contains("author{__typename login}"));
        assert!(query.contains("...on CheckRun{status conclusion}"));
        assert!(query.contains("...on StatusContext{state}"));
        assert!(!query.contains("workflowName"));
        assert!(!query.contains("detailsUrl"));
        assert!(!query.contains("description"));
        assert!(!query.contains("color"));

        let closed =
            super::pr_list_command_args("owner/repo", PrStateFilter::Closed, None, 30).unwrap();
        assert!(arg_is(&closed, "states[]=CLOSED"));
        assert!(arg_is(&closed, "states[]=MERGED"));
        assert!(!arg_is(&closed, "states[]=OPEN"));

        let all = super::pr_list_command_args("owner/repo", PrStateFilter::All, None, 30).unwrap();
        assert!(arg_is(&all, "states[]=OPEN"));
        assert!(arg_is(&all, "states[]=CLOSED"));
        assert!(arg_is(&all, "states[]=MERGED"));

        let paged = super::pr_list_command_args(
            "gh.example.test/acme/widgets",
            PrStateFilter::Open,
            Some("Y3Vyc29y="),
            30,
        )
        .unwrap();
        assert!(paged
            .windows(2)
            .any(|pair| { pair[0] == "--hostname" && pair[1] == "gh.example.test" }));
        assert!(arg_is(&paged, "owner=acme"));
        assert!(arg_is(&paged, "name=widgets"));
        assert!(arg_is(&paged, "after=Y3Vyc29y="));
        assert!(!arg_is(&paged, "owner=gh.example.test"));

        assert!(super::pr_list_command_args("noslash", PrStateFilter::Open, None, 30).is_err());
    }

    /// Rows `gh pr list --limit` requested on main. Page `page` (0-based) used
    /// `offset + limit + 1` because `gh pr list` has no offset, and gh split a
    /// request above 100 into one GraphQL query per 100 rows inside that process.
    fn main_pr_list_page_cost(page: u32, page_size: u32) -> (u32, u32) {
        let rows = page * page_size + page_size + 1;
        (rows, rows.div_ceil(100))
    }

    #[test]
    fn main_pr_list_paging_transfers_every_earlier_row() {
        let page_size = 30;
        assert_eq!(main_pr_list_page_cost(0, page_size), (31, 1));
        assert_eq!(main_pr_list_page_cost(1, page_size), (61, 1));
        assert_eq!(main_pr_list_page_cost(3, page_size), (121, 2));
        assert_eq!(main_pr_list_page_cost(9, page_size), (301, 4));
        let (rows, queries) = (0..10).fold((0, 0), |(rows, queries), page| {
            let (page_rows, page_queries) = main_pr_list_page_cost(page, page_size);
            (rows + page_rows, queries + page_queries)
        });
        assert_eq!(rows, 1660);
        assert_eq!(queries, 22);
    }

    fn list_page(
        nodes: Vec<serde_json::Value>,
        has_next: bool,
        cursor: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequests": {
                        "nodes": nodes,
                        "pageInfo": {"hasNextPage": has_next, "endCursor": cursor}
                    }
                }
            }
        })
    }

    fn list_node(
        number: u32,
        draft: bool,
        author: Option<&str>,
        labels: &[&str],
        checks: serde_json::Value,
    ) -> serde_json::Value {
        let author = author.map(|login| serde_json::json!({"__typename": "User", "login": login}));
        serde_json::json!({
            "number": number,
            "title": format!("PR {number}"),
            "state": "OPEN",
            "isDraft": draft,
            "updatedAt": "2025-01-01T00:00:00Z",
            "author": author,
            "labels": {"nodes": labels.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>()},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": checks}}}}]}
        })
    }

    fn check_run(conclusion: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "__typename": "CheckRun",
            "status": if conclusion.is_some() { "COMPLETED" } else { "IN_PROGRESS" },
            "conclusion": conclusion,
        })
    }

    #[test]
    fn pr_list_payload_keeps_draft_author_labels_and_ci_mark() {
        let body = list_page(
            vec![list_node(
                42,
                true,
                Some("alice"),
                &["bug", "ui", "perf"],
                serde_json::json!([
                    check_run(Some("FAILURE")),
                    {"__typename": "StatusContext", "state": "PENDING"}
                ]),
            )],
            true,
            Some("cursor-1"),
        );
        let page = super::parse_pr_list_payload(body, "owner/repo").unwrap();
        assert!(page.has_more);
        assert_eq!(page.end_cursor.as_deref(), Some("cursor-1"));
        let pr = &page.items[0];
        assert!(pr.is_draft);
        assert_eq!(pr.state, "OPEN");
        assert_eq!(pr.author.login, "alice");
        assert_eq!(
            pr.labels
                .iter()
                .map(|label| label.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bug", "ui", "perf"]
        );
        assert_eq!(
            CiStatus::from_rollup(&pr.status_check_rollup),
            CiStatus::Failure
        );
    }

    #[test]
    fn pr_list_payload_ci_matches_the_existing_rollup_rules() {
        let cases = [
            (serde_json::json!([]), CiStatus::None),
            (
                serde_json::json!([check_run(Some("SUCCESS"))]),
                CiStatus::Success,
            ),
            (
                serde_json::json!([check_run(Some("NEUTRAL")), check_run(Some("SKIPPED"))]),
                CiStatus::Success,
            ),
            (serde_json::json!([check_run(None)]), CiStatus::Pending),
            (
                serde_json::json!([check_run(Some("STALE"))]),
                CiStatus::Failure,
            ),
            (
                serde_json::json!([check_run(Some("STARTUP_FAILURE"))]),
                CiStatus::Failure,
            ),
            (
                serde_json::json!([check_run(Some("CANCELLED"))]),
                CiStatus::Failure,
            ),
            (
                serde_json::json!([{"__typename": "StatusContext", "state": "SUCCESS"}]),
                CiStatus::Success,
            ),
            (
                serde_json::json!([{"__typename": "StatusContext", "state": "PENDING"}]),
                CiStatus::Pending,
            ),
            (
                serde_json::json!([{"__typename": "StatusContext", "state": "EXPECTED"}]),
                CiStatus::Pending,
            ),
            (
                serde_json::json!([{"__typename": "StatusContext", "state": "ERROR"}]),
                CiStatus::Failure,
            ),
        ];
        for (checks, expected) in cases {
            let body = list_page(
                vec![list_node(1, false, Some("a"), &[], checks)],
                false,
                None,
            );
            let page = super::parse_pr_list_payload(body, "owner/repo").unwrap();
            assert_eq!(
                CiStatus::from_rollup(&page.items[0].status_check_rollup),
                expected
            );
        }
    }

    #[test]
    fn pr_list_author_login_matches_gh_pr_list() {
        let mut bot = list_node(1, false, Some("dependabot"), &[], serde_json::json!([]));
        bot["author"]["__typename"] = serde_json::json!("Bot");
        let mut mannequin = list_node(2, false, Some("aatmmr"), &[], serde_json::json!([]));
        mannequin["author"]["__typename"] = serde_json::json!("Mannequin");
        let mut org = list_node(3, false, Some("acme"), &[], serde_json::json!([]));
        org["author"]["__typename"] = serde_json::json!("Organization");
        let mut enterprise = list_node(4, false, Some("ent"), &[], serde_json::json!([]));
        enterprise["author"]["__typename"] = serde_json::json!("EnterpriseUserAccount");
        let user = list_node(5, false, Some("alice"), &[], serde_json::json!([]));
        let deleted = list_node(6, false, None, &[], serde_json::json!([]));
        let page = super::parse_pr_list_payload(
            list_page(
                vec![bot, mannequin, org, enterprise, user, deleted],
                false,
                None,
            ),
            "owner/repo",
        )
        .unwrap();
        let logins: Vec<&str> = page
            .items
            .iter()
            .map(|pr| pr.author.login.as_str())
            .collect();
        assert_eq!(
            logins,
            vec![
                "app/dependabot",
                "app/aatmmr",
                "app/acme",
                "app/ent",
                "alice",
                "app/",
            ]
        );
    }

    #[test]
    fn pr_list_graphql_error_uses_the_gh_pr_list_wording() {
        let stdout = r#"{"data":{"repository":null},"errors":[{"type":"NOT_FOUND","path":["repository"],"locations":[{"line":1,"column":9}],"message":"Could not resolve to a Repository with the name 'owner/missing'."}]}"#;
        assert_eq!(
            super::graphql_failure_message(stdout).as_deref(),
            Some(
                "GraphQL: Could not resolve to a Repository with the name 'owner/missing'. (repository)"
            )
        );
        assert!(super::graphql_failure_message("not json").is_none());
        assert!(super::graphql_failure_message(r#"{"data":{"repository":null}}"#).is_none());
    }

    #[test]
    fn pr_list_payload_keeps_a_pull_request_whose_author_was_deleted() {
        let body = list_page(
            vec![list_node(7, false, None, &[], serde_json::json!([]))],
            false,
            None,
        );
        let page = super::parse_pr_list_payload(body, "owner/repo").unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].number, 7);
        assert_eq!(page.items[0].author.login, "app/");
        assert!(!page.has_more);
        assert!(page.end_cursor.is_none());
    }

    #[test]
    fn missing_repository_payload_uses_the_not_found_phrase() {
        let body = serde_json::json!({"data": {"repository": null}});
        let error = super::parse_pr_list_payload(body, "owner/missing").unwrap_err();
        let failure = crate::github::GhFailure::from_error(&error);
        assert_eq!(
            failure.title(crate::github::FailureSurface::PrList),
            "Repository not found"
        );
        assert_eq!(failure.reason(), "Check the owner/repo name.");
    }

    #[test]
    fn graphql_error_body_keeps_the_repository_phrase() {
        let body = serde_json::json!({
            "errors": [{
                "message": "Could not resolve to a Repository with the name 'owner/missing'."
            }]
        });
        let error = super::check_graphql_errors(&body).unwrap_err();
        let failure = crate::github::GhFailure::from_error(&error);
        assert_eq!(
            failure.title(crate::github::FailureSurface::PrList),
            "Repository not found"
        );
    }

    fn list_calls(log: &str) -> Vec<String> {
        log.lines()
            .filter(|line| line.contains("pullRequests"))
            .map(str::to_string)
            .collect()
    }

    fn passes_after(call: &str) -> bool {
        call.split_whitespace().any(|arg| arg.starts_with("after="))
    }

    #[cfg(unix)]
    fn numbered_nodes(start: u32, count: u32) -> Vec<serde_json::Value> {
        (0..count)
            .map(|offset| list_node(start + offset, false, Some("a"), &[], serde_json::json!([])))
            .collect()
    }

    /// A limit above GraphQL's 100-row maximum is still returned in full.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn pr_list_limit_above_one_graphql_page_is_not_capped() {
        use crate::github::fake_gh::FakeGh;

        let gh = FakeGh::install("", "", "", "");
        gh.set_pr_list(&list_page(numbered_nodes(1, 100), true, Some("page-1")).to_string());
        gh.set_pr_list_after(&list_page(numbered_nodes(101, 30), true, Some("page-2")).to_string());

        let page = fetch_pr_list_after("owner/repo", PrStateFilter::Open, None, 130)
            .await
            .unwrap();
        let calls = list_calls(&gh.calls());
        assert_eq!(calls.len(), 2, "{}", gh.calls());
        assert!(
            calls[0].split_whitespace().any(|arg| arg == "limit=100"),
            "{}",
            calls[0]
        );
        assert!(!passes_after(&calls[0]), "{}", calls[0]);
        assert!(
            calls[1].split_whitespace().any(|arg| arg == "limit=30"),
            "{}",
            calls[1]
        );
        assert!(
            calls[1].split_whitespace().any(|arg| arg == "after=page-1"),
            "{}",
            calls[1]
        );
        assert_eq!(page.items.len(), 130);
        assert_eq!(page.items.first().map(|pr| pr.number), Some(1));
        assert_eq!(page.items.last().map(|pr| pr.number), Some(130));
        assert!(page.has_more);
        assert_eq!(page.end_cursor.as_deref(), Some("page-2"));
        assert!(!gh.calls().contains("--limit"), "{}", gh.calls());
    }

    #[tokio::test]
    async fn limit_zero_does_not_ask_github() {
        let page = fetch_pr_list_after("owner/repo", PrStateFilter::Open, Some("keep"), 0)
            .await
            .unwrap();
        assert!(page.items.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.end_cursor.as_deref(), Some("keep"));
    }
}
