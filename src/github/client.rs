use anyhow::{Context, Result};
use std::io;
use std::process::Command;
use thiserror::Error;

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

    #[test]
    fn classify_gh_failure_prefers_rate_limit_over_http_404() {
        let message = "API rate limit exceeded (HTTP 404)";
        assert_eq!(
            classify_gh_failure(&anyhow::anyhow!(message)),
            GhFailureKind::RateLimited
        );
    }
}
