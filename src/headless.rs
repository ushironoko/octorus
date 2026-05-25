use anyhow::Result;
use serde::Serialize;
use tokio::sync::mpsc;

use std::borrow::Cow;

use crate::ai::adapter::{
    CommentSeverity, Context, ReviewAction, RevieweeOutput, RevieweeStatus, ReviewerOutput,
};
use crate::ai::orchestrator::{Orchestrator, OrchestratorCommand, RallyEvent, RallyState};
use crate::ai::prompt_loader::{PromptLoader, PromptSource};
use crate::config::Config;
use crate::github;

use crate::config::SENSITIVE_AI_KEYS;
use crate::ui::common::truncate_with_width;

/// Run AI Rally in headless mode (no TUI).
///
/// Progress logs are written to stderr. On completion, a JSON summary is written to stdout.
/// This is invoked when `--ai-rally --pr <number>` is specified.
/// Returns `true` if approved, `false` otherwise.
pub async fn run_headless_rally(
    repo: &str,
    pr_number: u32,
    config: &Config,
    working_dir: Option<&str>,
    accept_local_overrides: bool,
    output_path: Option<&str>,
) -> Result<bool> {
    eprintln!("[Headless] Fetching PR #{} from {}...", pr_number, repo);

    let pr = github::fetch_pr(repo, pr_number).await?;
    let files = github::fetch_changed_files(repo, pr_number).await?;

    let mut file_patches: Vec<(String, String)> = files
        .iter()
        .filter_map(|f| f.patch.as_ref().map(|p| (f.filename.clone(), p.clone())))
        .collect();

    // Fallback: if some files are missing patches (large files), fetch full PR diff
    let has_missing_patches = files.iter().any(|f| f.patch.is_none());
    if has_missing_patches {
        eprintln!("[Headless] Some files missing patches, fetching full PR diff...");
        if let Ok(full_diff) = github::fetch_pr_diff(repo, pr_number).await {
            let parsed = crate::diff::parse_unified_diff(&full_diff);
            for (filename, patch) in &parsed {
                if !file_patches.iter().any(|(f, _)| f == filename) {
                    file_patches.push((filename.clone(), patch.clone()));
                }
            }
        }
    }

    let diff = file_patches
        .iter()
        .map(|(_, p)| p.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let context = Context {
        repo: repo.to_string(),
        pr_number,
        pr_title: pr.title.clone(),
        pr_body: pr.body.clone(),
        diff,
        working_dir: working_dir.map(|s| s.to_string()),
        head_sha: pr.head.sha.clone(),
        base_branch: pr.base.ref_name.clone(),
        external_comments: Vec::new(),
        local_mode: false,
        file_patches,
    };

    run_headless_with_context(
        repo,
        pr_number,
        config,
        context,
        accept_local_overrides,
        output_path,
    )
    .await
}

/// Run AI Rally in headless mode for local diff.
///
/// Progress logs are written to stderr. On completion, a JSON summary is written to stdout.
/// This is invoked when `--local --ai-rally` is specified.
/// Returns `true` if approved, `false` otherwise.
pub async fn run_headless_rally_local(
    repo: &str,
    config: &Config,
    working_dir: Option<&str>,
    accept_local_overrides: bool,
    output_path: Option<&str>,
) -> Result<bool> {
    eprintln!("[Headless] Running local diff rally...");

    let wd = working_dir.map(|s| s.to_string()).or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    });

    let dir = wd.as_deref().unwrap_or(".");
    let base_branch = detect_local_base_branch(Some(dir)).unwrap_or_else(|| "main".to_string());

    // Use `git diff HEAD` to capture working tree changes (staged + unstaged)
    let diff_output = tokio::process::Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(dir)
        .output()
        .await?;

    if !diff_output.status.success() {
        let stderr = String::from_utf8_lossy(&diff_output.stderr);
        anyhow::bail!(
            "git diff HEAD failed (exit {}): {}",
            diff_output.status,
            stderr.trim()
        );
    }

    let mut diff = String::from_utf8_lossy(&diff_output.stdout).to_string();

    // Include untracked files (mirrors TUI behavior in loader.rs)
    // Uses `git ls-files --others --exclude-standard` + `git diff --no-index`
    let untracked_diff = collect_untracked_diff(dir).await;
    if !untracked_diff.is_empty() {
        if !diff.is_empty() && !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str(&untracked_diff);
    }

    // Fallback: if no uncommitted changes, try committed changes vs base branch
    if diff.trim().is_empty() {
        eprintln!(
            "[Headless] No uncommitted changes, trying diff against origin/{}...",
            base_branch
        );
        let fallback_output = tokio::process::Command::new("git")
            .args(["diff", &format!("origin/{}...HEAD", base_branch)])
            .current_dir(dir)
            .output()
            .await?;

        if fallback_output.status.success() {
            diff = String::from_utf8_lossy(&fallback_output.stdout).to_string();
        } else {
            let stderr = String::from_utf8_lossy(&fallback_output.stderr);
            eprintln!("[Headless] Fallback diff also failed: {}", stderr.trim());
        }
    }

    if diff.trim().is_empty() {
        anyhow::bail!(
            "No changes detected: both git diff HEAD and git diff origin/{}...HEAD returned empty",
            base_branch
        );
    }

    let head_sha = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let context = Context {
        repo: repo.to_string(),
        pr_number: 0,
        pr_title: "Local diff".to_string(),
        pr_body: None,
        diff,
        working_dir: wd,
        head_sha,
        base_branch,
        external_comments: Vec::new(),
        local_mode: true,
        file_patches: Vec::new(),
    };

    run_headless_with_context(
        repo,
        0,
        config,
        context,
        accept_local_overrides,
        output_path,
    )
    .await
}

/// Collect sensitive local overrides from config and local prompt files.
///
/// Returns a list of override descriptions. If non-empty and `accept_local_overrides`
/// is false, the caller should refuse to proceed.
fn collect_sensitive_overrides(config: &Config) -> Vec<Cow<'static, str>> {
    let mut sensitive_overrides: Vec<Cow<'static, str>> = SENSITIVE_AI_KEYS
        .iter()
        .filter(|key| config.local_overrides.contains(**key))
        .map(|s| Cow::Borrowed(*s))
        .collect();

    let prompt_loader = PromptLoader::new(&config.ai, &config.project_root);
    for (filename, source) in prompt_loader.resolve_all_sources() {
        if let PromptSource::Local(path) = source {
            sensitive_overrides.push(Cow::Owned(format!(
                "local prompt: {} ({})",
                filename,
                path.display()
            )));
        }
    }

    sensitive_overrides
}

/// Core headless execution logic shared between PR and local modes.
async fn run_headless_with_context(
    repo: &str,
    pr_number: u32,
    config: &Config,
    context: Context,
    accept_local_overrides: bool,
    output_path: Option<&str>,
) -> Result<bool> {
    // Check for sensitive local config overrides
    let sensitive_overrides = collect_sensitive_overrides(config);
    let prompt_loader = PromptLoader::new(&config.ai, &config.project_root);

    if !sensitive_overrides.is_empty() && !accept_local_overrides {
        eprintln!(
            "[Headless] WARNING: Local .octorus/ overrides detected that affect AI behavior:"
        );
        for key in &sensitive_overrides {
            eprintln!("  - {}", key);
        }
        eprintln!("[Headless] Use --accept-local-overrides to explicitly allow these overrides.");
        anyhow::bail!(
            "Refusing to run AI Rally with local overrides: {}. \
             Use --accept-local-overrides to bypass this check.",
            sensitive_overrides
                .iter()
                .map(|s| s.as_ref())
                .collect::<Vec<&str>>()
                .join(", ")
        );
    }

    let (event_tx, mut event_rx) = mpsc::channel(100);
    let (cmd_tx, cmd_rx) = mpsc::channel(10);

    let local_mode = context.local_mode;
    let auto_post = config.ai.auto_post;

    let mut orchestrator = Orchestrator::new(
        repo,
        pr_number,
        config.ai.clone(),
        event_tx,
        Some(cmd_rx),
        prompt_loader,
    )?;
    orchestrator.set_context(context);

    // Spawn orchestrator in background
    let orchestrator_handle = tokio::spawn(async move { orchestrator.run().await });

    // Event loop: receive events and auto-respond to interactive requests
    let outcome = run_headless_event_loop(&mut event_rx, &cmd_tx, local_mode, auto_post).await;

    // Wait for orchestrator to finish
    let _ = orchestrator_handle.await;

    // Existing stderr output (unchanged)
    match &outcome.result {
        HeadlessResult::Approved(_) => {
            eprintln!("\n[Headless] Rally completed: Approved");
        }
        HeadlessResult::NotApproved(reason) => {
            eprintln!("\n[Headless] Rally completed: {}", reason);
        }
        HeadlessResult::Error(msg) => {
            eprintln!("\n[Headless] Rally error: {}", msg);
        }
    }

    // JSON output to stdout and optionally to file
    let json_output = build_json_output(&outcome);
    write_json_stdout(&json_output);
    if let Some(path) = output_path {
        write_json_file(&json_output, path);
    }

    Ok(matches!(outcome.result, HeadlessResult::Approved(_)))
}

enum HeadlessResult {
    Approved(String),
    NotApproved(String),
    Error(String),
}

/// Internal outcome from the headless event loop, bundling result with collected data.
struct HeadlessOutcome {
    result: HeadlessResult,
    iterations: u32,
    last_review: Option<ReviewerOutput>,
    last_fix: Option<RevieweeOutput>,
}

/// Result kind for JSON output (serialized as snake_case).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum HeadlessResultKind {
    Approved,
    NotApproved,
    Error,
}

/// JSON output structure written to stdout on headless completion.
#[derive(Debug, Serialize)]
struct HeadlessJsonOutput {
    result: HeadlessResultKind,
    iterations: u32,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_review: Option<ReviewerOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_fix: Option<RevieweeOutput>,
}

/// Event loop that processes RallyEvents and auto-responds to interactive requests.
///
/// Headless decision for any `*PostConfirmNeeded` event. Returns
/// `true` (approve, will be posted) only when running against a real PR
/// (`!local_mode`) **and** the user opted in via `ai.auto_post = true`.
/// Otherwise the post is skipped — the proposal/review/fix is still
/// printed to stderr by the caller, matching local-mode behavior and
/// the TUI's confirm-dialog semantics.
fn headless_post_decision(local_mode: bool, auto_post: bool) -> bool {
    if local_mode {
        eprintln!("  -> Skipping (local mode, no PR to post to)");
        return false;
    }
    if !auto_post {
        eprintln!("  -> Skipping post (ai.auto_post=false in headless mode)");
        return false;
    }
    eprintln!("  -> Auto-approving post (ai.auto_post=true, headless mode)");
    true
}

/// Headless policy:
/// - Clarification: auto-skip (continue with best judgment)
/// - Permission: auto-deny (prevents dynamic tool expansion without human review)
/// - PostConfirmation: respect `auto_post`. If `auto_post=false`, skip the
///   PR post (PostConfirmResponse(false)) — same behavior as `local_mode`,
///   for parity with the TUI confirm-dialog semantics. Only `auto_post=true`
///   in non-local mode actually pushes to the PR.
/// - AgentText: suppressed (prevents structured output JSON leakage)
/// - AgentThinking: suppressed (noise reduction)
async fn run_headless_event_loop(
    event_rx: &mut mpsc::Receiver<RallyEvent>,
    cmd_tx: &mpsc::Sender<OrchestratorCommand>,
    local_mode: bool,
    auto_post: bool,
) -> HeadlessOutcome {
    let mut last_error: Option<String> = None;
    let mut current_iteration: u32 = 0;
    let mut last_review: Option<ReviewerOutput> = None;
    let mut last_fix: Option<RevieweeOutput> = None;

    while let Some(event) = event_rx.recv().await {
        match event {
            RallyEvent::RallyStarted { review_only } => {
                if review_only {
                    eprintln!(
                        "\n[Mode] Review Only — proposal iteration mode (reviewee designs fix plans, no code changes)"
                    );
                } else {
                    eprintln!("\n[Mode] Standard (review + fix)");
                }
            }
            RallyEvent::IterationStarted(n) => {
                current_iteration = n;
                eprintln!("\n=== Iteration {} ===", n);
            }
            RallyEvent::StateChanged(state) => match state {
                RallyState::ReviewerReviewing => {
                    eprintln!("[Reviewer] Reviewing...");
                }
                RallyState::RevieweeFix => {
                    eprintln!("[Reviewee] Fixing...");
                }
                RallyState::RevieweeProposing => {
                    eprintln!("[Reviewee] Designing fix proposal...");
                }
                RallyState::Completed => {
                    // Will be handled by Approved event
                }
                RallyState::Aborted => {
                    return HeadlessOutcome {
                        result: HeadlessResult::NotApproved(
                            last_error.unwrap_or_else(|| "Aborted".to_string()),
                        ),
                        iterations: current_iteration,
                        last_review,
                        last_fix,
                    };
                }
                RallyState::Error => {
                    return HeadlessOutcome {
                        result: HeadlessResult::Error(
                            last_error.unwrap_or_else(|| "Unknown error".to_string()),
                        ),
                        iterations: current_iteration,
                        last_review,
                        last_fix,
                    };
                }
                _ => {}
            },
            RallyEvent::ReviewCompleted(output) => {
                eprintln!("{}", format_review_output(&output));
                last_review = Some(output);
            }
            RallyEvent::FixCompleted(output) => {
                eprintln!("{}", format_fix_output(&output));
                last_fix = Some(output);
            }
            RallyEvent::Approved(summary) => {
                eprintln!("\n[Approved] {}", summary);
                return HeadlessOutcome {
                    result: HeadlessResult::Approved(summary),
                    iterations: current_iteration,
                    last_review,
                    last_fix,
                };
            }
            RallyEvent::ReviewOnlyCompleted(output) => {
                let action = format!("{:?}", output.action);
                eprintln!(
                    "\n[Review-only completed] Terminated without approval after {} iteration(s). final_action={} summary={}",
                    current_iteration, action, output.summary
                );
                let summary = output.summary.clone();
                last_review = Some(output);
                return HeadlessOutcome {
                    result: HeadlessResult::NotApproved(format!(
                        "Review-only mode: terminated without approval after {} iterations (final action={}; {})",
                        current_iteration, action, summary
                    )),
                    iterations: current_iteration,
                    last_review,
                    last_fix,
                };
            }
            RallyEvent::Error(msg) => {
                eprintln!("\n[Error] {}", msg);
                last_error = Some(msg);
            }
            RallyEvent::Log(msg) => {
                eprintln!("  {}", msg);
            }
            RallyEvent::AgentToolUse(name, _input) => {
                eprintln!("  > {}", name);
            }
            RallyEvent::AgentToolResult(name, result) => {
                let truncated = truncate_with_width(&result, 200);
                eprintln!("  < {}: {}", name, truncated);
            }
            // Suppress AgentThinking, AgentText, and pause events (headless can't pause)
            RallyEvent::AgentThinking(_)
            | RallyEvent::AgentText(_)
            | RallyEvent::Paused
            | RallyEvent::Resumed => {}
            // Auto-skip clarification (headless can't interact)
            RallyEvent::ClarificationNeeded(question) => {
                eprintln!("  [Clarification needed] {}", question);
                eprintln!("  -> Auto-skipping (headless mode)");
                let _ = cmd_tx.send(OrchestratorCommand::SkipClarification).await;
            }
            // Deny permission by default in headless mode.
            // Auto-approving would allow dynamic tool expansion via prompt injection,
            // bypassing the allowedTools constraint. Deny-by-default is the safe choice
            // for non-interactive CI runs.
            RallyEvent::PermissionNeeded(action, reason) => {
                eprintln!("  [Permission needed] {}: {}", action, reason);
                eprintln!("  -> Auto-denying (headless mode, no human to confirm)");
                let _ = cmd_tx
                    .send(OrchestratorCommand::PermissionResponse(false))
                    .await;
            }
            // Post confirmation handling:
            // - local_mode: auto-deny (no PR to post to)
            // - otherwise: auto-approve (headless can't interact)
            RallyEvent::ReviewPostConfirmNeeded(info) => {
                eprintln!(
                    "  [Post review] {}: {} ({} comments)",
                    info.action, info.summary, info.comment_count
                );
                let approved = headless_post_decision(local_mode, auto_post);
                let _ = cmd_tx
                    .send(OrchestratorCommand::PostConfirmResponse(approved))
                    .await;
            }
            RallyEvent::FixPostConfirmNeeded(info) => {
                eprintln!(
                    "  [Post fix] {} (files: {})",
                    info.summary,
                    info.files_modified.join(", ")
                );
                let approved = headless_post_decision(local_mode, auto_post);
                let _ = cmd_tx
                    .send(OrchestratorCommand::PostConfirmResponse(approved))
                    .await;
            }
            RallyEvent::ProposalCompleted(p) => {
                eprintln!(
                    "[Proposal][iter {}] {} ({} plan item(s), files: {})",
                    current_iteration,
                    p.summary,
                    p.plan.len(),
                    p.target_files().join(", ")
                );
            }
            RallyEvent::ProposalPostConfirmNeeded(info) => {
                eprintln!(
                    "  [Post proposal] {} ({} plan item(s), files: {})",
                    info.summary,
                    info.plan_item_count,
                    info.target_files.join(", ")
                );
                let approved = headless_post_decision(local_mode, auto_post);
                let _ = cmd_tx
                    .send(OrchestratorCommand::PostConfirmResponse(approved))
                    .await;
            }
        }
    }

    // Channel closed - orchestrator finished without explicit terminal event
    HeadlessOutcome {
        result: HeadlessResult::NotApproved(
            last_error.unwrap_or_else(|| "Rally ended unexpectedly".to_string()),
        ),
        iterations: current_iteration,
        last_review,
        last_fix,
    }
}

/// Format ReviewerOutput as human-readable text (no JSON).
pub fn format_review_output(output: &ReviewerOutput) -> String {
    let mut lines = Vec::new();

    let action_str = match output.action {
        ReviewAction::Approve => "approve",
        ReviewAction::RequestChanges => "request_changes",
        ReviewAction::Comment => "comment",
    };
    lines.push(format!("[Review] Action: {}", action_str));
    lines.push(format!("  Summary: {}", output.summary));

    if !output.comments.is_empty() {
        lines.push(format!("  Comments ({}):", output.comments.len()));
        for comment in &output.comments {
            let severity = match comment.severity {
                CommentSeverity::Critical => "critical",
                CommentSeverity::Major => "major",
                CommentSeverity::Minor => "minor",
                CommentSeverity::Suggestion => "suggestion",
            };
            let location = format!("{}:{}", comment.path, comment.line);
            lines.push(format!(
                "    - {} [{}] {}",
                location, severity, comment.body
            ));
        }
    }

    if !output.blocking_issues.is_empty() {
        lines.push("  Blocking issues:".to_string());
        for issue in &output.blocking_issues {
            lines.push(format!("    - {}", issue));
        }
    }

    lines.join("\n")
}

/// Format RevieweeOutput as human-readable text (no JSON).
pub fn format_fix_output(output: &RevieweeOutput) -> String {
    let mut lines = Vec::new();

    let status_str = match output.status {
        RevieweeStatus::Completed => "completed",
        RevieweeStatus::NeedsClarification => "needs_clarification",
        RevieweeStatus::NeedsPermission => "needs_permission",
        RevieweeStatus::Error => "error",
    };
    lines.push(format!("[Fix] Status: {}", status_str));
    lines.push(format!("  Summary: {}", output.summary));

    if !output.files_modified.is_empty() {
        lines.push("  Files modified:".to_string());
        for file in &output.files_modified {
            lines.push(format!("    - {}", file));
        }
    }

    if let Some(error) = &output.error_details {
        lines.push(format!("  Error: {}", error));
    }

    lines.join("\n")
}

/// Build JSON output from headless outcome (pure function for testability).
fn build_json_output(outcome: &HeadlessOutcome) -> HeadlessJsonOutput {
    let (result_kind, summary) = match &outcome.result {
        HeadlessResult::Approved(summary) => (HeadlessResultKind::Approved, summary.clone()),
        HeadlessResult::NotApproved(reason) => (HeadlessResultKind::NotApproved, reason.clone()),
        HeadlessResult::Error(msg) => (HeadlessResultKind::Error, msg.clone()),
    };
    HeadlessJsonOutput {
        result: result_kind,
        iterations: outcome.iterations,
        summary,
        last_review: outcome.last_review.clone(),
        last_fix: outcome.last_fix.clone(),
    }
}

/// Write JSON output to stdout with flush guarantee and broken pipe safety.
fn write_json_stdout(output: &HeadlessJsonOutput) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match serde_json::to_writer(&mut handle, output) {
        Ok(()) => {
            let _ = writeln!(handle);
            let _ = handle.flush();
        }
        Err(e) => {
            eprintln!("[Headless] JSON serialization failed: {}", e);
        }
    }
}

/// Write JSON output to a file (atomic write via temp file + rename).
///
/// Note: Relative paths are resolved from the process's current working directory,
/// not from `--working-dir`.
fn write_json_file(output: &HeadlessJsonOutput, path: &str) {
    use std::io::Write;
    let target = std::path::Path::new(path);

    // Ensure parent directory exists
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("[Headless] Failed to create output directory: {}", e);
                return;
            }
        }
    }

    // Atomic write: write to temp file with PID-based unique name, then rename
    let file_name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp_path = target.with_file_name(format!(".{}.{}.tmp", file_name, std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp_path)?;
        serde_json::to_writer_pretty(&mut file, output).map_err(std::io::Error::other)?;
        writeln!(file)?;
        file.flush()?;
        std::fs::rename(&temp_path, target)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            eprintln!("[Headless] JSON result written to: {}", path);
        }
        Err(e) => {
            eprintln!("[Headless] Failed to write output file: {}", e);
            let _ = std::fs::remove_file(&temp_path);
        }
    }
}

/// Write error JSON to stdout (and optionally to file) for early failures.
pub fn write_error_json(error: &str, output_path: Option<&str>) {
    let output = HeadlessJsonOutput {
        result: HeadlessResultKind::Error,
        iterations: 0,
        summary: error.to_string(),
        last_review: None,
        last_fix: None,
    };
    write_json_stdout(&output);
    if let Some(path) = output_path {
        write_json_file(&output, path);
    }
}

/// Collect diff output for untracked files (mirrors loader.rs behavior).
///
/// Uses `git ls-files --others --exclude-standard` to discover untracked files,
/// then `git diff --no-index -- /dev/null <file>` to generate unified diffs.
async fn collect_untracked_diff(dir: &str) -> String {
    // List untracked files (respecting .gitignore)
    let ls_output = tokio::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(dir)
        .output()
        .await;

    let untracked_files: Vec<String> = match ls_output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        _ => return String::new(),
    };

    if untracked_files.is_empty() {
        return String::new();
    }

    eprintln!(
        "[Headless] Including {} untracked file(s) in diff",
        untracked_files.len()
    );

    let mut parts = Vec::new();
    for filename in &untracked_files {
        let diff_output = tokio::process::Command::new("git")
            .args([
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--no-index",
                "--",
                "/dev/null",
                filename,
            ])
            .current_dir(dir)
            .output()
            .await;

        if let Ok(output) = diff_output {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if !stdout.trim().is_empty() {
                parts.push(stdout);
            }
        }
    }

    parts.join("\n")
}

/// Detect base branch for local diff (same logic as app.rs).
fn detect_local_base_branch(working_dir: Option<&str>) -> Option<String> {
    let dir = working_dir.unwrap_or(".");
    let output = std::process::Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;

    if output.status.success() {
        let ref_str = String::from_utf8_lossy(&output.stdout);
        ref_str
            .trim()
            .strip_prefix("refs/remotes/origin/")
            .map(|s| s.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::adapter::{CommentSeverity, ReviewAction, ReviewComment, RevieweeStatus};

    #[test]
    fn test_format_review_output_approve() {
        let output = ReviewerOutput {
            action: ReviewAction::Approve,
            summary: "All looks good".to_string(),
            comments: vec![],
            blocking_issues: vec![],
        };

        let text = format_review_output(&output);
        assert!(text.contains("[Review] Action: approve"));
        assert!(text.contains("Summary: All looks good"));
        assert!(!text.contains("{"));
        assert!(!text.contains("}"));
    }

    #[test]
    fn test_format_review_output_request_changes() {
        let output = ReviewerOutput {
            action: ReviewAction::RequestChanges,
            summary: "Found 2 issues".to_string(),
            comments: vec![
                ReviewComment {
                    path: "src/main.rs".to_string(),
                    line: 42,
                    body: "Variable should be constant".to_string(),
                    severity: CommentSeverity::Major,
                },
                ReviewComment {
                    path: "src/lib.rs".to_string(),
                    line: 10,
                    body: "Consider renaming".to_string(),
                    severity: CommentSeverity::Minor,
                },
            ],
            blocking_issues: vec!["Error handling missing".to_string()],
        };

        let text = format_review_output(&output);
        assert!(text.contains("[Review] Action: request_changes"));
        assert!(text.contains("Comments (2):"));
        assert!(text.contains("src/main.rs:42 [major]"));
        assert!(text.contains("src/lib.rs:10 [minor]"));
        assert!(text.contains("Blocking issues:"));
        assert!(text.contains("Error handling missing"));
        // No JSON artifacts
        assert!(!text.contains("\"action\""));
        assert!(!text.contains("\"summary\""));
    }

    #[test]
    fn test_format_review_output_suggestion_severity() {
        let output = ReviewerOutput {
            action: ReviewAction::Comment,
            summary: "General feedback".to_string(),
            comments: vec![ReviewComment {
                path: "README.md".to_string(),
                line: 1,
                body: "Update docs".to_string(),
                severity: CommentSeverity::Suggestion,
            }],
            blocking_issues: vec![],
        };

        let text = format_review_output(&output);
        assert!(text.contains("README.md:1 [suggestion] Update docs"));
    }

    #[test]
    fn test_format_fix_output_completed() {
        let output = RevieweeOutput {
            status: RevieweeStatus::Completed,
            summary: "Fixed all issues".to_string(),
            files_modified: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            question: None,
            permission_request: None,
            error_details: None,
        };

        let text = format_fix_output(&output);
        assert!(text.contains("[Fix] Status: completed"));
        assert!(text.contains("Summary: Fixed all issues"));
        assert!(text.contains("Files modified:"));
        assert!(text.contains("- src/main.rs"));
        assert!(text.contains("- src/lib.rs"));
        // No JSON artifacts
        assert!(!text.contains("\"status\""));
        assert!(!text.contains("\"files_modified\""));
    }

    #[test]
    fn test_format_fix_output_error() {
        let output = RevieweeOutput {
            status: RevieweeStatus::Error,
            summary: "Build failed".to_string(),
            files_modified: vec![],
            question: None,
            permission_request: None,
            error_details: Some("cargo build exited with code 1".to_string()),
        };

        let text = format_fix_output(&output);
        assert!(text.contains("[Fix] Status: error"));
        assert!(text.contains("Error: cargo build exited with code 1"));
    }

    #[test]
    fn test_truncate_with_width_short() {
        assert_eq!(truncate_with_width("hello", 10).as_ref(), "hello");
    }

    #[test]
    fn test_truncate_with_width_long() {
        let long = "a".repeat(300);
        let result = truncate_with_width(&long, 200);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_truncate_with_width_multibyte() {
        // Each Japanese char is 2 display-columns wide
        let s = "あいうえおかきくけこ"; // 10 chars, 20 display columns
        let result = truncate_with_width(s, 5);
        // max_width=5: budget 4 for chars + 1 for "…", "あい" = 4 cols
        assert_eq!(result.as_ref(), "あい…");
    }

    #[test]
    fn test_truncate_with_width_multibyte_mixed() {
        // "aあbいc" = 1+2+1+2+1 = 7 display columns
        let s = "aあbいc";
        // max_width=4: budget 3, "aあ" = 3 cols → "aあ…"
        assert_eq!(truncate_with_width(s, 4).as_ref(), "aあ…");
        assert_eq!(truncate_with_width(s, 7).as_ref(), "aあbいc");
        assert_eq!(truncate_with_width(s, 10).as_ref(), "aあbいc");
    }

    #[test]
    fn test_truncate_with_width_exact_boundary() {
        let s = "abcde";
        assert_eq!(truncate_with_width(s, 5).as_ref(), "abcde");
        assert_eq!(truncate_with_width(s, 4).as_ref(), "abc…");
    }

    #[test]
    fn test_truncate_with_width_empty() {
        assert_eq!(truncate_with_width("", 10).as_ref(), "");
        assert_eq!(truncate_with_width("", 0).as_ref(), "");
    }

    #[test]
    fn test_truncate_with_width_max_one() {
        assert_eq!(truncate_with_width("hello", 1).as_ref(), "…");
    }

    #[test]
    fn test_json_output_approved() {
        let outcome = HeadlessOutcome {
            result: HeadlessResult::Approved("All good, no issues found".to_string()),
            iterations: 2,
            last_review: Some(ReviewerOutput {
                action: ReviewAction::Approve,
                summary: "All good".to_string(),
                comments: vec![],
                blocking_issues: vec![],
            }),
            last_fix: Some(RevieweeOutput {
                status: RevieweeStatus::Completed,
                summary: "Fixed".to_string(),
                files_modified: vec!["src/main.rs".to_string()],
                question: None,
                permission_request: None,
                error_details: None,
            }),
        };
        insta::assert_json_snapshot!(build_json_output(&outcome), @r#"
        {
          "result": "approved",
          "iterations": 2,
          "summary": "All good, no issues found",
          "last_review": {
            "action": "approve",
            "summary": "All good",
            "comments": [],
            "blocking_issues": []
          },
          "last_fix": {
            "status": "completed",
            "summary": "Fixed",
            "files_modified": [
              "src/main.rs"
            ]
          }
        }
        "#);
    }

    #[test]
    fn test_json_output_not_approved() {
        let outcome = HeadlessOutcome {
            result: HeadlessResult::NotApproved("Max iterations reached".to_string()),
            iterations: 3,
            last_review: Some(ReviewerOutput {
                action: ReviewAction::RequestChanges,
                summary: "Still has issues".to_string(),
                comments: vec![ReviewComment {
                    path: "src/lib.rs".to_string(),
                    line: 10,
                    body: "Fix this".to_string(),
                    severity: CommentSeverity::Major,
                }],
                blocking_issues: vec!["Error handling".to_string()],
            }),
            last_fix: Some(RevieweeOutput {
                status: RevieweeStatus::Completed,
                summary: "Attempted fix".to_string(),
                files_modified: vec!["src/lib.rs".to_string()],
                question: None,
                permission_request: None,
                error_details: None,
            }),
        };
        insta::assert_json_snapshot!(build_json_output(&outcome), @r#"
        {
          "result": "not_approved",
          "iterations": 3,
          "summary": "Max iterations reached",
          "last_review": {
            "action": "request_changes",
            "summary": "Still has issues",
            "comments": [
              {
                "path": "src/lib.rs",
                "line": 10,
                "body": "Fix this",
                "severity": "major"
              }
            ],
            "blocking_issues": [
              "Error handling"
            ]
          },
          "last_fix": {
            "status": "completed",
            "summary": "Attempted fix",
            "files_modified": [
              "src/lib.rs"
            ]
          }
        }
        "#);
    }

    #[test]
    fn test_json_output_error_no_review() {
        let outcome = HeadlessOutcome {
            result: HeadlessResult::Error("Agent crashed".to_string()),
            iterations: 0,
            last_review: None,
            last_fix: None,
        };
        let output = build_json_output(&outcome);
        // None fields should be omitted via skip_serializing_if
        let json = serde_json::to_value(&output).unwrap();
        assert!(!json.as_object().unwrap().contains_key("last_review"));
        assert!(!json.as_object().unwrap().contains_key("last_fix"));
        insta::assert_json_snapshot!(output, @r#"
        {
          "result": "error",
          "iterations": 0,
          "summary": "Agent crashed"
        }
        "#);
    }

    #[test]
    fn test_collect_sensitive_overrides_empty_when_no_local_overrides() {
        let config = Config::default();
        let overrides = collect_sensitive_overrides(&config);
        assert!(overrides.is_empty());
    }

    #[test]
    fn test_collect_sensitive_overrides_detects_ai_config_keys() {
        let mut config = Config::default();
        config.local_overrides.insert("ai.reviewer".to_string());
        config
            .local_overrides
            .insert("ai.reviewee_additional_tools".to_string());

        let overrides = collect_sensitive_overrides(&config);
        assert_eq!(overrides.len(), 2);
        assert!(overrides.iter().any(|o| o.as_ref() == "ai.reviewer"));
        assert!(overrides
            .iter()
            .any(|o| o.as_ref() == "ai.reviewee_additional_tools"));
    }

    #[test]
    fn test_collect_sensitive_overrides_ignores_non_sensitive_keys() {
        let mut config = Config::default();
        // Non-sensitive keys should not appear
        config.local_overrides.insert("diff.theme".to_string());
        config
            .local_overrides
            .insert("keybindings.move_down".to_string());

        let overrides = collect_sensitive_overrides(&config);
        assert!(overrides.is_empty());
    }

    #[test]
    fn test_collect_sensitive_overrides_detects_local_prompt_files() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let prompts_dir = project_root.join(".octorus/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("reviewer.md"), "custom prompt").unwrap();

        let config = Config {
            project_root,
            ..Config::default()
        };

        let overrides = collect_sensitive_overrides(&config);
        assert_eq!(overrides.len(), 1);
        assert!(overrides[0].as_ref().contains("local prompt: reviewer.md"));
    }

    #[test]
    fn test_collect_sensitive_overrides_combines_config_and_prompt_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let prompts_dir = project_root.join(".octorus/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("reviewee.md"), "custom").unwrap();

        let mut config = Config {
            project_root,
            ..Config::default()
        };
        config.local_overrides.insert("ai.auto_post".to_string());

        let overrides = collect_sensitive_overrides(&config);
        assert_eq!(overrides.len(), 2);
        assert!(overrides.iter().any(|o| o.as_ref() == "ai.auto_post"));
        assert!(overrides
            .iter()
            .any(|o| o.as_ref().contains("local prompt: reviewee.md")));
    }

    #[test]
    fn test_write_json_file_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.json");
        let output = HeadlessJsonOutput {
            result: HeadlessResultKind::Approved,
            iterations: 1,
            summary: "All good".to_string(),
            last_review: None,
            last_fix: None,
        };

        write_json_file(&output, path.to_str().unwrap());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["result"], "approved");
        assert_eq!(parsed["iterations"], 1);
        assert_eq!(parsed["summary"], "All good");
    }

    #[test]
    fn test_write_json_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/result.json");
        let output = HeadlessJsonOutput {
            result: HeadlessResultKind::Error,
            iterations: 0,
            summary: "Failed".to_string(),
            last_review: None,
            last_fix: None,
        };

        write_json_file(&output, path.to_str().unwrap());

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["result"], "error");
    }

    #[test]
    fn test_write_json_file_atomic_no_temp_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.json");
        let output = HeadlessJsonOutput {
            result: HeadlessResultKind::Approved,
            iterations: 2,
            summary: "Done".to_string(),
            last_review: None,
            last_fix: None,
        };

        write_json_file(&output, path.to_str().unwrap());

        // Temp file should not remain
        let temp_path = path.with_file_name(format!(".result.json.{}.tmp", std::process::id()));
        assert!(!temp_path.exists());
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_event_loop_handles_rally_started_then_approved() {
        // Ensure the new RallyStarted arm in the headless event loop:
        // (a) does not return early, and (b) lets the loop reach Approved.
        let (tx, mut rx) = mpsc::channel(8);
        let (cmd_tx, _cmd_rx) = mpsc::channel(8);

        tx.send(RallyEvent::RallyStarted { review_only: true })
            .await
            .unwrap();
        tx.send(RallyEvent::IterationStarted(1)).await.unwrap();
        tx.send(RallyEvent::Approved("Looks good".to_string()))
            .await
            .unwrap();
        drop(tx);

        let outcome = run_headless_event_loop(&mut rx, &cmd_tx, false, true).await;
        match outcome.result {
            HeadlessResult::Approved(summary) => {
                assert_eq!(summary, "Looks good");
            }
            other => panic!(
                "expected Approved after RallyStarted+Approved sequence, got {:?}",
                match other {
                    HeadlessResult::NotApproved(s) => format!("NotApproved({})", s),
                    HeadlessResult::Error(s) => format!("Error({})", s),
                    HeadlessResult::Approved(_) => unreachable!(),
                }
            ),
        }
        assert_eq!(outcome.iterations, 1);
    }

    /// Drain commands sent to the orchestrator and assert exactly one
    /// PostConfirmResponse matching `expected` was sent. Other variants
    /// (PermissionResponse, SkipClarification, etc.) are ignored — the
    /// post-confirmation flow is the only thing under test here.
    async fn assert_single_post_confirm(
        mut cmd_rx: mpsc::Receiver<OrchestratorCommand>,
        expected: bool,
        ctx: &str,
    ) {
        let mut seen: Vec<bool> = Vec::new();
        while let Some(cmd) = cmd_rx.recv().await {
            if let OrchestratorCommand::PostConfirmResponse(b) = cmd {
                seen.push(b);
            }
        }
        assert_eq!(
            seen,
            vec![expected],
            "{}: expected exactly one PostConfirmResponse({}), got {:?}",
            ctx,
            expected,
            seen
        );
    }

    /// Build a fake ProposalPostInfo for confirm-flow tests.
    fn fake_proposal_post_info() -> crate::ai::orchestrator::ProposalPostInfo {
        crate::ai::orchestrator::ProposalPostInfo {
            summary: "fake proposal".to_string(),
            target_files: vec!["src/foo.rs".to_string()],
            plan_item_count: 1,
        }
    }

    fn fake_review_post_info() -> crate::ai::orchestrator::ReviewPostInfo {
        crate::ai::orchestrator::ReviewPostInfo {
            action: "RequestChanges".to_string(),
            summary: "fake review".to_string(),
            comment_count: 0,
        }
    }

    fn fake_fix_post_info() -> crate::ai::orchestrator::FixPostInfo {
        crate::ai::orchestrator::FixPostInfo {
            summary: "fake fix".to_string(),
            files_modified: vec!["src/bar.rs".to_string()],
        }
    }

    #[tokio::test]
    async fn test_headless_proposal_post_skipped_when_auto_post_false() {
        // Regression: headless mode previously auto-approved every post
        // confirmation regardless of auto_post, silently pushing the
        // final proposal to the PR even when the user configured
        // ai.auto_post = false. The fix must respect auto_post: when false,
        // skip (PostConfirmResponse(false)) instead of approve.
        let (tx, mut rx) = mpsc::channel(8);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        tx.send(RallyEvent::ProposalPostConfirmNeeded(
            fake_proposal_post_info(),
        ))
        .await
        .unwrap();
        drop(tx);

        let _outcome = run_headless_event_loop(&mut rx, &cmd_tx, false, false).await;
        drop(cmd_tx);

        assert_single_post_confirm(cmd_rx, false, "proposal/auto_post=false").await;
    }

    #[tokio::test]
    async fn test_headless_proposal_post_approved_when_auto_post_true() {
        // Inverse of the regression: auto_post=true must still auto-approve.
        let (tx, mut rx) = mpsc::channel(8);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        tx.send(RallyEvent::ProposalPostConfirmNeeded(
            fake_proposal_post_info(),
        ))
        .await
        .unwrap();
        drop(tx);

        let _outcome = run_headless_event_loop(&mut rx, &cmd_tx, false, true).await;
        drop(cmd_tx);

        assert_single_post_confirm(cmd_rx, true, "proposal/auto_post=true").await;
    }

    #[tokio::test]
    async fn test_headless_review_post_skipped_when_auto_post_false() {
        // Same consistency rule for review post.
        let (tx, mut rx) = mpsc::channel(8);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        tx.send(RallyEvent::ReviewPostConfirmNeeded(fake_review_post_info()))
            .await
            .unwrap();
        drop(tx);

        let _outcome = run_headless_event_loop(&mut rx, &cmd_tx, false, false).await;
        drop(cmd_tx);

        assert_single_post_confirm(cmd_rx, false, "review/auto_post=false").await;
    }

    #[tokio::test]
    async fn test_headless_fix_post_skipped_when_auto_post_false() {
        // Same consistency rule for fix post.
        let (tx, mut rx) = mpsc::channel(8);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        tx.send(RallyEvent::FixPostConfirmNeeded(fake_fix_post_info()))
            .await
            .unwrap();
        drop(tx);

        let _outcome = run_headless_event_loop(&mut rx, &cmd_tx, false, false).await;
        drop(cmd_tx);

        assert_single_post_confirm(cmd_rx, false, "fix/auto_post=false").await;
    }

    #[tokio::test]
    async fn test_headless_local_mode_skips_regardless_of_auto_post() {
        // local_mode is a hard skip — there is no PR to post to. auto_post
        // (true or false) does not change this.
        for auto_post in [true, false] {
            let (tx, mut rx) = mpsc::channel(8);
            let (cmd_tx, cmd_rx) = mpsc::channel(8);
            tx.send(RallyEvent::ProposalPostConfirmNeeded(
                fake_proposal_post_info(),
            ))
            .await
            .unwrap();
            drop(tx);

            let _outcome = run_headless_event_loop(&mut rx, &cmd_tx, true, auto_post).await;
            drop(cmd_tx);

            assert_single_post_confirm(
                cmd_rx,
                false,
                &format!("local_mode=true/auto_post={}", auto_post),
            )
            .await;
        }
    }

    #[test]
    fn test_write_error_json_with_output_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("error.json");

        write_error_json("something failed", Some(path.to_str().unwrap()));

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["result"], "error");
        assert_eq!(parsed["summary"], "something failed");
    }
}
