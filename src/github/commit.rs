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
