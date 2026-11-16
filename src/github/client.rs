use anyhow::{Context, Result};
use std::io;
use std::process::Command;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Error)]
pub enum DetectRepoError {
    #[error("Not a git repository. Use --repo to specify.")]
    NotGitRepo,
    #[error("No GitHub remote found. Use --repo to specify.")]
    NoGitHubRemote,
    #[error("gh CLI error: {0}")]
    GhError(String),
}

/// Detect the repository name from the current directory using `gh repo view`
pub async fn detect_repo() -> std::result::Result<String, DetectRepoError> {
    let result = tokio::task::spawn_blocking(|| {
        let output = Command::new("gh")
            .args([
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "-q",
                ".nameWithOwner",
            ])
            .output();

        match output {
            Ok(output) => {
                if output.status.success() {
                    let repo = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if repo.is_empty() {
                        Err(DetectRepoError::NoGitHubRemote)
                    } else {
                        Ok(repo)
                    }
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if stderr.contains("not a git repository") {
                        Err(DetectRepoError::NotGitRepo)
                    } else if stderr.contains("no git remotes")
                        || stderr.contains("could not determine")
                    {
                        Err(DetectRepoError::NoGitHubRemote)
                    } else {
                        Err(DetectRepoError::GhError(stderr.trim().to_string()))
                    }
                }
            }
            Err(e) => Err(DetectRepoError::GhError(format!(
                "Failed to execute gh CLI: {}",
                e
            ))),
        }
    })
    .await;

    match result {
        Ok(r) => r,
        Err(e) => Err(DetectRepoError::GhError(format!(
            "spawn_blocking task panicked: {}",
            e
        ))),
    }
}

fn format_gh_error(stderr: &str, stdout: &str) -> String {
    tracing::debug!(stderr = %stderr, stdout = %stdout, "gh command failed");
    if stdout.is_empty() {
        format!("gh command failed: {}", stderr)
    } else {
        let truncated: String = stdout.chars().take(200).collect();
        let suffix = if stdout.len() > truncated.len() {
            "..."
        } else {
            ""
        };
        format!("gh command failed: {} ({}{})", stderr, truncated, suffix)
    }
}

/// Build a child process for a GitHub CLI query.
///
/// `kill_on_drop` is set here rather than at each call site so cancellation
/// actually stops the work: dropping the fetch future must take the child with
/// it, otherwise a cancelled fetch only discards the result while the process
/// keeps running to completion.
fn spawnable_command(program: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.kill_on_drop(true);
    cmd
}

/// Execute gh CLI command and return stdout
pub async fn gh_command(args: &[&str]) -> Result<String> {
    let mut command = spawnable_command("gh");
    let output = command
        .args(args)
        .output()
        .await
        .context("Failed to execute gh CLI - is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!("{}", format_gh_error(stderr.trim(), stdout.trim()));
    }

    String::from_utf8(output.stdout).context("gh output contains invalid UTF-8")
}

/// Execute gh CLI command, treating specified exit codes as success.
/// For example, `gh pr checks` returns exit code 8 when checks are pending.
pub async fn gh_command_allow_exit_codes(args: &[&str], allowed_codes: &[i32]) -> Result<String> {
    let mut command = spawnable_command("gh");
    let output = command
        .args(args)
        .output()
        .await
        .context("Failed to execute gh CLI - is it installed?")?;

    let code = output.status.code().unwrap_or(-1);
    if output.status.success() || allowed_codes.contains(&code) {
        String::from_utf8(output.stdout).context("gh output contains invalid UTF-8")
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!("{}", format_gh_error(stderr.trim(), stdout.trim()));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhFailureKind {
    Offline,
    Unauthenticated,
    CliMissing,
    RateLimited,
    Other,
}

const RATE_LIMIT_MARKERS: &[&str] = &[
    "rate limit exceeded",
    "rate limit already exceeded",
    "secondary rate limit",
    "http 429",
];

const UNAUTHENTICATED_MARKERS: &[&str] = &[
    "not logged into any github hosts",
    "gh auth login",
    "bad credentials",
    "http 401",
];

const OFFLINE_MARKERS: &[&str] = &[
    "dial tcp",
    "no such host",
    "i/o timeout",
    "connection refused",
    "network is unreachable",
    "proxyconnect",
    "tls handshake timeout",
    "temporary failure in name resolution",
    "could not resolve host",
    "error connecting to",
    "check your internet connection",
];

pub fn classify_gh_failure(error: &anyhow::Error) -> GhFailureKind {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io_error| io_error.kind() == io::ErrorKind::NotFound)
    }) {
        return GhFailureKind::CliMissing;
    }

    let message = error_chain_lowercased(error);
    if message_has_any(&message, RATE_LIMIT_MARKERS) {
        GhFailureKind::RateLimited
    } else if message_has_any(&message, UNAUTHENTICATED_MARKERS) {
        GhFailureKind::Unauthenticated
    } else if message_has_any(&message, OFFLINE_MARKERS) {
        GhFailureKind::Offline
    } else {
        GhFailureKind::Other
    }
}

pub(super) fn error_chain_lowercased(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ")
        .to_ascii_lowercase()
}

fn message_has_any(message: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| message.contains(marker))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhFailure {
    pub kind: GhFailureKind,
    pub message: String,
}

impl std::fmt::Display for GhFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Which pane a failure is drawn in. Known kinds keep one title on every
/// surface. `Other` takes its title from the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureSurface {
    PrList,
    IssueList,
    FileList,
    Comments,
    CockpitCounts,
    Inbox,
}

/// How loudly a failure is drawn. Warning is `⚠`, danger is `✕`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureSeverity {
    Warning,
    Danger,
}

/// Display-only refinement of [`GhFailureKind`]. `NotFound` and `NoAccess` are
/// not kinds: HTTP 404 stays [`GhFailureKind::Other`], and only these methods
/// read the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayKind {
    Offline,
    Unauthenticated,
    CliMissing,
    RateLimited,
    NotFound,
    NoAccess,
    Other,
}

impl GhFailure {
    pub fn from_error(error: &anyhow::Error) -> Self {
        Self {
            kind: classify_gh_failure(error),
            message: error.to_string(),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self {
            kind: GhFailureKind::Other,
            message: message.into(),
        }
    }

    /// A failure whose only remaining evidence is the text. Classification uses
    /// the same markers as [`classify_gh_failure`]; 404 and 403 stay `Other`
    /// until [`Self::title`] refines them.
    pub fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::from_error(&anyhow::Error::msg(message))
    }

    pub fn title(&self, surface: FailureSurface) -> String {
        match self.display_kind() {
            DisplayKind::Offline => "Can't reach GitHub".to_string(),
            DisplayKind::Unauthenticated => "gh is not authenticated".to_string(),
            DisplayKind::CliMissing => "gh is not installed".to_string(),
            DisplayKind::RateLimited => "GitHub API rate limit exceeded".to_string(),
            DisplayKind::NotFound => "Repository not found".to_string(),
            DisplayKind::NoAccess => "No access to this repository".to_string(),
            DisplayKind::Other => match surface {
                FailureSurface::PrList => "Couldn't load pull requests",
                FailureSurface::IssueList => "Couldn't load issues",
                FailureSurface::FileList => "Couldn't load files",
                FailureSurface::Comments => "Couldn't load comments",
                FailureSurface::CockpitCounts => "Couldn't load counts",
                FailureSurface::Inbox => "Couldn't load the inbox",
            }
            .to_string(),
        }
    }

    /// The line under the title. An empty string means the pane draws no reason
    /// line. `Other` is the message collapsed onto one line; the pane truncates
    /// it to the available width.
    pub fn reason(&self) -> String {
        match self.display_kind() {
            DisplayKind::Offline => "Check your network connection.".to_string(),
            DisplayKind::Unauthenticated => "run: gh auth login".to_string(),
            DisplayKind::CliMissing => {
                "Install GitHub CLI (https://cli.github.com), then retry.".to_string()
            }
            DisplayKind::RateLimited => "Try again later.".to_string(),
            DisplayKind::NotFound => "Check the owner/repo name.".to_string(),
            DisplayKind::NoAccess => "Check your permissions.".to_string(),
            DisplayKind::Other => one_line_message(&self.message),
        }
    }

    pub fn severity(&self) -> FailureSeverity {
        match self.display_kind() {
            DisplayKind::Offline | DisplayKind::RateLimited => FailureSeverity::Warning,
            DisplayKind::Unauthenticated
            | DisplayKind::CliMissing
            | DisplayKind::NotFound
            | DisplayKind::NoAccess
            | DisplayKind::Other => FailureSeverity::Danger,
        }
    }

    fn display_kind(&self) -> DisplayKind {
        match self.kind {
            GhFailureKind::Offline => DisplayKind::Offline,
            GhFailureKind::Unauthenticated => DisplayKind::Unauthenticated,
            GhFailureKind::CliMissing => DisplayKind::CliMissing,
            GhFailureKind::RateLimited => DisplayKind::RateLimited,
            GhFailureKind::Other => refine_other_message(&self.message),
        }
    }
}

/// 404 and 403 are a display refinement of `Other`. A bare "not found" is not
/// a repository miss: `command not found` would match it.
fn refine_other_message(message: &str) -> DisplayKind {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("http 404") || lowered.contains("could not resolve to a repository") {
        DisplayKind::NotFound
    } else if lowered.contains("http 403") || lowered.contains("forbidden") {
        DisplayKind::NoAccess
    } else {
        DisplayKind::Other
    }
}

/// Newlines become spaces so the reason is one line. Blank text stays blank,
/// and the pane then skips the reason line.
fn one_line_message(message: &str) -> String {
    let mut line = String::new();
    for part in message.split(['\n', '\r']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(part);
    }
    line
}

/// Execute gh api command with JSON output
pub async fn gh_api(endpoint: &str) -> Result<serde_json::Value> {
    let output = gh_command(&["api", endpoint]).await?;
    serde_json::from_str(&output).context("Failed to parse gh api response as JSON")
}

/// Execute gh api command with automatic pagination for array endpoints.
/// Fetches all pages and merges into a single JSON array.
/// Caller should include `per_page=100` in endpoint if desired.
pub async fn gh_api_paginate(endpoint: &str) -> Result<serde_json::Value> {
    let output = gh_command(&["api", "--paginate", "--slurp", endpoint]).await?;
    let pages: Vec<serde_json::Value> =
        serde_json::from_str(&output).context("Failed to parse gh api paginated response")?;
    flatten_pages(pages)
}

/// Flatten an array of JSON arrays (from --paginate --slurp) into a single array.
/// Returns an error if any page is not a JSON array.
pub(super) fn flatten_pages(pages: Vec<serde_json::Value>) -> Result<serde_json::Value> {
    let mut result = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        match page {
            serde_json::Value::Array(items) => result.extend(items.iter().cloned()),
            other => anyhow::bail!(
                "Expected JSON array for page {}, got {}",
                i + 1,
                other_type_name(other)
            ),
        }
    }
    Ok(serde_json::Value::Array(result))
}

fn other_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Field type for gh api command
pub enum FieldValue<'a> {
    /// String field (-f)
    String(&'a str),
    /// Raw/typed field (-F) - for integers, booleans, null
    Raw(&'a str),
}

fn build_field_args(fields: &[(&str, FieldValue<'_>)]) -> Vec<String> {
    let mut args = Vec::new();
    for (key, value) in fields {
        match value {
            FieldValue::String(v) => {
                args.push("-f".to_string());
                args.push(format!("{}={}", key, v));
            }
            FieldValue::Raw(v) => {
                args.push("-F".to_string());
                args.push(format!("{}={}", key, v));
            }
        }
    }
    args
}

/// Execute gh api with method and fields
pub async fn gh_api_post(
    endpoint: &str,
    fields: &[(&str, FieldValue<'_>)],
) -> Result<serde_json::Value> {
    let mut args = vec![
        "api".to_string(),
        "--method".to_string(),
        "POST".to_string(),
        endpoint.to_string(),
    ];
    args.extend(build_field_args(fields));
    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tracing::debug!(args = ?args_refs, "gh api post");
    let output = gh_command(&args_refs).await?;
    serde_json::from_str(&output).context("Failed to parse gh api response as JSON")
}

/// Execute `gh api` with a JSON request body read from stdin.
///
/// Use this when the body has nested arrays or objects, which the `-f`/`-F`
/// field syntax cannot express (for example the `comments` array of a review).
/// On this path the body is not parsed by gh, so `@file` and `key=value`
/// syntax in the text are sent as written.
pub(super) async fn gh_api_post_json(
    endpoint: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    let payload = serde_json::to_vec(body).context("Failed to serialize gh api request body")?;
    tracing::debug!(endpoint = %endpoint, "gh api post json");
    let output = run_with_stdin(
        "gh",
        &[
            "api",
            "--method",
            "POST",
            endpoint,
            "--header",
            "Content-Type: application/json",
            "--input",
            "-",
        ],
        &payload,
    )
    .await?;
    serde_json::from_str(&output).context("Failed to parse gh api response as JSON")
}

/// Run `program` with `args`, write `stdin_bytes` to its standard input, and
/// return its stdout. The write runs alongside the wait so a payload larger
/// than the pipe buffer cannot deadlock against a child that is writing stdout.
async fn run_with_stdin(program: &str, args: &[&str], stdin_bytes: &[u8]) -> Result<String> {
    let mut command = spawnable_command(program);
    command
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .context("Failed to execute gh CLI - is it installed?")?;
    let mut child_stdin = child
        .stdin
        .take()
        .context("Failed to open stdin of gh CLI")?;
    let payload = stdin_bytes.to_vec();
    let write = async move { child_stdin.write_all(&payload).await };
    let (write_result, output) = tokio::join!(write, child.wait_with_output());
    let output = output.context("Failed to wait for gh CLI")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!("{}", format_gh_error(stderr.trim(), stdout.trim()));
    }
    write_result.context("Failed to write request body to gh CLI")?;
    String::from_utf8(output.stdout).context("gh output contains invalid UTF-8")
}

/// Execute gh graphql API with query and variables.
pub async fn gh_api_graphql(
    query: &str,
    fields: &[(&str, FieldValue<'_>)],
) -> Result<serde_json::Value> {
    let mut args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={}", query),
    ];
    args.extend(build_field_args(fields));
    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tracing::debug!(args = ?args_refs, "gh api graphql");
    let output = gh_command(&args_refs).await?;
    serde_json::from_str(&output).context("Failed to parse gh graphql response as JSON")
}

pub fn check_graphql_errors(response: &serde_json::Value) -> Result<()> {
    if let Some(errors) = response.get("errors") {
        anyhow::bail!("GitHub GraphQL returned errors: {}", errors);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io;

    #[test]
    fn test_flatten_pages_single_page() {
        let pages = vec![json!([1, 2, 3])];
        let result = flatten_pages(pages).unwrap();
        assert_eq!(result, json!([1, 2, 3]));
    }

    #[test]
    fn test_flatten_pages_multiple_pages() {
        let pages = vec![json!([1, 2]), json!([3, 4]), json!([5])];
        let result = flatten_pages(pages).unwrap();
        assert_eq!(result, json!([1, 2, 3, 4, 5]));
    }

    #[test]
    fn test_flatten_pages_empty() {
        let pages: Vec<serde_json::Value> = vec![];
        let result = flatten_pages(pages).unwrap();
        assert_eq!(result, json!([]));
    }

    #[test]
    fn test_flatten_pages_empty_arrays() {
        let pages = vec![json!([]), json!([]), json!([])];
        let result = flatten_pages(pages).unwrap();
        assert_eq!(result, json!([]));
    }

    #[test]
    fn test_flatten_pages_rejects_non_array_object() {
        let pages = vec![json!([1, 2]), json!({"key": "value"})];
        let err = flatten_pages(pages).unwrap_err();
        assert!(err.to_string().contains("Expected JSON array for page 2"));
        assert!(err.to_string().contains("object"));
    }

    #[test]
    fn test_flatten_pages_rejects_non_array_string() {
        let pages = vec![json!("not an array")];
        let err = flatten_pages(pages).unwrap_err();
        assert!(err.to_string().contains("Expected JSON array for page 1"));
        assert!(err.to_string().contains("string"));
    }

    #[test]
    fn test_flatten_pages_rejects_null() {
        let pages = vec![json!([1]), json!(null)];
        let err = flatten_pages(pages).unwrap_err();
        assert!(err.to_string().contains("page 2"));
        assert!(err.to_string().contains("null"));
    }

    /// Dropping a GitHub fetch must take the child process with it.
    ///
    /// Exercises the builder every `gh` invocation in this module uses, with
    /// an absolute stand-in program so the test changes no process-wide state.
    /// Mutating `PATH` here would make concurrently running tests resolve this
    /// test's stand-in instead of the real `gh` binary.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_dropped_gh_fetch_kills_the_child_it_spawned() {
        let dir = tempfile::tempdir().unwrap();
        let started = dir.path().join("started");
        let completed = dir.path().join("completed");
        let script = format!(
            ": > {started}; sleep 1; : > {completed}",
            started = started.display(),
            completed = completed.display(),
        );

        let mut command = spawnable_command("/bin/sh");
        command.args(["-c", &script]);
        let fetch = command.output();

        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        drop(fetch);
        tokio::time::sleep(std::time::Duration::from_millis(1_400)).await;

        assert!(
            started.exists(),
            "the stand-in child must have been spawned before the drop"
        );
        assert!(
            !completed.exists(),
            "a dropped gh fetch must not leave its child running to completion"
        );
    }

    #[test]
    fn classify_gh_failure_matches_the_locked_commit_lookup_inputs() {
        let missing = anyhow::Error::new(io::Error::from(io::ErrorKind::NotFound))
            .context("Failed to execute gh CLI - is it installed?");
        assert_eq!(classify_gh_failure(&missing), GhFailureKind::CliMissing);

        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(
                "GitHub CLI command failed: not logged into any GitHub hosts; run gh auth login"
            )),
            GhFailureKind::Unauthenticated
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(
                "GitHub CLI command failed: HTTP 401: Bad credentials"
            )),
            GhFailureKind::Unauthenticated
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(
                "GitHub CLI command failed: API rate limit exceeded"
            )),
            GhFailureKind::RateLimited
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(
                "GitHub CLI command failed: secondary rate limit"
            )),
            GhFailureKind::RateLimited
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("GitHub CLI command failed: HTTP 429")),
            GhFailureKind::RateLimited
        );
    }

    #[test]
    fn classify_gh_failure_treats_not_found_kind_as_cli_missing() {
        let missing = anyhow::Error::new(io::Error::from(io::ErrorKind::NotFound))
            .context("Failed to execute gh CLI - is it installed?");
        assert_eq!(classify_gh_failure(&missing), GhFailureKind::CliMissing);

        // Flattening to Display drops the io::Error, so the same words are not CliMissing.
        let flattened = anyhow::anyhow!("{missing}");
        assert_eq!(classify_gh_failure(&flattened), GhFailureKind::Other);

        let text_only = anyhow::anyhow!("Failed to execute gh CLI - is it installed?");
        assert_eq!(classify_gh_failure(&text_only), GhFailureKind::Other);

        let permission = anyhow::Error::new(io::Error::from(io::ErrorKind::PermissionDenied))
            .context("Failed to execute gh CLI - is it installed?");
        assert_eq!(classify_gh_failure(&permission), GhFailureKind::Other);
    }

    #[test]
    fn classify_gh_failure_marks_each_offline_phrase() {
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("dial tcp 127.0.0.1:443")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("no such host")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("i/o timeout")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("connection refused")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("network is unreachable")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("proxyconnect")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("tls handshake timeout")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("temporary failure in name resolution")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("could not resolve host")),
            GhFailureKind::Offline
        );
    }

    #[test]
    fn classify_gh_failure_marks_gh_dns_error_offline() {
        // gh printError for *net.DNSError, cli/cli internal/ghcmd/cmd.go.
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("error connecting to api.github.com")),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(
                "check your internet connection or https://githubstatus.com"
            )),
            GhFailureKind::Offline
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(
                "error connecting to api.github.com\n\
                 check your internet connection or https://githubstatus.com"
            )),
            GhFailureKind::Offline
        );
    }

    #[test]
    fn classify_gh_failure_marks_graphql_rate_limit_already_exceeded() {
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(
                "gh: API rate limit already exceeded for user ID 583231."
            )),
            GhFailureKind::RateLimited
        );
    }

    #[test]
    fn classify_gh_failure_leaves_eof_and_http_404_as_other() {
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("eof")),
            GhFailureKind::Other
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("EOF")),
            GhFailureKind::Other
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!("connection reset by peer")),
            GhFailureKind::Other
        );
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(
                "GitHub CLI command failed: HTTP 403: SSO authorization required"
            )),
            GhFailureKind::Other
        );

        for message in [
            "gh command failed: gh: No commit found for SHA: deadbeef (HTTP 404)",
            "gh command failed: gh: No commit found for SHA: deadbeef",
            "gh command failed: HTTP 404: Not Found \
             (https://api.github.com/repos/owner/repo/commits/deadbeef/pulls)",
        ] {
            assert_eq!(
                classify_gh_failure(&anyhow::anyhow!(message)),
                GhFailureKind::Other,
                "{message}"
            );
        }
    }

    /// The JSON body must reach the child unchanged, byte for byte.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_with_stdin_delivers_the_body_to_the_child() {
        let payload = br#"{"comments":[{"path":"src/a.rs","body":"multi\nline @notes.txt"}]}"#;
        let out = run_with_stdin("/bin/cat", &[], payload).await.unwrap();
        assert_eq!(out.as_bytes(), payload);
    }

    /// A body larger than a pipe buffer must not deadlock against a child that
    /// writes stdout while it reads stdin.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_with_stdin_survives_a_body_larger_than_a_pipe_buffer() {
        let payload = vec![b'x'; 1 << 20];
        let out = run_with_stdin("/bin/cat", &[], &payload).await.unwrap();
        assert_eq!(out.len(), payload.len());
    }

    /// A failing `gh` must surface its stderr, which is what the user sees.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_with_stdin_reports_the_failure_with_stderr() {
        let err = run_with_stdin(
            "/bin/sh",
            &["-c", "echo 'HTTP 422: Unprocessable Entity' >&2; exit 1"],
            b"{}",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("HTTP 422"), "{err}");
    }

    #[test]
    fn classify_gh_failure_prefers_rate_limit_over_http_404() {
        let message = "API rate limit exceeded (HTTP 404)";
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(message)),
            GhFailureKind::RateLimited
        );
    }

    fn shown(kind: GhFailureKind, message: &str) -> GhFailure {
        GhFailure {
            kind,
            message: message.to_string(),
        }
    }

    #[test]
    fn http_404_stays_other_and_reads_as_repository_not_found() {
        for message in [
            "gh command failed: gh: Not Found (HTTP 404)",
            "Could not resolve to a repository",
            "COULD NOT RESOLVE TO A REPOSITORY with owner/name",
        ] {
            let failure = GhFailure::from_message(message);
            assert_eq!(failure.kind, GhFailureKind::Other, "{message}");
            assert_eq!(
                failure.title(FailureSurface::PrList),
                "Repository not found"
            );
            assert_eq!(failure.reason(), "Check the owner/repo name.");
            assert_eq!(failure.severity(), FailureSeverity::Danger);
            assert_eq!(
                failure.title(FailureSurface::IssueList),
                "Repository not found",
                "a known display kind keeps one title on every surface"
            );
        }
    }

    #[test]
    fn bare_not_found_is_not_a_missing_repository() {
        let failure = GhFailure::from_message("gh: command not found");
        assert_eq!(failure.kind, GhFailureKind::Other);
        assert_eq!(
            failure.title(FailureSurface::PrList),
            "Couldn't load pull requests"
        );
        assert_eq!(failure.reason(), "gh: command not found");
    }

    #[test]
    fn http_403_and_forbidden_read_as_no_access() {
        for message in ["HTTP 403: Forbidden", "gh: Forbidden"] {
            let failure = GhFailure::other(message);
            assert_eq!(failure.kind, GhFailureKind::Other, "{message}");
            assert_eq!(
                failure.title(FailureSurface::FileList),
                "No access to this repository"
            );
            assert_eq!(failure.reason(), "Check your permissions.");
            assert_eq!(failure.severity(), FailureSeverity::Danger);
        }
    }

    #[test]
    fn known_kinds_keep_one_title_and_the_spec_reason() {
        let cases = [
            (
                GhFailureKind::Offline,
                FailureSeverity::Warning,
                "Can't reach GitHub",
                "Check your network connection.",
            ),
            (
                GhFailureKind::Unauthenticated,
                FailureSeverity::Danger,
                "gh is not authenticated",
                "run: gh auth login",
            ),
            (
                GhFailureKind::CliMissing,
                FailureSeverity::Danger,
                "gh is not installed",
                "Install GitHub CLI (https://cli.github.com), then retry.",
            ),
            (
                GhFailureKind::RateLimited,
                FailureSeverity::Warning,
                "GitHub API rate limit exceeded",
                "Try again later.",
            ),
        ];
        for (kind, severity, title, reason) in cases {
            let failure = shown(kind, "ignored because the kind is known");
            assert_eq!(failure.severity(), severity, "{kind:?}");
            assert_eq!(failure.title(FailureSurface::PrList), title, "{kind:?}");
            assert_eq!(failure.title(FailureSurface::Inbox), title, "{kind:?}");
            assert_eq!(failure.reason(), reason, "{kind:?}");
            assert!(
                !failure.reason().to_ascii_lowercase().contains("minute"),
                "rate-limit reset is not available"
            );
        }
    }

    #[test]
    fn other_titles_follow_the_surface_and_blank_messages_have_no_reason() {
        let failure = GhFailure::other("  \n  ");
        assert_eq!(failure.reason(), "");
        assert_eq!(failure.severity(), FailureSeverity::Danger);
        assert_eq!(
            failure.title(FailureSurface::PrList),
            "Couldn't load pull requests"
        );
        assert_eq!(
            failure.title(FailureSurface::IssueList),
            "Couldn't load issues"
        );
        assert_eq!(
            failure.title(FailureSurface::FileList),
            "Couldn't load files"
        );
        assert_eq!(
            failure.title(FailureSurface::Comments),
            "Couldn't load comments"
        );
        assert_eq!(
            failure.title(FailureSurface::CockpitCounts),
            "Couldn't load counts"
        );
        assert_eq!(
            failure.title(FailureSurface::Inbox),
            "Couldn't load the inbox"
        );
    }

    #[test]
    fn other_reason_joins_lines_with_spaces() {
        let failure = GhFailure::other("first line\nsecond line\r\nthird");
        assert_eq!(failure.reason(), "first line second line third");
    }
}
