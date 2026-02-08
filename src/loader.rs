use tokio::sync::mpsc;
use tracing::warn;

use crate::diff;
use crate::github::{self, ChangedFile, PullRequest};

pub enum DataLoadResult {
    /// APIからデータ取得成功
    Success {
        pr: Box<PullRequest>,
        files: Vec<ChangedFile>,
    },
    /// エラー
    Error(String),
}

/// コメント送信結果
pub enum CommentSubmitResult {
    /// 送信成功
    Success,
    /// エラー
    Error(String),
}

/// バックグラウンド取得モード
pub enum FetchMode {
    /// 新規取得（キャッシュミスまたは強制更新）
    Fresh,
    /// 更新チェックのみ（キャッシュヒット時）
    CheckUpdate(String), // cached updated_at
}

/// バックグラウンドでPRデータを取得
pub async fn fetch_pr_data(
    repo: String,
    pr_number: u32,
    mode: FetchMode,
    tx: mpsc::Sender<DataLoadResult>,
) {
    match mode {
        FetchMode::Fresh => {
            fetch_and_send(&repo, pr_number, tx).await;
        }
        FetchMode::CheckUpdate(cached_updated_at) => {
            check_for_updates(&repo, pr_number, &cached_updated_at, tx).await;
        }
    }
}

async fn fetch_and_send(repo: &str, pr_number: u32, tx: mpsc::Sender<DataLoadResult>) {
    match tokio::try_join!(
        github::fetch_pr(repo, pr_number),
        github::fetch_changed_files(repo, pr_number)
    ) {
        Ok((pr, mut files)) => {
            // Check if any files have missing patches (large file limitation)
            let has_missing_patches = files.iter().any(|f| f.patch.is_none());

            if has_missing_patches {
                // Fetch full diff using gh pr diff as fallback
                match github::fetch_pr_diff(repo, pr_number).await {
                    Ok(full_diff) => {
                        let mut patch_map = diff::parse_unified_diff(&full_diff);

                        // Apply patches only to files that are missing them
                        for file in files.iter_mut() {
                            if file.patch.is_none() {
                                if let Some(patch) = patch_map.remove(&file.filename) {
                                    file.patch = Some(patch);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Fallback failed, log warning and continue with "No diff available"
                        warn!("Failed to fetch full diff for fallback: {}", e);
                    }
                }
            }

            let _ = tx
                .send(DataLoadResult::Success {
                    pr: Box::new(pr),
                    files,
                })
                .await;
        }
        Err(e) => {
            let _ = tx.send(DataLoadResult::Error(e.to_string())).await;
        }
    }
}

async fn check_for_updates(
    repo: &str,
    pr_number: u32,
    cached_updated_at: &str,
    tx: mpsc::Sender<DataLoadResult>,
) {
    // PRの基本情報だけ取得してupdated_atを比較
    if let Ok(fresh_pr) = github::fetch_pr(repo, pr_number).await {
        if fresh_pr.updated_at != cached_updated_at {
            // 更新あり → 全データ再取得
            fetch_and_send(repo, pr_number, tx).await;
        }
        // 更新なし → 何もしない（キャッシュデータをそのまま使用）
    }
}
