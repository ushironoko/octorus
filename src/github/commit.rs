use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::client::{gh_api, gh_command};

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

/// コミット一覧のページネーション結果
pub struct CommitListPage {
    pub items: Vec<PrCommit>,
    pub has_more: bool,
}

/// PRのコミット一覧を取得（ページネーション対応）
///
/// `page` は 1-indexed。`per_page + 1` 件を要求して `has_more` を判定する。
/// GitHub API は最大250コミットまで（API仕様制限）。
pub async fn fetch_pr_commits(
    repo: &str,
    pr_number: u32,
    page: u32,
    per_page: u32,
) -> Result<CommitListPage> {
    let fetch_count = per_page + 1;
    let endpoint = format!(
        "repos/{}/pulls/{}/commits?per_page={}&page={}",
        repo, pr_number, fetch_count, page
    );
    let json = gh_api(&endpoint)
        .await
        .context("Failed to fetch PR commits")?;

    let responses: Vec<CommitResponse> =
        serde_json::from_value(json).context("Failed to parse PR commits response")?;

    let has_more = responses.len() > per_page as usize;

    let commits: Vec<PrCommit> = responses
        .into_iter()
        .take(per_page as usize)
        .map(|r| commit_response_to_pr_commit(r))
        .collect();

    Ok(CommitListPage {
        items: commits,
        has_more,
    })
}

fn commit_response_to_pr_commit(r: CommitResponse) -> PrCommit {
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

/// ローカル git log からコミット一覧を取得（ページネーション対応）
///
/// デフォルトブランチ（main/master）からの差分コミットを返す。
/// `git rev-list --count` で総数を取得し、`--skip` / `--max-count` で
/// 必要なページ分のみ取得する（全件取得を回避）。
pub async fn fetch_local_commits(
    working_dir: Option<&str>,
    offset: u32,
    limit: u32,
) -> Result<CommitListPage> {
    // デフォルトブランチを検出（upstream tracking branch ではなく main/master）
    let default_branch = detect_default_branch(working_dir).await;

    let range = default_branch
        .map(|b| format!("{}..HEAD", b))
        .unwrap_or_else(|| "HEAD".to_string());

    // Phase 1: 総コミット数を取得（SHA のみカウント、高速）
    let total = match count_commits(working_dir, &range).await {
        Ok(n) => n,
        Err(_) => {
            // range が無効の場合は HEAD のみでフォールバック
            count_commits(working_dir, "HEAD").await.unwrap_or(0)
        }
    };

    if total == 0 || offset as usize >= total {
        return Ok(CommitListPage {
            items: vec![],
            has_more: false,
        });
    }

    let has_more = (offset + limit) < total as u32;

    // Phase 2: 必要なページ分のみ取得
    // git log は newest-first で出力するため、oldest-first の offset/limit を
    // newest-first の --skip/--max-count に変換してから Rust 側で reverse する。
    let remaining = total.saturating_sub(offset as usize);
    let max_count = (limit as usize).min(remaining);
    let skip = total.saturating_sub(offset as usize + max_count);

    let mut cmd = tokio::process::Command::new("git");
    cmd.args([
        "log",
        "--format=%H%x00%s%x00%an%x00%aI",
        &format!("--skip={}", skip),
        &format!("--max-count={}", max_count),
        &range,
    ]);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    let output = cmd.output().await.context("Failed to run git log")?;

    if !output.status.success() {
        // range が無効の場合は HEAD のみでフォールバック
        let mut cmd2 = tokio::process::Command::new("git");
        cmd2.args([
            "log",
            "--format=%H%x00%s%x00%an%x00%aI",
            &format!("--skip={}", skip),
            &format!("--max-count={}", max_count),
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
        let mut items = parse_git_log_output(&output2.stdout);
        items.reverse();
        return Ok(CommitListPage { items, has_more });
    }

    let mut items = parse_git_log_output(&output.stdout);
    items.reverse(); // newest-first → oldest-first
    Ok(CommitListPage { items, has_more })
}

/// range 内のコミット総数を取得（SHA カウントのみ、高速）
async fn count_commits(working_dir: Option<&str>, range: &str) -> Result<usize> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["rev-list", "--count", range]);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    let output = cmd.output().await.context("Failed to count commits")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git rev-list --count failed: {}", stderr.trim());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse()
        .context("Failed to parse commit count")
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

fn parse_git_log_output(stdout: &[u8]) -> Vec<PrCommit> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
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
        .collect()
}

/// テスト用: 全件パース済みデータから offset/limit でページ切り出し
#[cfg(test)]
fn parse_git_log_page(stdout: &[u8], offset: u32, limit: u32) -> Result<CommitListPage> {
    let all = parse_git_log_output(stdout);
    let has_more = all.len() > (offset + limit) as usize;
    let items: Vec<PrCommit> = all
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    Ok(CommitListPage { items, has_more })
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

    /// 50件のコミットデータを生成し、ページ境界が重複しないことを検証
    #[test]
    fn test_parse_git_log_page_non_overlapping_pagination() {
        // 50件のコミットを生成（git log --reverse 相当: 古い順）
        let lines: Vec<String> = (0..50)
            .map(|i| format!("sha{:03}\x00commit {}\x00author\x002024-01-01T00:00:00Z", i, i))
            .collect();
        let stdout = lines.join("\n");
        let bytes = stdout.as_bytes();

        let limit = 20u32;

        // ページ1: offset=0
        let page1 = parse_git_log_page(bytes, 0, limit).unwrap();
        assert_eq!(page1.items.len(), 20);
        assert!(page1.has_more);
        assert_eq!(page1.items[0].message, "commit 0");
        assert_eq!(page1.items[19].message, "commit 19");

        // ページ2: offset=20
        let page2 = parse_git_log_page(bytes, 20, limit).unwrap();
        assert_eq!(page2.items.len(), 20);
        assert!(page2.has_more);
        assert_eq!(page2.items[0].message, "commit 20");
        assert_eq!(page2.items[19].message, "commit 39");

        // ページ3: offset=40（残り10件）
        let page3 = parse_git_log_page(bytes, 40, limit).unwrap();
        assert_eq!(page3.items.len(), 10);
        assert!(!page3.has_more);
        assert_eq!(page3.items[0].message, "commit 40");
        assert_eq!(page3.items[9].message, "commit 49");

        // ページ間で重複がないことを検証
        let page1_shas: Vec<&str> = page1.items.iter().map(|c| c.sha.as_str()).collect();
        let page2_shas: Vec<&str> = page2.items.iter().map(|c| c.sha.as_str()).collect();
        let page3_shas: Vec<&str> = page3.items.iter().map(|c| c.sha.as_str()).collect();

        for sha in &page1_shas {
            assert!(!page2_shas.contains(sha), "page1 and page2 overlap: {}", sha);
            assert!(!page3_shas.contains(sha), "page1 and page3 overlap: {}", sha);
        }
        for sha in &page2_shas {
            assert!(!page3_shas.contains(sha), "page2 and page3 overlap: {}", sha);
        }
    }
}
