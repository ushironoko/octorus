//! Batched `markFileAsViewed` / `unmarkFileAsViewed` mutations.
//!
//! A directory mark used to spawn one `gh` process per file and stop at the
//! first error. Files are now sent together as aliased mutations. A single
//! file still uses that original mutation, so the `v` key talks to `gh` the
//! same way it always has.
//!
//! When GitHub accepts some aliases and rejects others, every accepted path is
//! kept, including ones after the first error. A mark never clears a file:
//! an alias for a path that was already viewed is a no-op on the server, and
//! undoing it would unmark a file this process did not view. Later chunks are
//! not sent after a failure. The error text is the same `gh` failure the
//! per-file loop shows, so stderr and the response body stay on the toast.

use anyhow::{Context, Result};
use serde_json::Value;

use super::{classify_gh_failure, set_file_viewed, GhFailureKind};

/// Aliases per GraphQL call. This bounds the query, not how many files can be
/// marked: the rest go out in the next call, in the same order.
const VIEWED_MUTATION_BATCH: usize = 40;

/// Paths GitHub accepted, in request order, and the first failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesViewedUpdate {
    pub marked_paths: Vec<String>,
    pub error: Option<String>,
}

/// Mark or unmark `paths` on one pull request.
///
/// `marked_paths` is every path whose mutation succeeded, in request order.
/// That includes aliases after the first error: they were requested and GitHub
/// applied them. The error is that first failure, in the per-file `gh` text.
pub(crate) async fn set_files_viewed(
    repo: &str,
    pr_node_id: &str,
    paths: &[String],
    set_viewed: bool,
) -> FilesViewedUpdate {
    if paths.is_empty() {
        return FilesViewedUpdate {
            marked_paths: Vec::new(),
            error: None,
        };
    }
    // One file is the keypress path. Keep its `gh` invocation identical.
    if paths.len() == 1 {
        return mark_sequential(repo, pr_node_id, paths, set_viewed).await;
    }

    let mut marked_paths = Vec::with_capacity(paths.len());
    for chunk in paths.chunks(VIEWED_MUTATION_BATCH) {
        match request_chunk(pr_node_id, chunk, set_viewed).await {
            ChunkReport::AppliedAll => marked_paths.extend(chunk.iter().cloned()),
            ChunkReport::FailedClosed { message } => {
                let Some(path) = chunk.first() else {
                    continue;
                };
                return FilesViewedUpdate {
                    marked_paths,
                    error: Some(format!("{path}: {message}")),
                };
            }
            ChunkReport::RetryPerFile => {
                let sequential = mark_sequential(repo, pr_node_id, chunk, set_viewed).await;
                marked_paths.extend(sequential.marked_paths);
                if sequential.error.is_some() {
                    return FilesViewedUpdate {
                        marked_paths,
                        error: sequential.error,
                    };
                }
            }
            ChunkReport::Partial {
                before,
                failed_path,
                failed_message,
                applied_after,
            } => {
                // Successes after the error were applied. Leave them, including
                // a path that was already viewed: the mark did not change it,
                // and a later unmark would.
                marked_paths.extend(before);
                marked_paths.extend(applied_after);
                return FilesViewedUpdate {
                    marked_paths,
                    error: Some(format!("{failed_path}: {failed_message}")),
                };
            }
        }
    }

    FilesViewedUpdate {
        marked_paths,
        error: None,
    }
}

/// The per-file loop this batch replaces. Fallback uses it so a refused batch
/// still marks every file the old loop would have marked, and stops at the
/// same file.
async fn mark_sequential(
    repo: &str,
    pr_node_id: &str,
    paths: &[String],
    set_viewed: bool,
) -> FilesViewedUpdate {
    let mut marked_paths = Vec::with_capacity(paths.len());
    for path in paths {
        match set_file_viewed(repo, pr_node_id, path, set_viewed).await {
            Ok(()) => marked_paths.push(path.clone()),
            Err(error) => {
                return FilesViewedUpdate {
                    marked_paths,
                    error: Some(format!("{path}: {error}")),
                };
            }
        }
    }
    FilesViewedUpdate {
        marked_paths,
        error: None,
    }
}

enum ChunkReport {
    AppliedAll,
    /// The call never produced per-file results, and retrying cannot succeed.
    FailedClosed {
        message: String,
    },
    /// The call was refused before any alias ran. Per-file calls are safe.
    RetryPerFile,
    Partial {
        before: Vec<String>,
        failed_path: String,
        failed_message: String,
        applied_after: Vec<String>,
    },
}

async fn request_chunk(pr_node_id: &str, paths: &[String], set_viewed: bool) -> ChunkReport {
    let query = viewed_mutation_query(set_viewed, paths.len());
    let args = graphql_args(&query, pr_node_id, paths);
    let run = match run_gh(&args).await {
        Ok(run) => run,
        Err(error) => {
            return if kind_stops(&error) {
                ChunkReport::FailedClosed {
                    message: error.to_string(),
                }
            } else {
                ChunkReport::RetryPerFile
            };
        }
    };

    let trimmed = run.stdout.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if value.is_object() {
            let mut report = close_terminal(interpret(&value, paths));
            if let ChunkReport::Partial { failed_message, .. } = &mut report {
                // The per-file loop surfaces stderr and the body through
                // `gh command failed: …`. A shorter parsed message drops both.
                *failed_message = gh_failure_text(run.stderr.trim(), trimmed);
            }
            return report;
        }
    }

    let message = gh_failure_text(run.stderr.trim(), trimmed);
    if kind_stops_message(&message) {
        ChunkReport::FailedClosed { message }
    } else {
        ChunkReport::RetryPerFile
    }
}

/// Request-level JSON still carries text that might be an offline failure.
/// Callers see [`ChunkReport::RetryPerFile`] once that has been ruled out.
#[derive(Debug)]
enum ChunkBuild {
    AppliedAll,
    RetryPerFileMessage {
        message: String,
    },
    Partial {
        before: Vec<String>,
        failed_path: String,
        failed_message: String,
        applied_after: Vec<String>,
    },
}

fn close_terminal(report: ChunkBuild) -> ChunkReport {
    match report {
        ChunkBuild::AppliedAll => ChunkReport::AppliedAll,
        ChunkBuild::RetryPerFileMessage { message } if kind_stops_message(&message) => {
            ChunkReport::FailedClosed { message }
        }
        ChunkBuild::RetryPerFileMessage { .. } => ChunkReport::RetryPerFile,
        ChunkBuild::Partial {
            before,
            failed_path,
            failed_message,
            applied_after,
        } => ChunkReport::Partial {
            before,
            failed_path,
            failed_message,
            applied_after,
        },
    }
}

fn interpret(value: &Value, paths: &[String]) -> ChunkBuild {
    let mut alias_error: Vec<Option<String>> = vec![None; paths.len()];
    let mut request_messages = Vec::new();
    if let Some(errors) = value.get("errors").and_then(|errors| errors.as_array()) {
        for error in errors {
            let message = error
                .get("message")
                .and_then(|message| message.as_str())
                .unwrap_or("GitHub GraphQL error")
                .to_string();
            match error
                .get("path")
                .and_then(|path| path.as_array())
                .and_then(|path| path.first())
                .and_then(|path| path.as_str())
                .and_then(alias_index)
            {
                Some(index) if index < paths.len() => {
                    if alias_error[index].is_none() {
                        alias_error[index] = Some(message);
                    }
                }
                _ => request_messages.push(message),
            }
        }
    }

    let data = value.get("data").and_then(|data| data.as_object());
    let mut saw_alias = false;
    let mut before = Vec::new();
    let mut applied_after = Vec::new();
    let mut failure: Option<(String, String)> = None;

    for (index, path) in paths.iter().enumerate() {
        let alias = format!("m{index}");
        let field_error = alias_error[index].clone();
        let field = data.and_then(|map| map.get(&alias));
        if field_error.is_some() || field.is_some() {
            saw_alias = true;
        }
        if let Some(message) = field_error {
            if failure.is_none() {
                failure = Some((path.clone(), message));
            }
            continue;
        }
        if matches!(field, Some(Value::Object(_))) {
            if failure.is_none() {
                before.push(path.clone());
            } else {
                applied_after.push(path.clone());
            }
            continue;
        }
        if field.is_some() || data.is_some() {
            saw_alias = true;
            let message = request_messages
                .first()
                .cloned()
                .unwrap_or_else(|| "GitHub GraphQL returned no result for this file".to_string());
            if failure.is_none() {
                failure = Some((path.clone(), message));
            }
        }
    }

    if !saw_alias {
        let message = request_messages
            .into_iter()
            .next()
            .unwrap_or_else(|| "GitHub GraphQL rejected the viewed-marking batch".to_string());
        return ChunkBuild::RetryPerFileMessage { message };
    }

    match failure {
        None => ChunkBuild::AppliedAll,
        Some((failed_path, failed_message)) => ChunkBuild::Partial {
            before,
            failed_path,
            failed_message,
            applied_after,
        },
    }
}

fn alias_index(name: &str) -> Option<usize> {
    name.strip_prefix('m')?.parse().ok()
}

fn kind_stops(error: &anyhow::Error) -> bool {
    matches!(
        classify_gh_failure(error),
        GhFailureKind::Offline | GhFailureKind::Unauthenticated | GhFailureKind::CliMissing
    )
}

fn kind_stops_message(message: &str) -> bool {
    kind_stops(&anyhow::Error::msg(message.to_string()))
}

fn viewed_mutation_query(set_viewed: bool, count: usize) -> String {
    let field = if set_viewed {
        "markFileAsViewed"
    } else {
        "unmarkFileAsViewed"
    };
    let mut variable_defs = String::from("$pullRequestId: ID!");
    let mut selections = String::new();
    for index in 0..count {
        variable_defs.push_str(&format!(", $p{index}: String!"));
        if index > 0 {
            selections.push(' ');
        }
        selections.push_str(&format!(
            "m{index}: {field}(input: {{pullRequestId: $pullRequestId, path: $p{index}}}) {{ clientMutationId }}"
        ));
    }
    format!("mutation({variable_defs}) {{ {selections} }}")
}

fn graphql_args(query: &str, pr_node_id: &str, paths: &[String]) -> Vec<String> {
    let mut args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={query}"),
        "-f".to_string(),
        format!("pullRequestId={pr_node_id}"),
    ];
    for (index, path) in paths.iter().enumerate() {
        args.push("-f".to_string());
        args.push(format!("p{index}={path}"));
    }
    args
}

struct GhRun {
    stdout: String,
    stderr: String,
}

async fn run_gh(args: &[String]) -> Result<GhRun> {
    let mut command = tokio::process::Command::new("gh");
    command.kill_on_drop(true);
    command.args(args);
    let output = command
        .output()
        .await
        .context("Failed to execute gh CLI - is it installed?")?;
    let stdout = String::from_utf8(output.stdout).context("gh output contains invalid UTF-8")?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok(GhRun { stdout, stderr })
}

/// Same shape as the `gh` error used by the single-file mutation.
fn gh_failure_text(stderr: &str, stdout: &str) -> String {
    if stdout.is_empty() {
        format!("gh command failed: {stderr}")
    } else {
        let truncated: String = stdout.chars().take(200).collect();
        let suffix = if stdout.len() > truncated.len() {
            "..."
        } else {
            ""
        };
        format!("gh command failed: {stderr} ({truncated}{suffix})")
    }
}

#[cfg(test)]
mod interpret_tests {
    use super::*;
    use serde_json::json;

    fn paths(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn success_ignores_object_key_order() {
        let value = json!({
            "data": {
                "m1": {"clientMutationId": null},
                "m0": {"clientMutationId": null}
            }
        });
        let files = paths(&["src/a.rs", "src/b.rs"]);
        assert!(matches!(interpret(&value, &files), ChunkBuild::AppliedAll));
    }

    #[test]
    fn partial_keeps_the_prefix_and_names_the_tail_that_landed() {
        let value = json!({
            "data": {
                "m2": {"clientMutationId": null},
                "m0": {"clientMutationId": null},
                "m1": null
            },
            "errors": [{"path": ["m1"], "message": "File not changed"}]
        });
        let files = paths(&["src/a.rs", "src/b.rs", "src/c.rs"]);
        match interpret(&value, &files) {
            ChunkBuild::Partial {
                before,
                failed_path,
                failed_message,
                applied_after,
            } => {
                assert_eq!(before, paths(&["src/a.rs"]));
                assert_eq!(failed_path, "src/b.rs");
                assert_eq!(failed_message, "File not changed");
                assert_eq!(applied_after, paths(&["src/c.rs"]));
            }
            other => panic!("expected a partial batch, got {other:?}"),
        }
    }

    #[test]
    fn an_error_on_a_field_is_not_treated_as_applied() {
        let value = json!({
            "data": {"m0": {"clientMutationId": null}},
            "errors": [{"path": ["m0"], "message": "File not changed"}]
        });
        let files = paths(&["src/a.rs"]);
        match interpret(&value, &files) {
            ChunkBuild::Partial {
                before,
                failed_path,
                applied_after,
                ..
            } => {
                assert!(before.is_empty());
                assert!(applied_after.is_empty());
                assert_eq!(failed_path, "src/a.rs");
            }
            other => panic!("expected the aliased error to win, got {other:?}"),
        }
    }

    #[test]
    fn a_request_level_error_is_retried_per_file() {
        let value = json!({"errors": [{"message": "Query is too complex"}]});
        let files = paths(&["src/a.rs", "src/b.rs"]);
        match interpret(&value, &files) {
            ChunkBuild::RetryPerFileMessage { message } => {
                assert_eq!(message, "Query is too complex");
            }
            other => panic!("expected a whole-batch rejection, got {other:?}"),
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) mod fake {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const SCRIPT: &str = include_str!("../../tests/fixtures/fake-gh-viewed/gh");

    const TOGGLES: [&str; 3] = [
        "FAKE_GH_VIEWED_FAIL_PATH",
        "FAKE_GH_VIEWED_REJECT_BATCH",
        "FAKE_GH_VIEWED_OFFLINE",
    ];

    /// `gh` on PATH that only answers `pr_node_id`. It remembers which paths are
    /// viewed, so a test can see a mark that was already set and must not be
    /// cleared. Dropping it restores PATH.
    pub struct ViewedFakeGh {
        _dir: tempfile::TempDir,
        log: PathBuf,
        state: PathBuf,
        old_path: Option<OsString>,
        pr_node_id: String,
    }

    impl ViewedFakeGh {
        pub fn install(pr_node_id: &str) -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let script = dir.path().join("gh");
            fs::write(&script, SCRIPT).expect("write gh double");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod gh");
            let log = dir.path().join("calls.log");
            let state = dir.path().join("viewed.txt");
            fs::write(&state, "").expect("viewed state");
            let old_path = std::env::var_os("PATH");
            let mut path = OsString::from(dir.path());
            path.push(":");
            if let Some(old) = &old_path {
                path.push(old);
            }
            std::env::set_var("PATH", &path);
            std::env::set_var("FAKE_GH_LOG", &log);
            std::env::set_var("FAKE_GH_VIEWED_STATE", &state);
            std::env::set_var("FAKE_GH_VIEWED_PR", pr_node_id);
            for name in TOGGLES {
                std::env::remove_var(name);
            }
            Self {
                _dir: dir,
                log,
                state,
                old_path,
                pr_node_id: pr_node_id.to_string(),
            }
        }

        /// Paths already viewed on GitHub before this test's calls.
        pub fn pre_viewed(&self, paths: &[&str]) {
            let mut body = String::new();
            for path in paths {
                body.push_str(path);
                body.push('\n');
            }
            fs::write(&self.state, body).expect("seed viewed state");
        }

        /// Paths viewed on the double after the calls, in sorted order.
        pub fn viewed_paths(&self) -> Vec<String> {
            let text = fs::read_to_string(&self.state).unwrap_or_default();
            let mut paths: Vec<String> = text
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            paths.sort();
            paths
        }

        pub fn fail_path(&self, path: &str) {
            std::env::set_var("FAKE_GH_VIEWED_FAIL_PATH", path);
        }

        pub fn reject_batch(&self) {
            std::env::set_var("FAKE_GH_VIEWED_REJECT_BATCH", "1");
        }

        pub fn offline(&self) {
            std::env::set_var("FAKE_GH_VIEWED_OFFLINE", "1");
        }

        /// Calls whose arguments name this test's pull request.
        pub fn scoped_calls(&self) -> Vec<String> {
            let log = fs::read_to_string(&self.log).unwrap_or_default();
            split_calls(&log)
                .into_iter()
                .filter(|call| call.contains(&self.pr_node_id))
                .collect()
        }
    }

    impl Drop for ViewedFakeGh {
        fn drop(&mut self) {
            match &self.old_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
            std::env::remove_var("FAKE_GH_LOG");
            std::env::remove_var("FAKE_GH_VIEWED_STATE");
            std::env::remove_var("FAKE_GH_VIEWED_PR");
            for name in TOGGLES {
                std::env::remove_var(name);
            }
        }
    }

    fn split_calls(log: &str) -> Vec<String> {
        let mut calls = Vec::new();
        let mut current = String::new();
        for line in log.lines() {
            if let Some(rest) = line.strip_prefix("CALL ") {
                if !current.is_empty() {
                    calls.push(std::mem::take(&mut current));
                }
                current.push_str(rest);
            } else if !current.is_empty() {
                current.push('\n');
                current.push_str(line);
            }
        }
        if !current.is_empty() {
            calls.push(current);
        }
        calls
    }
}

#[cfg(all(test, unix))]
mod gh_tests {
    use super::fake::ViewedFakeGh;
    use super::*;
    use serial_test::serial;

    fn names(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_string()).collect()
    }

    #[tokio::test]
    async fn empty_paths_do_not_call_gh() {
        let update = set_files_viewed("o/r", "PR_viewed_batch_empty_193", &[], true).await;
        assert!(update.marked_paths.is_empty());
        assert!(update.error.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn one_file_still_uses_the_single_mutation() {
        let pr = "PR_viewed_batch_one_193";
        let gh = ViewedFakeGh::install(pr);
        let paths = names(&["src/a.rs"]);

        let update = set_files_viewed("o/r", pr, &paths, true).await;

        assert_eq!(update.marked_paths, paths);
        assert!(update.error.is_none());
        let calls = gh.scoped_calls();
        assert_eq!(
            calls.len(),
            1,
            "main issued 1 gh call for one file; still 1"
        );
        assert!(calls[0].contains("path=src/a.rs"), "{}", calls[0]);
        assert!(!calls[0].contains("p0="), "{}", calls[0]);
        assert!(calls[0].contains("markFileAsViewed"), "{}", calls[0]);
    }

    #[tokio::test]
    #[serial]
    async fn unviewing_one_file_still_uses_the_single_mutation() {
        let pr = "PR_viewed_batch_unview_193";
        let gh = ViewedFakeGh::install(pr);
        let paths = names(&["src/a.rs"]);

        let update = set_files_viewed("o/r", pr, &paths, false).await;

        assert_eq!(update.marked_paths, paths);
        assert!(update.error.is_none());
        let calls = gh.scoped_calls();
        assert_eq!(calls.len(), 1, "main issued 1 gh call to unview one file");
        assert!(calls[0].contains("unmarkFileAsViewed"), "{}", calls[0]);
        assert!(calls[0].contains("path=src/a.rs"), "{}", calls[0]);
    }

    #[tokio::test]
    #[serial]
    async fn five_files_mark_in_one_call() {
        let pr = "PR_viewed_batch_five_193";
        let gh = ViewedFakeGh::install(pr);
        let paths = names(&["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs"]);

        let update = set_files_viewed("o/r", pr, &paths, true).await;

        assert_eq!(update.marked_paths, paths);
        assert!(update.error.is_none());
        let calls = gh.scoped_calls();
        assert_eq!(
            calls.len(),
            1,
            "main issued {} gh calls (one per file); batching issues {}",
            paths.len(),
            calls.len()
        );
        assert!(calls[0].contains("p4=src/e.rs"), "{}", calls[0]);
        assert!(calls[0].contains("m4:"), "{}", calls[0]);
        assert!(!calls[0].contains("unmarkFileAsViewed"), "{}", calls[0]);
    }

    #[tokio::test]
    #[serial]
    async fn a_full_batch_plus_one_file_is_two_calls_and_marks_every_file() {
        let pr = "PR_viewed_batch_chunks_193";
        let gh = ViewedFakeGh::install(pr);
        let paths: Vec<String> = (0..VIEWED_MUTATION_BATCH + 1)
            .map(|index| format!("f{index}.rs"))
            .collect();

        let update = set_files_viewed("o/r", pr, &paths, true).await;

        assert_eq!(update.marked_paths, paths);
        assert!(update.error.is_none());
        let calls = gh.scoped_calls();
        assert_eq!(
            calls.len(),
            2,
            "main issued {} gh calls; the batch size splits them into {}",
            paths.len(),
            calls.len()
        );
        let last = paths.last().expect("at least one path");
        assert!(
            calls[1].contains(&format!("p0={last}")),
            "the file past the first chunk is still sent: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_pre_viewed_file_after_a_failure_stays_viewed() {
        let pr = "PR_viewed_batch_partial_193";
        let gh = ViewedFakeGh::install(pr);
        gh.fail_path("src/b.rs");
        // Already viewed on GitHub. The mark alias succeeds as a no-op. An undo
        // would clear it; main never sends the call, so it stays viewed.
        gh.pre_viewed(&["src/c.rs"]);
        let paths = names(&["src/a.rs", "src/b.rs", "src/c.rs"]);

        let update = set_files_viewed("o/r", pr, &paths, true).await;

        assert_eq!(update.marked_paths, names(&["src/a.rs", "src/c.rs"]));
        let error = update.error.expect("failure");
        assert!(
            error.starts_with("src/b.rs: gh command failed: gh: File not changed ("),
            "{error}"
        );
        assert!(
            error.contains("\"message\":\"File not changed\""),
            "{error}"
        );
        assert!(error.ends_with(')'), "{error}");
        let calls = gh.scoped_calls();
        // main marks src/a.rs, then fails src/b.rs and stops: 2 calls. The batch
        // is one call and does not unmark src/c.rs.
        assert_eq!(calls.len(), 1, "main=2; one batch and no undo: {calls:?}");
        assert!(
            !calls.iter().any(|call| call.contains("unmarkFileAsViewed")),
            "a mark must not clear a viewed file: {calls:?}"
        );
        assert_eq!(
            gh.viewed_paths(),
            names(&["src/a.rs", "src/c.rs"]),
            "server keeps a (newly marked) and c (already viewed)"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_refused_batch_falls_back_and_marks_every_file() {
        let pr = "PR_viewed_batch_reject_ok_193";
        let gh = ViewedFakeGh::install(pr);
        gh.reject_batch();
        let paths = names(&["src/a.rs", "src/b.rs", "src/c.rs"]);

        let update = set_files_viewed("o/r", pr, &paths, true).await;

        assert_eq!(update.marked_paths, paths);
        assert!(update.error.is_none());
        let calls = gh.scoped_calls();
        assert_eq!(
            calls.len(),
            4,
            "main issued 3 gh calls; a refused batch adds one, then one per file. got {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_refused_batch_stops_at_the_first_per_file_failure() {
        let pr = "PR_viewed_batch_reject_stop_193";
        let gh = ViewedFakeGh::install(pr);
        gh.reject_batch();
        gh.fail_path("src/b.rs");
        let paths = names(&["src/a.rs", "src/b.rs", "src/c.rs"]);

        let update = set_files_viewed("o/r", pr, &paths, true).await;

        assert_eq!(update.marked_paths, names(&["src/a.rs"]));
        let error = update.error.expect("failure");
        assert!(error.starts_with("src/b.rs:"), "{error}");
        assert!(error.contains("File not changed"), "{error}");
        let calls = gh.scoped_calls();
        assert_eq!(
            calls.len(),
            3,
            "main=2 (src/a.rs, src/b.rs); refused batch plus those two. got {calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call.contains("path=src/c.rs")),
            "src/c.rs is not sent once src/b.rs fails: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn offline_does_not_fan_out() {
        let pr = "PR_viewed_batch_offline_193";
        let gh = ViewedFakeGh::install(pr);
        gh.offline();
        let paths = names(&["src/a.rs", "src/b.rs", "src/c.rs"]);

        let update = set_files_viewed("o/r", pr, &paths, true).await;

        assert!(update.marked_paths.is_empty());
        let error = update.error.expect("offline");
        assert!(error.starts_with("src/a.rs:"), "{error}");
        assert!(error.contains("dial tcp"), "{error}");
        assert_eq!(
            gh.scoped_calls().len(),
            1,
            "main makes one failing call when the first file cannot reach GitHub"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_failure_in_the_first_chunk_does_not_send_the_next_chunk() {
        let pr = "PR_viewed_batch_stop_chunk_193";
        let gh = ViewedFakeGh::install(pr);
        gh.fail_path("f0.rs");
        // f10 is in the failing chunk and already viewed. f40 is the next
        // chunk, also already viewed. Neither may be cleared.
        let already_later = format!("f{VIEWED_MUTATION_BATCH}.rs");
        gh.pre_viewed(&["f10.rs", &already_later]);
        let paths: Vec<String> = (0..VIEWED_MUTATION_BATCH + 1)
            .map(|index| format!("f{index}.rs"))
            .collect();
        let last = paths.last().expect("path").clone();

        let update = set_files_viewed("o/r", pr, &paths, true).await;

        let error = update.error.expect("failure");
        assert!(
            error.starts_with("f0.rs: gh command failed: gh: File not changed ("),
            "{error}"
        );
        let calls = gh.scoped_calls();
        // main fails the first file and stops: 1 call. The batch is that one
        // call too, and it must not follow the failure with an unmark.
        assert_eq!(
            calls.len(),
            1,
            "main=1; first-file failure is one batch and no undo: {calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call.contains("unmarkFileAsViewed")),
            "no file is unmarked: {calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call.contains(&last)),
            "{last} is past the failure and must not be sent: {calls:?}"
        );
        let expected: Vec<String> = (1..VIEWED_MUTATION_BATCH)
            .map(|index| format!("f{index}.rs"))
            .collect();
        assert_eq!(update.marked_paths, expected);
        assert!(gh.viewed_paths().contains(&"f10.rs".to_string()));
        assert!(gh.viewed_paths().contains(&last));
        assert!(!gh.viewed_paths().iter().any(|path| path == "f0.rs"));
    }
}
