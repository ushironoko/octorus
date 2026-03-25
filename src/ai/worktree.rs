use anyhow::{bail, Context, Result};
use tracing::warn;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const GIT_TIMEOUT_SECS: u64 = 30;

pub async fn get_repo_root(working_dir: Option<&str>) -> Result<String> {
    let dir = working_dir.unwrap_or(".");
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .await?;
    if !output.status.success() {
        bail!("Not in a git repository (resolved from '{}')", dir);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub enum WorktreeSetup {
    Created { path: String, branch: String },
    ExistingReused { path: String },
}

pub fn rally_branch_name(pr_number: u32) -> String {
    format!("octorus/rally/{}", pr_number)
}

pub async fn setup_rally_worktree(
    repo_root: &str,
    target_dir: &str,
    pr_number: u32,
    head_sha: &str,
) -> Result<WorktreeSetup> {
    // So creation, validation, and agent execution all use the same path
    // regardless of process CWD.
    let abs_target = absolutize_path(target_dir);
    let abs_target_str = abs_target.to_string_lossy();

    if abs_target.exists() {
        validate_existing_worktree(&abs_target_str, repo_root).await?;
        return Ok(WorktreeSetup::ExistingReused {
            path: abs_target_str.into_owned(),
        });
    }

    let fetch_result = timeout(
        Duration::from_secs(GIT_TIMEOUT_SECS),
        Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(repo_root)
            .output(),
    )
    .await;

    match fetch_result {
        Ok(Ok(output)) if !output.status.success() => {
            warn!("git fetch origin failed, continuing with local refs");
        }
        Ok(Err(e)) => {
            warn!("git fetch command failed: {}, continuing with local refs", e);
        }
        Err(_) => {
            warn!(
                "git fetch timed out after {}s, continuing with local refs",
                GIT_TIMEOUT_SECS
            );
        }
        _ => {}
    }

    let branch = rally_branch_name(pr_number);
    let branch_exists = check_branch_exists(repo_root, &branch).await;

    if branch_exists {
        let output = Command::new("git")
            .args(["worktree", "add", abs_target_str.as_ref(), &branch])
            .current_dir(repo_root)
            .output()
            .await
            .context("Failed to run git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree add failed: {}", stderr.trim());
        }

        let reset_output = Command::new("git")
            .args(["reset", "--hard", head_sha])
            .current_dir(abs_target_str.as_ref())
            .output()
            .await
            .context("Failed to run git reset")?;

        if !reset_output.status.success() {
            let stderr = String::from_utf8_lossy(&reset_output.stderr);
            warn!("git reset --hard {} failed: {}", head_sha, stderr.trim());
        }
    } else {
        let output = Command::new("git")
            .args(["worktree", "add", "-b", &branch, abs_target_str.as_ref(), head_sha])
            .current_dir(repo_root)
            .output()
            .await
            .context("Failed to run git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree add failed: {}", stderr.trim());
        }
    }

    Ok(WorktreeSetup::Created {
        path: abs_target_str.into_owned(),
        branch,
    })
}

pub async fn validate_existing_worktree(target_dir: &str, repo_root: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(target_dir)
        .output()
        .await
        .context("Failed to check if directory is a git repository")?;

    if !output.status.success() {
        bail!(
            "Directory '{}' exists but is not a git repository",
            target_dir
        );
    }

    // Reject subdirectories: target_dir must be the worktree/repo root itself
    let toplevel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let toplevel_canonical =
        std::fs::canonicalize(&toplevel).unwrap_or_else(|_| PathBuf::from(&toplevel));
    let target_canonical =
        std::fs::canonicalize(target_dir).unwrap_or_else(|_| absolutize_path(target_dir));

    if toplevel_canonical != target_canonical {
        bail!(
            "Directory '{}' is a subdirectory of repository '{}', not a worktree root. \
             --working-dir must point to a repository or worktree root.",
            target_dir,
            toplevel
        );
    }

    let target_objects = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(target_dir)
        .output()
        .await?;
    let repo_objects = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(repo_root)
        .output()
        .await?;

    if target_objects.status.success() && repo_objects.status.success() {
        let target_path = String::from_utf8_lossy(&target_objects.stdout)
            .trim()
            .to_string();
        let repo_path = String::from_utf8_lossy(&repo_objects.stdout)
            .trim()
            .to_string();

        // --git-common-dir returns relative paths against CWD, not the repo root
        let target_resolved = if Path::new(&target_path).is_relative() {
            Path::new(target_dir).join(&target_path)
        } else {
            target_path.into()
        };
        let repo_resolved = if Path::new(&repo_path).is_relative() {
            Path::new(repo_root).join(&repo_path)
        } else {
            repo_path.into()
        };

        let target_common_canonical =
            std::fs::canonicalize(&target_resolved).unwrap_or(target_resolved);
        let repo_common_canonical =
            std::fs::canonicalize(&repo_resolved).unwrap_or(repo_resolved);

        if target_common_canonical != repo_common_canonical {
            bail!(
                "Directory '{}' belongs to a different repository",
                target_dir
            );
        }
    }

    let status_output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(target_dir)
        .output()
        .await
        .context("Failed to check git status")?;

    if status_output.status.success() {
        let status = String::from_utf8_lossy(&status_output.stdout);
        if !status.trim().is_empty() {
            bail!(
                "Directory '{}' has uncommitted changes. Please commit or stash them before starting a rally.",
                target_dir
            );
        }
    }

    Ok(())
}

fn absolutize_path(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

async fn check_branch_exists(repo_root: &str, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", &format!("refs/heads/{}", branch)])
        .current_dir(repo_root)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn cleanup_commands(worktree_path: &str, pr_number: u32) -> String {
    let branch = rally_branch_name(pr_number);
    format!(
        "git worktree remove {} && git branch -D {}",
        worktree_path, branch
    )
}
