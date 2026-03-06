use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::client::{gh_api_paginate, gh_command};

/// GitHub API レスポンスの中間構造体（nested JSON を平坦化）
#[derive(Debug, Clone, Deserialize)]
struct CommitResponse {
    sha: String,
    commit: CommitDetail,
    author: Option<GitHubUser>,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitDetail {
    message: String,
    author: Option<CommitPersonDetail>,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitPersonDetail {
    name: Option<String>,
    date: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubUser {
    login: String,
}

/// PRコミットの公開型
#[derive(Debug, Clone)]
pub struct PrCommit {
    pub sha: String,
    /// コミットメッセージの1行目
    pub message: String,
    pub author_name: String,
    pub author_login: Option<String>,
    /// ISO 8601 形式の日時文字列
    pub date: String,
}

impl PrCommit {
    /// SHA の先頭7文字を返す
    pub fn short_sha(&self) -> &str {
        &self.sha[..self.sha.len().min(7)]
    }
}

/// PRのコミット一覧を取得
///
/// GitHub API はページネーションしても最大250コミットが上限（API仕様制限）
pub async fn fetch_pr_commits(repo: &str, pr_number: u32) -> Result<Vec<PrCommit>> {
    let endpoint = format!(
        "repos/{}/pulls/{}/commits?per_page=100",
        repo, pr_number
    );
    let json = gh_api_paginate(&endpoint)
        .await
        .context("Failed to fetch PR commits")?;

    let responses: Vec<CommitResponse> =
        serde_json::from_value(json).context("Failed to parse PR commits response")?;

    let commits = responses
        .into_iter()
        .map(|r| {
            let message = r.commit.message.lines().next().unwrap_or("").to_string();
            let author_name = r
                .commit
                .author
                .as_ref()
                .and_then(|a| a.name.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let author_login = r.author.map(|a| a.login);
            let date = r
                .commit
                .author
                .as_ref()
                .and_then(|a| a.date.clone())
                .unwrap_or_default();
            PrCommit {
                sha: r.sha,
                message,
                author_name,
                author_login,
                date,
            }
        })
        .collect();

    Ok(commits)
}

/// 特定コミットの unified diff を取得
pub async fn fetch_commit_diff(repo: &str, sha: &str) -> Result<String> {
    let endpoint = format!("repos/{}/commits/{}", repo, sha);
    let diff = gh_command(&[
        "api",
        "-H",
        "Accept: application/vnd.github.v3.diff",
        &endpoint,
    ])
    .await
    .context("Failed to fetch commit diff")?;
    Ok(diff)
}

/// ローカル git log からコミット一覧を取得
///
/// デフォルトブランチ（main/master）からの差分コミットを返す。
/// デフォルトブランチが検出できない場合は HEAD から最大100件を返す。
pub async fn fetch_local_commits(working_dir: Option<&str>) -> Result<Vec<PrCommit>> {
    // デフォルトブランチを検出（upstream tracking branch ではなく main/master）
    let default_branch = detect_default_branch(working_dir).await;

    let range = default_branch
        .map(|b| format!("{}..HEAD", b))
        .unwrap_or_else(|| "HEAD~100..HEAD".to_string());

    let mut cmd = tokio::process::Command::new("git");
    cmd.args([
        "log",
        "--format=%H%x00%s%x00%an%x00%aI",
        "--reverse",
        &range,
    ]);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    let output = cmd.output().await.context("Failed to run git log")?;

    if !output.status.success() {
        // range が無効の場合は HEAD から直近100件
        let mut cmd2 = tokio::process::Command::new("git");
        cmd2.args([
            "log",
            "--format=%H%x00%s%x00%an%x00%aI",
            "--reverse",
            "-100",
        ]);
        if let Some(dir) = working_dir {
            cmd2.current_dir(dir);
        }
        let output2 = cmd2
            .output()
            .await
            .context("Failed to run git log fallback")?;
        if !output2.status.success() {
            let stderr = String::from_utf8_lossy(&output2.stderr);
            anyhow::bail!("git log fallback failed: {}", stderr.trim());
        }
        return parse_git_log_output(&output2.stdout);
    }

    parse_git_log_output(&output.stdout)
}

/// デフォルトブランチ（origin/main or origin/master）を検出
async fn detect_default_branch(working_dir: Option<&str>) -> Option<String> {
    for candidate in &["origin/main", "origin/master"] {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["rev-parse", "--verify", candidate]);
        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }
        if let Ok(output) = cmd.output().await {
            if output.status.success() {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn parse_git_log_output(stdout: &[u8]) -> Result<Vec<PrCommit>> {
    let text = String::from_utf8_lossy(stdout);
    let commits = text
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '\0').collect();
            if parts.len() < 4 {
                return None;
            }
            Some(PrCommit {
                sha: parts[0].to_string(),
                message: parts[1].to_string(),
                author_name: parts[2].to_string(),
                author_login: None,
                date: parts[3].to_string(),
            })
        })
        .collect();
    Ok(commits)
}

/// ローカル git show でコミットの unified diff を取得
pub async fn fetch_local_commit_diff(
    working_dir: Option<&str>,
    sha: &str,
) -> Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["show", "--format=", "--patch", sha]);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    let output = cmd
        .output()
        .await
        .context("Failed to run git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git show failed: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// ISO 8601 の日時文字列を相対時間表示に変換
///
/// 例: "2h ago", "3d ago", "1m ago"
pub fn format_relative_time(iso_date: &str) -> String {
    let Ok(dt) = DateTime::parse_from_rfc3339(iso_date) else {
        return iso_date.to_string();
    };

    let now = Utc::now();
    let duration = now.signed_duration_since(dt.with_timezone(&Utc));

    let seconds = duration.num_seconds();
    if seconds < 0 {
        return "just now".to_string();
    }

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();
    let weeks = days / 7;
    let months = days / 30;
    let years = days / 365;

    if seconds < 60 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{}m ago", minutes)
    } else if hours < 24 {
        format!("{}h ago", hours)
    } else if days < 7 {
        format!("{}d ago", days)
    } else if weeks < 5 {
        format!("{}w ago", weeks)
    } else if months < 12 {
        format!("{}mo ago", months)
    } else {
        format!("{}y ago", years)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_relative_time_minutes() {
        let now = Utc::now();
        let five_min_ago = now - chrono::Duration::minutes(5);
        let iso = five_min_ago.to_rfc3339();
        let result = format_relative_time(&iso);
        assert_eq!(result, "5m ago");
    }

    #[test]
    fn test_format_relative_time_hours() {
        let now = Utc::now();
        let two_hours_ago = now - chrono::Duration::hours(2);
        let iso = two_hours_ago.to_rfc3339();
        let result = format_relative_time(&iso);
        assert_eq!(result, "2h ago");
    }

    #[test]
    fn test_format_relative_time_days() {
        let now = Utc::now();
        let three_days_ago = now - chrono::Duration::days(3);
        let iso = three_days_ago.to_rfc3339();
        let result = format_relative_time(&iso);
        assert_eq!(result, "3d ago");
    }

    #[test]
    fn test_format_relative_time_invalid() {
        let result = format_relative_time("not-a-date");
        assert_eq!(result, "not-a-date");
    }

    #[test]
    fn test_short_sha() {
        let commit = PrCommit {
            sha: "abc1234567890".to_string(),
            message: "test".to_string(),
            author_name: "author".to_string(),
            author_login: None,
            date: String::new(),
        };
        assert_eq!(commit.short_sha(), "abc1234");
    }
}
