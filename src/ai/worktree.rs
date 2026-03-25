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

/// Sibling directory to the repo root: `{repo_root}/../{repo_name}-rally-{pr}`
pub fn default_worktree_path(repo_root: &str, pr_number: u32) -> String {
    let root = Path::new(repo_root);
    let repo_name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let sibling = root
        .parent()
        .unwrap_or(root)
        .join(format!("{}-rally-{}", repo_name, pr_number));
    sibling.to_string_lossy().to_string()
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

    let branch = find_available_rally_branch(repo_root, pr_number).await;

    if check_branch_exists(repo_root, &branch).await {
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

/// Find a rally branch name that is not currently checked out in another worktree.
/// Tries `octorus/rally/{pr}`, then `octorus/rally/{pr}-2`, `-3`, etc.
async fn find_available_rally_branch(repo_root: &str, pr_number: u32) -> String {
    let base = rally_branch_name(pr_number);
    if !is_branch_locked_by_worktree(repo_root, &base).await {
        return base;
    }
    for suffix in 2..=100 {
        let candidate = format!("{}-{}", base, suffix);
        if !is_branch_locked_by_worktree(repo_root, &candidate).await {
            return candidate;
        }
    }
    // Extremely unlikely, but fall back to base and let git report the error
    base
}

async fn is_branch_locked_by_worktree(repo_root: &str, branch: &str) -> bool {
    // A branch is "locked" if it exists AND is currently checked out in a worktree.
    // `git worktree list --porcelain` lists all worktrees with their branches.
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .await;
    let Ok(output) = output else { return false };
    if !output.status.success() {
        return false;
    }
    let needle = format!("branch refs/heads/{}", branch);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == needle)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed: {}", args, status);
    }

    fn write_file(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn init_repo(dir: &Path) -> String {
        run_git(dir, &["init", "-b", "main"]);
        write_file(&dir.join("README.md"), "# test\n");
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "initial"]);

        let output = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn test_rally_branch_name() {
        assert_eq!(rally_branch_name(123), "octorus/rally/123");
        assert_eq!(rally_branch_name(0), "octorus/rally/0");
    }

    #[test]
    fn test_default_worktree_path() {
        assert_eq!(
            default_worktree_path("/home/user/repos/octorus", 42),
            "/home/user/repos/octorus-rally-42"
        );
        assert_eq!(
            default_worktree_path("/repos/my-app", 1),
            "/repos/my-app-rally-1"
        );
    }

    #[test]
    fn test_cleanup_commands() {
        assert_eq!(
            cleanup_commands("/tmp/wt", 99),
            "git worktree remove /tmp/wt && git branch -D octorus/rally/99"
        );
    }

    #[tokio::test]
    async fn test_setup_creates_worktree_for_nonexistent_path() {
        let tempdir = tempdir().unwrap();
        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let head_sha = init_repo(&repo_dir);

        let wt_path = tempdir.path().join("repo-rally-1");
        let result = setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt_path.to_str().unwrap(),
            1,
            &head_sha,
        )
        .await
        .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join("README.md").exists());
        match result {
            WorktreeSetup::Created { ref branch, .. } => {
                assert_eq!(branch, "octorus/rally/1");
            }
            WorktreeSetup::ExistingReused { .. } => panic!("expected Created"),
        }
    }

    #[tokio::test]
    async fn test_setup_reuses_existing_clean_worktree() {
        let tempdir = tempdir().unwrap();
        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let head_sha = init_repo(&repo_dir);

        let wt_path = tempdir.path().join("repo-rally-2");
        // First creation
        setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt_path.to_str().unwrap(),
            2,
            &head_sha,
        )
        .await
        .unwrap();

        // Second call reuses
        let result = setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt_path.to_str().unwrap(),
            2,
            &head_sha,
        )
        .await
        .unwrap();

        matches!(result, WorktreeSetup::ExistingReused { .. });
    }

    #[tokio::test]
    async fn test_setup_resets_existing_branch_to_head_sha() {
        let tempdir = tempdir().unwrap();
        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let sha1 = init_repo(&repo_dir);

        // Add a second commit
        write_file(&repo_dir.join("second.txt"), "second\n");
        run_git(&repo_dir, &["add", "."]);
        run_git(&repo_dir, &["commit", "-m", "second"]);
        let sha2 = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo_dir)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();

        let wt_path = tempdir.path().join("repo-rally-3");

        // Create worktree at sha1
        setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt_path.to_str().unwrap(),
            3,
            &sha1,
        )
        .await
        .unwrap();

        // Remove worktree to free the branch
        StdCommand::new("git")
            .args(["worktree", "remove", wt_path.to_str().unwrap()])
            .current_dir(&repo_dir)
            .status()
            .unwrap();

        // Re-create at sha2 — branch already exists, should reset
        setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt_path.to_str().unwrap(),
            3,
            &sha2,
        )
        .await
        .unwrap();

        // Verify HEAD in worktree matches sha2
        let wt_head = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&wt_path)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();
        assert_eq!(wt_head, sha2);
    }

    #[tokio::test]
    async fn test_validate_rejects_non_git_directory() {
        let tempdir = tempdir().unwrap();
        let non_git = tempdir.path().join("not-a-repo");
        std::fs::create_dir_all(&non_git).unwrap();

        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir);

        let err = validate_existing_worktree(
            non_git.to_str().unwrap(),
            repo_dir.to_str().unwrap(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("not a git repository"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_validate_rejects_dirty_worktree() {
        let tempdir = tempdir().unwrap();
        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let head_sha = init_repo(&repo_dir);

        let wt_path = tempdir.path().join("repo-rally-4");
        setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt_path.to_str().unwrap(),
            4,
            &head_sha,
        )
        .await
        .unwrap();

        // Dirty the worktree
        write_file(&wt_path.join("dirty.txt"), "uncommitted\n");

        let err = validate_existing_worktree(
            wt_path.to_str().unwrap(),
            repo_dir.to_str().unwrap(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("uncommitted changes"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_validate_rejects_different_repository() {
        let tempdir = tempdir().unwrap();

        let repo_a = tempdir.path().join("repo-a");
        std::fs::create_dir_all(&repo_a).unwrap();
        init_repo(&repo_a);

        let repo_b = tempdir.path().join("repo-b");
        std::fs::create_dir_all(&repo_b).unwrap();
        init_repo(&repo_b);

        let err = validate_existing_worktree(
            repo_b.to_str().unwrap(),
            repo_a.to_str().unwrap(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("different repository"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_validate_rejects_subdirectory() {
        let tempdir = tempdir().unwrap();
        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir);

        let subdir = repo_dir.join("src");
        std::fs::create_dir_all(&subdir).unwrap();

        let err = validate_existing_worktree(
            subdir.to_str().unwrap(),
            repo_dir.to_str().unwrap(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("subdirectory"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_validate_accepts_same_repo_worktree() {
        let tempdir = tempdir().unwrap();
        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let head_sha = init_repo(&repo_dir);

        let wt_path = tempdir.path().join("repo-rally-5");
        setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt_path.to_str().unwrap(),
            5,
            &head_sha,
        )
        .await
        .unwrap();

        // Should pass validation
        validate_existing_worktree(
            wt_path.to_str().unwrap(),
            repo_dir.to_str().unwrap(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_get_repo_root_returns_toplevel() {
        let tempdir = tempdir().unwrap();
        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir);

        let subdir = repo_dir.join("src");
        std::fs::create_dir_all(&subdir).unwrap();

        let root = get_repo_root(Some(subdir.to_str().unwrap())).await.unwrap();
        let expected = std::fs::canonicalize(&repo_dir).unwrap();
        let actual = std::fs::canonicalize(root).unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_get_repo_root_fails_for_non_repo() {
        let tempdir = tempdir().unwrap();
        let result = get_repo_root(Some(tempdir.path().to_str().unwrap())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_setup_uses_suffixed_branch_when_base_is_locked() {
        let tempdir = tempdir().unwrap();
        let repo_dir = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let head_sha = init_repo(&repo_dir);

        // First worktree locks octorus/rally/10
        let wt1 = tempdir.path().join("wt1");
        let result1 = setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt1.to_str().unwrap(),
            10,
            &head_sha,
        )
        .await
        .unwrap();
        match &result1 {
            WorktreeSetup::Created { branch, .. } => {
                assert_eq!(branch, "octorus/rally/10");
            }
            _ => panic!("expected Created"),
        }

        // Second worktree for same PR should get suffixed branch
        let wt2 = tempdir.path().join("wt2");
        let result2 = setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt2.to_str().unwrap(),
            10,
            &head_sha,
        )
        .await
        .unwrap();
        match &result2 {
            WorktreeSetup::Created { branch, .. } => {
                assert_eq!(branch, "octorus/rally/10-2");
            }
            _ => panic!("expected Created"),
        }

        // Third worktree increments further
        let wt3 = tempdir.path().join("wt3");
        let result3 = setup_rally_worktree(
            repo_dir.to_str().unwrap(),
            wt3.to_str().unwrap(),
            10,
            &head_sha,
        )
        .await
        .unwrap();
        match &result3 {
            WorktreeSetup::Created { branch, .. } => {
                assert_eq!(branch, "octorus/rally/10-3");
            }
            _ => panic!("expected Created"),
        }

        assert!(wt1.exists());
        assert!(wt2.exists());
        assert!(wt3.exists());
    }
}
