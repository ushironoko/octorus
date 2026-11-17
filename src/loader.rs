use anyhow::{Context, Result};
use chrono::Utc;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::warn;

use crate::diff;
use crate::github::{self, ChangedFile, PullRequest};

pub enum DataLoadResult {
    /// APIからデータ取得成功
    Success {
        pr: Box<PullRequest>,
        files: Vec<ChangedFile>,
        /// Live tip of the base branch from the same metadata read. `None`
        /// means that ref was gone or the field was missing.
        base_tip_sha: Option<String>,
    },
    /// The cached file list and patches are still current. `pr` is the pull
    /// request read used for the comparison, so title, body, state, and user
    /// replace the cached copy even when `updated_at` did not move. `None`
    /// leaves that copy untouched. `viewed` is the fresh per-file viewer state
    /// when that query succeeded; `None` leaves the cached flags untouched.
    Unchanged {
        pr_number: u32,
        viewed: Option<HashMap<String, bool>>,
        pr: Option<Box<PullRequest>>,
    },
    /// エラー
    Error(github::GhFailure),
}

/// 単一ファイルの diff 結果（バッチ/オンデマンド共通）
pub struct SingleFileDiffResult {
    pub filename: String,
    pub patch: Option<String>,
}

/// コメント送信結果
pub enum CommentSubmitResult {
    /// 送信成功（GitHub 上で即時に公開された）
    Success,
    /// 自分の保留中（PENDING）レビューに追加された。レビュー送信まで公開されない。
    AddedToPendingReview,
    /// The add ran past its timeout. GitHub may already hold the draft, so this is
    /// not reported as a failure.
    Unknown,
    /// Refused before anything was written. The text is kept so it can be retried.
    /// The message is the whole footer text.
    Refused(String),
    /// エラー
    Error(String),
}

pub enum ReviewSubmitResult {
    Submitted(crate::app::ReviewAction),
    Failed(String),
    /// The submit ran past its timeout. GitHub may still have accepted the
    /// review, so this is not reported as a failure.
    Unknown,
}

/// バックグラウンド取得モード
pub enum FetchMode {
    /// 新規取得（キャッシュミスまたは強制更新）
    Fresh,
    /// Cache-hit reopen of a complete file list.
    ///
    /// When `updated_at`, head SHA, base ref/SHA, and the live base-branch tip
    /// all match, the file list and patches stay cached. The pull request read
    /// is still applied, and viewed state is still refreshed. A mismatch, a
    /// missing base tip, or a failed read runs the same full fetch as
    /// [`FetchMode::Fresh`]. A cache that is missing a patch or a file list
    /// does not use this mode.
    CheckUpdate(crate::app::CachedPrCheck),
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
        FetchMode::CheckUpdate(cached) => {
            check_for_updates(&repo, pr_number, &cached, tx).await;
        }
    }
}

/// ローカル `git diff` から PR データを再構築して読み込み（2段階ロード版）
///
/// Phase 1: name-status + numstat のみ → ファイル一覧（patch: None）を即座に送信
/// Phase 2: バッチ diff ロードは app.rs 側で start_batch_diff_loading() 経由で行う
pub async fn fetch_local_diff(
    _repo: String,
    working_dir: Option<String>,
    tx: mpsc::Sender<DataLoadResult>,
) {
    let current_workdir = working_dir.as_deref();

    let name_status_output = match run_git_name_status(current_workdir).await {
        Ok(output) => output,
        Err(e) => {
            let _ = tx
                .send(DataLoadResult::Error(github::GhFailure::from_error(&e)))
                .await;
            return;
        }
    };
    let file_statuses = parse_name_status_output(&name_status_output);

    let numstat_output = run_git_numstat(current_workdir).await.ok();
    let file_changes = parse_numstat_output(numstat_output.as_deref());

    let mut files = build_changed_files_lazy(&file_statuses, &file_changes);

    merge_untracked_files_lazy(current_workdir, &mut files).await;

    let pr = PullRequest {
        number: 0,
        node_id: None,
        title: "Local HEAD diff".to_string(),
        body: Some("Current working tree diff from HEAD".to_string()),
        state: "local".to_string(),
        head: github::Branch {
            ref_name: "HEAD".to_string(),
            sha: current_head_sha(current_workdir)
                .await
                .unwrap_or_else(|_| "local".to_string()),
        },
        base: github::Branch {
            ref_name: "local".to_string(),
            sha: "local".to_string(),
        },
        user: github::User {
            login: "local".to_string(),
        },
        updated_at: Utc::now().to_rfc3339(),
    };

    let _ = tx
        .send(DataLoadResult::Success {
            pr: Box::new(pr),
            files,
            base_tip_sha: None,
        })
        .await;
}

/// バッチ diff ロード: ファイルリスト順にバッチで diff を取得し、チャネルに送信
pub async fn fetch_local_diffs_batched(
    working_dir: Option<String>,
    filenames: Vec<String>,
    untracked_filenames: Vec<String>,
    batch_size: usize,
    tx: mpsc::Sender<Vec<SingleFileDiffResult>>,
) {
    let wd = working_dir.as_deref();

    // tracked ファイルをバッチで処理
    for batch in filenames.chunks(batch_size) {
        let mut args = vec!["diff", "HEAD", "--"];
        let batch_strs: Vec<&str> = batch.iter().map(|s| s.as_str()).collect();
        args.extend(&batch_strs);

        let output = run_git_command(wd, &args).await;
        let mut patches = match output {
            Ok(diff_output) => diff::parse_unified_diff(&diff_output),
            Err(_) => HashMap::new(),
        };

        let results: Vec<SingleFileDiffResult> = batch
            .iter()
            .map(|filename| SingleFileDiffResult {
                filename: filename.clone(),
                patch: patches.remove(filename),
            })
            .collect();

        if tx.send(results).await.is_err() {
            return;
        }
    }

    // Untracked files have no blob in HEAD. Unchanged bodies are reused from a
    // stat cache. A cold batch is one `git diff --no-index` of a hardlink tree
    // when that output rewrites to the per-file text, and the per-file command
    // otherwise. Chunks stay the same size the UI already receives.
    let loaded = crate::untracked_diff::patches(wd, &untracked_filenames).await;
    debug_assert_eq!(loaded.len(), untracked_filenames.len());
    for (batch, patches) in untracked_filenames
        .chunks(batch_size)
        .zip(loaded.chunks(batch_size))
    {
        let results: Vec<SingleFileDiffResult> = batch
            .iter()
            .zip(patches.iter())
            .map(|(filename, patch)| SingleFileDiffResult {
                filename: filename.clone(),
                patch: patch.clone(),
            })
            .collect();
        if tx.send(results).await.is_err() {
            return;
        }
    }
}

/// 単一ファイルの diff をオンデマンド取得（tracked + untracked 自動判別）
pub async fn fetch_single_file_diff(
    working_dir: Option<String>,
    filename: String,
    is_untracked: bool,
    tx: mpsc::Sender<SingleFileDiffResult>,
) {
    let wd = working_dir.as_deref();

    let patch = if is_untracked {
        crate::untracked_diff::patch(wd, &filename).await
    } else {
        run_git_diff_file(wd, &filename)
            .await
            .ok()
            .filter(|p| !p.is_empty())
    };

    let _ = tx.send(SingleFileDiffResult { filename, patch }).await;
}

async fn fetch_and_send(repo: &str, pr_number: u32, tx: mpsc::Sender<DataLoadResult>) {
    match tokio::try_join!(
        load_pr_snapshot(repo, pr_number),
        github::fetch_changed_files(repo, pr_number)
    ) {
        Ok((snapshot, mut files)) => {
            let pr = snapshot.pr;
            let base_tip_sha = snapshot.base_tip_sha;
            if let Some(pr_node_id) = pr.node_id.as_deref() {
                match github::fetch_files_viewed_state(repo, pr_node_id).await {
                    Ok(viewed_state) => {
                        for file in files.iter_mut() {
                            file.viewed =
                                viewed_state.get(&file.filename).copied().unwrap_or(false);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to fetch viewed-state for PR files: {}", e);
                    }
                }
            }

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
                    base_tip_sha,
                })
                .await;
        }
        Err(e) => {
            let _ = tx
                .send(DataLoadResult::Error(github::GhFailure::from_error(&e)))
                .await;
        }
    }
}

/// The light check and the full fetch share one metadata read so the base
/// tip does not cost an extra `gh` call. If that read fails, the full fetch
/// still loads the pull request the way it did before the tip was added.
async fn load_pr_snapshot(repo: &str, pr_number: u32) -> Result<github::PrSnapshot> {
    match github::fetch_pr_snapshot(repo, pr_number).await {
        Ok(snapshot) => Ok(snapshot),
        Err(error) => {
            warn!(
                "base-tip metadata failed for {repo}#{pr_number}, reading the pull request without it: {error}"
            );
            let pr = github::fetch_pr(repo, pr_number).await?;
            Ok(github::PrSnapshot {
                pr,
                base_tip_sha: None,
            })
        }
    }
}

async fn check_for_updates(
    repo: &str,
    pr_number: u32,
    cached: &crate::app::CachedPrCheck,
    tx: mpsc::Sender<DataLoadResult>,
) {
    // One PR read compares the timestamp, the commits the diff is based on, and
    // the live tip of the base branch. A match keeps the file list and patches.
    // The read itself is applied, so a title, body, state, or user change that
    // did not move `updated_at` still shows up. A mismatch, a missing base tip,
    // or a failed read uses the full fetch.
    match github::fetch_pr_snapshot(repo, pr_number).await {
        Ok(snapshot) if cached_diff_is_current(cached, &snapshot) => {
            let viewed = refresh_viewed_state(repo, snapshot.pr.node_id.as_deref()).await;
            let _ = tx
                .send(DataLoadResult::Unchanged {
                    pr_number,
                    viewed,
                    pr: Some(Box::new(snapshot.pr)),
                })
                .await;
        }
        Ok(_) => {
            fetch_and_send(repo, pr_number, tx).await;
        }
        Err(error) => {
            warn!(
                "updated_at check failed for {repo}#{pr_number}, falling back to a full fetch: {error}"
            );
            fetch_and_send(repo, pr_number, tx).await;
        }
    }
}

/// The file list is the three-dot diff against the base branch. A timestamp
/// match is not enough when a commit moved, or when the base branch tip moved
/// without `base.sha`. A missing tip on either side is not a match.
fn cached_diff_is_current(
    cached: &crate::app::CachedPrCheck,
    snapshot: &github::PrSnapshot,
) -> bool {
    let fresh = &snapshot.pr;
    let tips_match = match (&cached.base_tip_sha, &snapshot.base_tip_sha) {
        (Some(cached_tip), Some(fresh_tip)) => cached_tip == fresh_tip,
        _ => false,
    };
    fresh.updated_at == cached.updated_at
        && fresh.head.sha == cached.head_sha
        && fresh.base.ref_name == cached.base_ref
        && fresh.base.sha == cached.base_sha
        && tips_match
}

/// Viewer viewed-state is not part of the PR `updated_at` timestamp.
/// `None` means the query did not succeed, so cached flags must be kept.
async fn refresh_viewed_state(
    repo: &str,
    pr_node_id: Option<&str>,
) -> Option<HashMap<String, bool>> {
    let pr_node_id = pr_node_id?;
    match github::fetch_files_viewed_state(repo, pr_node_id).await {
        Ok(viewed) => Some(viewed),
        Err(error) => {
            warn!("Failed to fetch viewed-state for PR files: {error}");
            None
        }
    }
}

async fn run_git_numstat(working_dir: Option<&str>) -> Result<String> {
    run_git_command(working_dir, &["diff", "--numstat", "HEAD"]).await
}

async fn run_git_name_status(working_dir: Option<&str>) -> Result<String> {
    run_git_command(working_dir, &["diff", "--name-status", "HEAD"]).await
}

/// Git の C-quoted パス文字列をデコードする。
///
/// `core.quotePath` が true（デフォルト）の場合、非 ASCII パスは
/// `"src/\343\201\202.rs"` のように C-quoted 形式で出力される。
/// この関数は引用符を除去し、オクタルエスケープを UTF-8 バイト列に変換する。
///
/// 引用符で囲まれていないパスはそのまま返す。
pub(crate) fn unquote_git_path(path: &str) -> Cow<'_, str> {
    // C-quoted 形式でなければそのまま返す。
    // len >= 2 の判定が必要: `"` 一文字は starts_with と ends_with の両方を
    // 同じ一文字で満たすので、下の [1..len-1] が [1..0] になり panic する。
    if path.len() < 2 || !path.starts_with('"') || !path.ends_with('"') {
        return Cow::Borrowed(path);
    }

    // 前後の引用符を除去
    let inner = &path[1..path.len() - 1];
    let mut bytes: Vec<u8> = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => bytes.push(b'\\'),
                Some('"') => bytes.push(b'"'),
                Some('n') => bytes.push(b'\n'),
                Some('t') => bytes.push(b'\t'),
                Some('a') => bytes.push(0x07), // bell
                Some('b') => bytes.push(0x08), // backspace
                Some('r') => bytes.push(b'\r'),
                Some('f') => bytes.push(0x0C), // form feed
                Some('v') => bytes.push(0x0B), // vertical tab
                Some(c) if c.is_ascii_digit() && c != '8' && c != '9' => {
                    // Octal escape: \ooo (1-3 digits)
                    let mut octal = String::with_capacity(3);
                    octal.push(c);
                    // Peek at next chars for octal digits
                    for _ in 0..2 {
                        // We need to peek, but chars doesn't support peek.
                        // Use a clone trick.
                        let mut peek = chars.clone();
                        if let Some(next) = peek.next() {
                            if next.is_ascii_digit() && next != '8' && next != '9' {
                                octal.push(next);
                                chars.next(); // consume
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    if let Ok(byte) = u8::from_str_radix(&octal, 8) {
                        bytes.push(byte);
                    }
                }
                Some(c) => {
                    // Unknown escape, keep as-is
                    bytes.push(b'\\');
                    let mut buf = [0u8; 4];
                    bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                None => bytes.push(b'\\'),
            }
        } else {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }

    Cow::Owned(String::from_utf8(bytes).unwrap_or_else(|e| {
        // Fallback: lossy conversion
        String::from_utf8_lossy(e.as_bytes()).into_owned()
    }))
}

/// name-status 出力をパース。rename/copy の3カラム形式にも対応:
///   "M\tsrc/foo.rs"           → ("src/foo.rs", "modified")
///   "A\tsrc/new.rs"           → ("src/new.rs", "added")
///   "D\tsrc/old.rs"           → ("src/old.rs", "removed")
///   "R100\told.rs\tnew.rs"    → ("new.rs", "renamed")
///   "C100\tsrc.rs\tdst.rs"    → ("dst.rs", "copied")
///
/// Git の C-quoted パス（非 ASCII ファイル名）も自動でデコードする。
fn parse_name_status_output(output: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();

    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let status_code = parts[0];
        let first_char = status_code.chars().next().unwrap_or(' ');

        match first_char {
            'R' | 'C' => {
                // Rename/Copy: 3カラム形式 "R100\told\tnew" or "C100\tsrc\tdst"
                if parts.len() >= 3 {
                    let new_name = unquote_git_path(parts[2]).into_owned();
                    let status = if first_char == 'R' {
                        "renamed"
                    } else {
                        "copied"
                    };
                    result.push((new_name, status.to_string()));
                }
            }
            'M' => {
                result.push((
                    unquote_git_path(parts[1]).into_owned(),
                    "modified".to_string(),
                ));
            }
            'A' => {
                result.push((unquote_git_path(parts[1]).into_owned(), "added".to_string()));
            }
            'D' => {
                result.push((
                    unquote_git_path(parts[1]).into_owned(),
                    "removed".to_string(),
                ));
            }
            'T' => {
                // Type change
                result.push((
                    unquote_git_path(parts[1]).into_owned(),
                    "modified".to_string(),
                ));
            }
            _ => {
                // Unknown status, treat as modified
                result.push((
                    unquote_git_path(parts[1]).into_owned(),
                    "modified".to_string(),
                ));
            }
        }
    }

    result
}

/// name-status + numstat から ChangedFile（patch: None）を構築
fn build_changed_files_lazy(
    name_status: &[(String, String)],
    numstat: &HashMap<String, (u32, u32)>,
) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = name_status
        .iter()
        .map(|(filename, status)| {
            let (additions, deletions) = numstat.get(filename).copied().unwrap_or((0, 0));
            ChangedFile {
                filename: filename.clone(),
                status: status.clone(),
                additions,
                deletions,
                patch: None,
                viewed: false,
            }
        })
        .collect();

    files.sort_unstable_by(|a, b| a.filename.cmp(&b.filename));
    files
}

/// untracked ファイルをリストのみ取得（patch: None）
async fn merge_untracked_files_lazy(working_dir: Option<&str>, files: &mut Vec<ChangedFile>) {
    let untracked_output = match run_git_untracked(working_dir).await {
        Ok(output) => output,
        Err(_) => return,
    };
    let names = parse_path_list(&untracked_output);
    let mut seen: HashSet<String> = files.iter().map(|file| file.filename.clone()).collect();
    let mut present = Vec::new();

    for filename in names {
        if !seen.insert(filename.clone()) {
            continue;
        }
        present.push(filename.clone());
        files.push(ChangedFile {
            filename,
            status: "added".to_string(),
            additions: 0,
            deletions: 0,
            patch: None,
            viewed: false,
        });
    }

    files.sort_unstable_by(|a, b| a.filename.cmp(&b.filename));
    // Drop cache entries for untracked paths that are gone or now tracked.
    crate::untracked_diff::retain(working_dir, &present);
}

async fn current_head_sha(working_dir: Option<&str>) -> Result<String> {
    run_git_command(working_dir, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string())
}

async fn run_git_command(working_dir: Option<&str>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    // Disable C-quoting of non-ASCII paths to get raw UTF-8 output
    command.args(["-c", "core.quotePath=false"]);
    command.args(args);

    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }

    let output = command
        .output()
        .await
        .context("failed to spawn git command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_numstat_output(output: Option<&str>) -> HashMap<String, (u32, u32)> {
    let mut result = HashMap::new();
    let Some(output) = output else {
        return result;
    };

    for line in output.lines() {
        let mut parts = line.split('\t');
        let added_raw = parts.next();
        let deleted_raw = parts.next();
        let path = parts.next_back();

        if let (Some(added_raw), Some(deleted_raw), Some(path)) = (added_raw, deleted_raw, path) {
            let parse_count = |value: &str| -> u32 { value.parse().unwrap_or(0) };
            result.insert(
                unquote_git_path(path).into_owned(),
                (parse_count(added_raw), parse_count(deleted_raw)),
            );
        }
    }

    result
}

fn parse_path_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| unquote_git_path(line.trim()).into_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

async fn run_git_diff_file(working_dir: Option<&str>, filename: &str) -> Result<String> {
    run_git_command(working_dir, &["diff", "HEAD", "--", filename]).await
}

async fn run_git_untracked(working_dir: Option<&str>) -> Result<String> {
    run_git_command(working_dir, &["ls-files", "--others", "--exclude-standard"]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    fn run_git(cmd: &mut Command, dir: &Path, args: &[&str], message: &str) {
        // The developer machine's global git config is not part of the fixture.
        // `commit.gpgsign` plus an SSH signer makes `git commit` exit 128 when
        // that signer cannot reach the agent.
        let output = cmd
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "octorus-test")
            .env("GIT_AUTHOR_EMAIL", "octorus-test@example.com")
            .env("GIT_COMMITTER_NAME", "octorus-test")
            .env("GIT_COMMITTER_EMAIL", "octorus-test@example.com")
            .output()
            .expect(message);

        assert!(
            output.status.success(),
            "{message}: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn write_file(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn test_fetch_local_diff_detects_subdir_changes_and_skips_ignored_files() {
        let tempdir = tempdir().unwrap();
        let workdir = tempdir.path();

        run_git(
            &mut Command::new("git"),
            workdir,
            &["init", "-b", "main"],
            "failed to initialize temp git repo",
        );
        write_file(&workdir.join(".gitignore"), "ignored/\n");
        write_file(&workdir.join("src/main.rs"), "fn main() {}\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", ".gitignore", "src/main.rs"],
            "failed to add initial files",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "initial commit"],
            "failed to create initial commit",
        );

        write_file(
            &workdir.join("src/main.rs"),
            "fn main() { println!(\"hello\"); }\n",
        );
        write_file(
            &workdir.join("feature/new_file.rs"),
            "pub fn feature() {}\n",
        );
        write_file(
            &workdir.join("ignored/skip.txt"),
            "this file should stay ignored",
        );

        let (tx, mut rx) = mpsc::channel::<DataLoadResult>(1);
        fetch_local_diff(
            "local".to_string(),
            Some(workdir.to_string_lossy().to_string()),
            tx,
        )
        .await;

        let result = rx.recv().await.unwrap();
        let files = match result {
            DataLoadResult::Success { files, .. } => files,
            DataLoadResult::Unchanged { .. } => {
                panic!("local diff reported an update check")
            }
            DataLoadResult::Error(err) => panic!("unexpected error: {err}"),
        };

        let filenames: Vec<_> = files.iter().map(|file| file.filename.as_str()).collect();

        assert!(filenames.contains(&"src/main.rs"));
        assert!(filenames.contains(&"feature/new_file.rs"));
        assert!(!filenames.contains(&"ignored/skip.txt"));
    }

    #[tokio::test]
    async fn test_fetch_local_diff_does_not_return_non_target_ignored_file() {
        let tempdir = tempdir().unwrap();
        let workdir = tempdir.path();

        run_git(
            &mut Command::new("git"),
            workdir,
            &["init", "-b", "main"],
            "failed to initialize temp git repo",
        );
        write_file(&workdir.join(".gitignore"), "ignored/\n");
        write_file(&workdir.join("README.md"), "# octorus\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", ".gitignore", "README.md"],
            "failed to add initial files",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "initial commit"],
            "failed to create initial commit",
        );

        write_file(&workdir.join("ignored/build.tmp"), "ignore\n");

        let (tx, mut rx) = mpsc::channel::<DataLoadResult>(1);
        fetch_local_diff(
            "local".to_string(),
            Some(workdir.to_string_lossy().to_string()),
            tx,
        )
        .await;

        let result = rx.recv().await.unwrap();
        let files = match result {
            DataLoadResult::Success { files, .. } => files,
            DataLoadResult::Unchanged { .. } => {
                panic!("local diff reported an update check")
            }
            DataLoadResult::Error(err) => panic!("unexpected error: {err}"),
        };

        assert!(files.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_local_diff_for_untracked_file_returns_lazy() {
        let tempdir = tempdir().unwrap();
        let workdir = tempdir.path();

        run_git(
            &mut Command::new("git"),
            workdir,
            &["init", "-b", "main"],
            "failed to initialize temp git repo",
        );
        write_file(&workdir.join("README.md"), "hello\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "README.md"],
            "failed to add initial file",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "initial commit"],
            "failed to commit initial file",
        );

        write_file(
            &workdir.join("src/new_feature.rs"),
            "pub fn hello() {\n    1 + 1\n}\n",
        );

        let (tx, mut rx) = mpsc::channel::<DataLoadResult>(1);
        fetch_local_diff(
            "local".to_string(),
            Some(workdir.to_string_lossy().to_string()),
            tx,
        )
        .await;

        let result = rx.recv().await.unwrap();
        let files = match result {
            DataLoadResult::Success { files, .. } => files,
            DataLoadResult::Unchanged { .. } => {
                panic!("local diff reported an update check")
            }
            DataLoadResult::Error(err) => panic!("unexpected error: {err}"),
        };

        let new_file = files
            .iter()
            .find(|file| file.filename == "src/new_feature.rs")
            .expect("untracked file should appear in local diff");

        // 2段階ロードでは patch は None（バッチロードで後から取得）
        assert!(new_file.patch.is_none());
        assert_eq!(new_file.status, "added");
    }

    #[tokio::test]
    async fn test_fetch_single_file_diff_for_tracked_file() {
        let tempdir = tempdir().unwrap();
        let workdir = tempdir.path();

        run_git(
            &mut Command::new("git"),
            workdir,
            &["init", "-b", "main"],
            "failed to initialize temp git repo",
        );
        write_file(&workdir.join("src/main.rs"), "fn main() {}\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "src/main.rs"],
            "failed to add initial file",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "initial commit"],
            "failed to create initial commit",
        );

        write_file(
            &workdir.join("src/main.rs"),
            "fn main() { println!(\"hello\"); }\n",
        );

        let (tx, mut rx) = mpsc::channel::<SingleFileDiffResult>(1);
        fetch_single_file_diff(
            Some(workdir.to_string_lossy().to_string()),
            "src/main.rs".to_string(),
            false,
            tx,
        )
        .await;

        let result = rx.recv().await.unwrap();
        assert_eq!(result.filename, "src/main.rs");
        let patch = result.patch.expect("tracked file should have a patch");
        assert!(patch.contains("+fn main() { println!(\"hello\"); }"));
    }

    #[tokio::test]
    async fn test_fetch_single_file_diff_for_untracked_file() {
        let tempdir = tempdir().unwrap();
        let workdir = tempdir.path();

        run_git(
            &mut Command::new("git"),
            workdir,
            &["init", "-b", "main"],
            "failed to initialize temp git repo",
        );
        write_file(&workdir.join("README.md"), "hello\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "README.md"],
            "failed to add initial file",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "initial commit"],
            "failed to create initial commit",
        );

        write_file(
            &workdir.join("src/new_feature.rs"),
            "pub fn hello() {\n    1 + 1\n}\n",
        );

        let (tx, mut rx) = mpsc::channel::<SingleFileDiffResult>(1);
        fetch_single_file_diff(
            Some(workdir.to_string_lossy().to_string()),
            "src/new_feature.rs".to_string(),
            true,
            tx,
        )
        .await;

        let result = rx.recv().await.unwrap();
        assert_eq!(result.filename, "src/new_feature.rs");
        let patch = result.patch.expect("untracked file should have a patch");
        assert!(patch.contains("+pub fn hello()"));
    }

    #[test]
    fn test_parse_name_status_output() {
        let output = "M\tsrc/foo.rs\nA\tsrc/new.rs\nD\tsrc/old.rs\nR100\told.rs\tnew.rs\nC100\tsrc.rs\tdst.rs\n";
        let result = parse_name_status_output(output);

        assert_eq!(result.len(), 5);
        assert_eq!(
            result[0],
            ("src/foo.rs".to_string(), "modified".to_string())
        );
        assert_eq!(result[1], ("src/new.rs".to_string(), "added".to_string()));
        assert_eq!(result[2], ("src/old.rs".to_string(), "removed".to_string()));
        assert_eq!(result[3], ("new.rs".to_string(), "renamed".to_string()));
        assert_eq!(result[4], ("dst.rs".to_string(), "copied".to_string()));
    }

    #[test]
    fn test_parse_name_status_output_empty() {
        let result = parse_name_status_output("");
        assert!(result.is_empty());
    }

    /// parse_unified_diff のキーと parse_name_status_output のファイル名が
    /// rename ファイルでも一致することを検証する。
    /// これが失敗すると fetch_local_diffs_batched の patches.remove(filename) が
    /// None を返し、rename ファイルの diff が取得できなくなる。
    #[test]
    fn test_parse_unified_diff_filename_matches_name_status_for_rename() {
        let name_status_output = "R100\tsrc/old_name.rs\tsrc/new_name.rs\n";
        let name_status = parse_name_status_output(name_status_output);
        assert_eq!(name_status[0].0, "src/new_name.rs");

        let unified_diff = "\
diff --git a/src/old_name.rs b/src/new_name.rs
similarity index 95%
rename from src/old_name.rs
rename to src/new_name.rs
index 1234567..abcdefg 100644
--- a/src/old_name.rs
+++ b/src/new_name.rs
@@ -1,3 +1,3 @@
-fn old_name() {
+fn new_name() {
 }";
        let mut patches = diff::parse_unified_diff(unified_diff);

        // name_status のファイル名で patch を取得できること
        let patch = patches.remove(&name_status[0].0);
        assert!(
            patch.is_some(),
            "parse_unified_diff key must match parse_name_status_output filename for renamed files"
        );
    }

    /// parse_unified_diff のキーと parse_name_status_output のファイル名が
    /// 通常の変更ファイルで一致することを検証する。
    #[test]
    fn test_parse_unified_diff_filename_matches_name_status_for_modified() {
        let name_status_output = "M\tsrc/main.rs\n";
        let name_status = parse_name_status_output(name_status_output);
        assert_eq!(name_status[0].0, "src/main.rs");

        let unified_diff = "\
diff --git a/src/main.rs b/src/main.rs
index 1234567..abcdefg 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!(\"Hello\");
 }";
        let mut patches = diff::parse_unified_diff(unified_diff);

        let patch = patches.remove(&name_status[0].0);
        assert!(
            patch.is_some(),
            "parse_unified_diff key must match parse_name_status_output filename for modified files"
        );
    }

    #[tokio::test]
    async fn test_fetch_local_diffs_batched_handles_renamed_file() {
        let tempdir = tempdir().unwrap();
        let workdir = tempdir.path();

        run_git(
            &mut Command::new("git"),
            workdir,
            &["init", "-b", "main"],
            "failed to initialize temp git repo",
        );
        write_file(&workdir.join("src/old_name.rs"), "fn old_name() {}\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "src/old_name.rs"],
            "failed to add initial file",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "initial commit"],
            "failed to create initial commit",
        );

        // git mv でリネーム + 内容変更
        run_git(
            &mut Command::new("git"),
            workdir,
            &["mv", "src/old_name.rs", "src/new_name.rs"],
            "failed to rename file",
        );
        write_file(&workdir.join("src/new_name.rs"), "fn new_name() {}\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "src/new_name.rs"],
            "failed to stage renamed file",
        );

        // バッチ diff で新ファイル名を使って patch が取得できることを検証
        let (tx, mut rx) = mpsc::channel::<Vec<SingleFileDiffResult>>(2);
        fetch_local_diffs_batched(
            Some(workdir.to_string_lossy().to_string()),
            vec!["src/new_name.rs".to_string()],
            vec![],
            20,
            tx,
        )
        .await;

        let results = rx.recv().await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].filename, "src/new_name.rs");
        assert!(
            results[0].patch.is_some(),
            "renamed file must have a patch when queried with new filename"
        );
    }

    #[test]
    fn test_build_changed_files_lazy() {
        let name_status = vec![
            ("src/foo.rs".to_string(), "modified".to_string()),
            ("src/new.rs".to_string(), "added".to_string()),
        ];
        let mut numstat = HashMap::new();
        numstat.insert("src/foo.rs".to_string(), (3u32, 1u32));
        numstat.insert("src/new.rs".to_string(), (10u32, 0u32));

        let files = build_changed_files_lazy(&name_status, &numstat);

        assert_eq!(files.len(), 2);
        // ソートされるので foo が先
        assert_eq!(files[0].filename, "src/foo.rs");
        assert_eq!(files[0].status, "modified");
        assert_eq!(files[0].additions, 3);
        assert_eq!(files[0].deletions, 1);
        assert!(files[0].patch.is_none());

        assert_eq!(files[1].filename, "src/new.rs");
        assert_eq!(files[1].status, "added");
        assert_eq!(files[1].additions, 10);
        assert_eq!(files[1].deletions, 0);
        assert!(files[1].patch.is_none());
    }

    #[test]
    fn test_unquote_git_path_plain() {
        assert_eq!(unquote_git_path("src/foo.rs"), "src/foo.rs");
    }

    #[test]
    fn test_unquote_git_path_plain_is_borrowed() {
        assert!(matches!(
            unquote_git_path("src/foo.rs"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn test_unquote_git_path_degenerate_quotes_do_not_panic() {
        // A single `"` satisfies both starts_with and ends_with with the same
        // character. Slicing [1..len-1] would be [1..0].
        assert_eq!(unquote_git_path("\""), "\"");
        assert_eq!(unquote_git_path(""), "");
        assert_eq!(unquote_git_path("\"\""), "");
        // A lone quote at either end is not a quoted path.
        assert_eq!(unquote_git_path("\"unterminated"), "\"unterminated");
        assert_eq!(unquote_git_path("unopened\""), "unopened\"");
    }

    #[test]
    fn test_unquote_git_path_cquoted_non_ascii() {
        // \343\201\202 = UTF-8 encoding of 'あ' (U+3042)
        assert_eq!(unquote_git_path(r#""src/\343\201\202.rs""#), "src/あ.rs");
    }

    #[test]
    fn test_unquote_git_path_cquoted_backslash_and_quote() {
        assert_eq!(unquote_git_path(r#""path\\to\"file""#), "path\\to\"file");
    }

    #[test]
    fn test_unquote_git_path_cquoted_special_escapes() {
        assert_eq!(unquote_git_path(r#""a\tb\nc""#), "a\tb\nc");
    }

    #[test]
    fn test_unquote_git_path_cquoted_multibyte_sequence() {
        // \346\227\245\346\234\254\350\252\236 = UTF-8 encoding of '日本語'
        assert_eq!(
            unquote_git_path(r#""src/\346\227\245\346\234\254\350\252\236.txt""#),
            "src/日本語.txt"
        );
    }

    #[test]
    fn test_parse_name_status_output_cquoted_paths() {
        // Git C-quotes non-ASCII filenames by default (core.quotePath=true)
        // In real git output, the tab-separated line looks like:
        //   M\t"src/\343\201\202.rs"
        // We construct this with format! to avoid Rust escape conflicts
        let output = format!("M\t{}\n", r#""src/\343\201\202.rs""#);
        let result = parse_name_status_output(&output);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "src/あ.rs");
        assert_eq!(result[0].1, "modified");
    }

    #[test]
    fn test_parse_numstat_output_cquoted_paths() {
        let output = format!("3\t1\t{}\n", r#""src/\343\201\202.rs""#);
        let result = parse_numstat_output(Some(&output));

        assert_eq!(result.len(), 1);
        assert!(
            result.contains_key("src/あ.rs"),
            "numstat should decode quoted paths"
        );
        assert_eq!(result["src/あ.rs"], (3, 1));
    }

    #[test]
    fn test_parse_path_list_cquoted_paths() {
        let output = format!("{}\nsrc/plain.rs\n", r#""src/\343\201\202.rs""#);
        let result = parse_path_list(&output);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "src/あ.rs");
        assert_eq!(result[1], "src/plain.rs");
    }

    #[tokio::test]
    async fn test_fetch_local_diffs_batched_non_ascii_filename() {
        let tempdir = tempdir().unwrap();
        let workdir = tempdir.path();

        run_git(
            &mut Command::new("git"),
            workdir,
            &["init", "-b", "main"],
            "failed to initialize temp git repo",
        );
        write_file(&workdir.join("README.md"), "hello\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "README.md"],
            "failed to add initial file",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "initial commit"],
            "failed to commit initial file",
        );

        // Create a non-ASCII tracked file, commit, then modify
        let non_ascii_path = workdir.join("src/日本語.rs");
        write_file(&non_ascii_path, "fn original() {}\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "src/日本語.rs"],
            "failed to add non-ASCII file",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "add non-ASCII file"],
            "failed to commit non-ASCII file",
        );

        // Modify the file so it appears in diff
        write_file(&non_ascii_path, "fn modified() {}\n");

        // First, verify fetch_local_diff returns the decoded filename
        let (tx, mut rx) = mpsc::channel::<DataLoadResult>(1);
        fetch_local_diff(
            "local".to_string(),
            Some(workdir.to_string_lossy().to_string()),
            tx,
        )
        .await;

        let result = rx.recv().await.unwrap();
        let files = match result {
            DataLoadResult::Success { files, .. } => files,
            DataLoadResult::Unchanged { .. } => {
                panic!("local diff reported an update check")
            }
            DataLoadResult::Error(err) => panic!("unexpected error: {err}"),
        };

        let non_ascii_file = files
            .iter()
            .find(|f| f.filename == "src/日本語.rs")
            .expect("non-ASCII filename should be decoded from C-quoted format");
        assert_eq!(non_ascii_file.status, "modified");

        // Now verify batched diff can retrieve the patch using decoded filename
        let (tx2, mut rx2) = mpsc::channel::<Vec<SingleFileDiffResult>>(2);
        fetch_local_diffs_batched(
            Some(workdir.to_string_lossy().to_string()),
            vec!["src/日本語.rs".to_string()],
            vec![],
            20,
            tx2,
        )
        .await;

        let results = rx2.recv().await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].filename, "src/日本語.rs");
        assert!(
            results[0].patch.is_some(),
            "batched diff must retrieve patch for non-ASCII filename"
        );
    }

    #[tokio::test]
    async fn test_fetch_single_file_diff_non_ascii_filename() {
        let tempdir = tempdir().unwrap();
        let workdir = tempdir.path();

        run_git(
            &mut Command::new("git"),
            workdir,
            &["init", "-b", "main"],
            "failed to initialize temp git repo",
        );
        write_file(&workdir.join("README.md"), "hello\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "README.md"],
            "failed to add initial file",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "initial commit"],
            "failed to commit initial file",
        );

        // Create and commit a non-ASCII tracked file, then modify
        let non_ascii_path = workdir.join("src/テスト.rs");
        write_file(&non_ascii_path, "fn original() {}\n");
        run_git(
            &mut Command::new("git"),
            workdir,
            &["add", "src/テスト.rs"],
            "failed to add non-ASCII file",
        );
        run_git(
            &mut Command::new("git"),
            workdir,
            &["commit", "-m", "add non-ASCII file"],
            "failed to commit non-ASCII file",
        );

        write_file(&non_ascii_path, "fn modified() {}\n");

        let (tx, mut rx) = mpsc::channel::<SingleFileDiffResult>(1);
        fetch_single_file_diff(
            Some(workdir.to_string_lossy().to_string()),
            "src/テスト.rs".to_string(),
            false,
            tx,
        )
        .await;

        let result = rx.recv().await.unwrap();
        assert_eq!(result.filename, "src/テスト.rs");
        assert!(
            result.patch.is_some(),
            "single file diff must retrieve patch for non-ASCII filename"
        );
        let patch = result.patch.unwrap();
        assert!(patch.contains("+fn modified()"));
    }
}

/// `gh` call counts for a cache-hit reopen. The test double logs every call.
#[cfg(all(test, unix))]
mod check_update_calls {
    use super::*;
    use crate::github::fake_gh::FakeGh;
    use serial_test::serial;
    use tokio::sync::mpsc;

    const UPDATED_AT: &str = "2024-01-01T00:00:00Z";
    const NEW_UPDATED_AT: &str = "2024-06-01T00:00:00Z";
    const PATCH: &str = "@@ -1 +1 @@\n-old\n+new\n";
    const BASE_TIP: &str = "cafebabe";

    fn pr_json(updated_at: &str) -> String {
        pr_json_parts(updated_at, "Title", "abc", "main", "def", Some(BASE_TIP))
    }

    fn pr_json_parts(
        updated_at: &str,
        title: &str,
        head_sha: &str,
        base_ref: &str,
        base_sha: &str,
        base_tip: Option<&str>,
    ) -> String {
        let base_ref_value = match base_tip {
            Some(oid) => serde_json::json!({ "target": { "oid": oid } }),
            None => serde_json::Value::Null,
        };
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "id": "PR_node",
                        "number": 7,
                        "title": title,
                        "body": "body",
                        "state": "OPEN",
                        "updatedAt": updated_at,
                        "headRefName": "feature",
                        "headRefOid": head_sha,
                        "baseRefName": base_ref,
                        "baseRefOid": base_sha,
                        "author": { "login": "octocat" },
                        "baseRef": base_ref_value,
                    }
                }
            }
        })
        .to_string()
    }

    /// The metadata read is GraphQL. `FAKE_GH_CREATE` is left empty so a REST
    /// pull that leaked in cannot satisfy it.
    fn open_gh(files: &str, metadata: &str, viewed: &str, publish: &str) -> FakeGh {
        let gh = FakeGh::install(files, "", viewed, publish);
        gh.set_pr_check(metadata);
        gh
    }

    fn cached_check(updated_at: &str) -> crate::app::CachedPrCheck {
        crate::app::CachedPrCheck {
            updated_at: updated_at.to_string(),
            head_sha: "abc".to_string(),
            base_ref: "main".to_string(),
            base_sha: "def".to_string(),
            base_tip_sha: Some(BASE_TIP.to_string()),
        }
    }

    fn files_json(patch: Option<&str>) -> String {
        serde_json::json!([[{
            "filename": "src/main.rs",
            "status": "modified",
            "additions": 1,
            "deletions": 1,
            "patch": patch,
        }]])
        .to_string()
    }

    fn viewed_json(state: &str) -> String {
        serde_json::json!({
            "data": {
                "node": {
                    "files": {
                        "nodes": [{ "path": "src/main.rs", "viewerViewedState": state }],
                        "pageInfo": { "hasNextPage": false, "endCursor": null }
                    }
                }
            }
        })
        .to_string()
    }

    fn calls_of(gh: &FakeGh) -> Vec<String> {
        gh.calls()
            .lines()
            .filter(|line| line.starts_with("CALL ") && call_is_for_pr(line))
            .map(str::to_string)
            .collect()
    }

    /// The fake `gh` is process-wide, so `pulls/1` and `issues/1` from other
    /// tests can land in this log. Count only this module's PR 7.
    fn call_is_for_pr(line: &str) -> bool {
        contains_pr_token(line, "/pulls/7")
            || contains_pr_token(line, "pr diff 7")
            || contains_pr_token(line, "pr checks 7")
            || contains_pr_token(line, "number=7")
            || line.contains("pullRequestId=PR_node")
    }

    fn contains_pr_token(line: &str, token: &str) -> bool {
        let mut rest = line;
        while let Some(index) = rest.find(token) {
            let after = index + token.len();
            let boundary = match rest[after..].chars().next() {
                Some(ch) => !ch.is_ascii_digit(),
                None => true,
            };
            if boundary {
                return true;
            }
            rest = &rest[index + 1..];
        }
        false
    }

    /// `(metadata, file list, viewed state, full diff)`
    fn classify(calls: &[String]) -> (usize, usize, usize, usize) {
        let mut metadata = 0;
        let mut files = 0;
        let mut viewed = 0;
        let mut diff = 0;
        for call in calls {
            if call.contains("pr diff") {
                diff += 1;
            } else if call.contains("viewerViewedState") {
                viewed += 1;
            } else if call.contains("--slurp") || call.contains("/files?") {
                files += 1;
            } else {
                metadata += 1;
            }
        }
        (metadata, files, viewed, diff)
    }

    #[test]
    fn call_filter_ignores_other_pull_requests() {
        assert!(call_is_for_pr("CALL api repos/o/r/pulls/7"));
        assert!(call_is_for_pr(
            "CALL api repos/o/r/pulls/7/files?per_page=100"
        ));
        assert!(!call_is_for_pr("CALL api repos/o/r/pulls/1"));
        assert!(!call_is_for_pr("CALL api repos/o/r/pulls/70"));
        assert!(!call_is_for_pr("CALL api repos/o/r/issues/1"));
        assert!(call_is_for_pr("CALL pr diff 7 -R owner/repo"));
        assert!(!call_is_for_pr("CALL pr diff 70 -R owner/repo"));
        assert!(call_is_for_pr(
            "CALL api graphql -f query=query{baseRef{target{oid}}} -F number=7"
        ));
        assert!(!call_is_for_pr(
            "CALL api graphql -f query=query{baseRef{target{oid}}} -F number=1"
        ));
        assert!(call_is_for_pr(
            "CALL api graphql -f query=query{viewerViewedState} -f pullRequestId=PR_node"
        ));
    }

    #[tokio::test]
    #[serial]
    async fn fresh_fetch_loads_metadata_files_and_viewed_state() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json(UPDATED_AT),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data("owner/repo".to_string(), 7, FetchMode::Fresh, tx).await;
        let DataLoadResult::Success {
            pr,
            files,
            base_tip_sha,
        } = rx.recv().await.unwrap()
        else {
            panic!("fresh fetch succeeds");
        };
        assert_eq!(pr.updated_at, UPDATED_AT);
        assert_eq!(pr.state, "open");
        assert_eq!(base_tip_sha.as_deref(), Some(BASE_TIP));
        assert_eq!(files[0].patch.as_deref(), Some(PATCH));
        assert!(files[0].viewed);

        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (1, 1, 1, 0),
            "fresh reopen calls: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn fresh_fetch_requests_the_full_diff_when_a_patch_is_missing() {
        let gh = open_gh(
            &files_json(None),
            &pr_json(UPDATED_AT),
            &viewed_json("UNVIEWED"),
            "",
        );
        gh.set_diff("diff --git a/src/main.rs b/src/main.rs\n@@ -1 +1 @@\n-old\n+recovered\n");
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data("owner/repo".to_string(), 7, FetchMode::Fresh, tx).await;
        let DataLoadResult::Success { files, .. } = rx.recv().await.unwrap() else {
            panic!("fresh fetch succeeds");
        };
        let patch = files[0].patch.as_deref().expect("gh pr diff fills the gap");
        assert!(
            patch.contains("+recovered"),
            "the recovered patch is the diff body, got {patch}"
        );
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (1, 1, 1, 1),
            "a missing patch is metadata, files, viewed, and gh pr diff: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_failed_diff_keeps_the_file_and_leaves_the_patch_missing() {
        let gh = open_gh(
            &files_json(None),
            &pr_json(UPDATED_AT),
            &viewed_json("UNVIEWED"),
            "",
        );
        gh.fail_diff();
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data("owner/repo".to_string(), 7, FetchMode::Fresh, tx).await;
        let DataLoadResult::Success { files, .. } = rx.recv().await.unwrap() else {
            panic!("a diff failure does not fail the file list");
        };
        assert!(files[0].patch.is_none());
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (1, 1, 1, 1),
            "the diff is still attempted: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn unchanged_updated_at_skips_files_and_patches_but_refreshes_viewed_state() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json(UPDATED_AT),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Unchanged {
            pr_number,
            viewed,
            pr,
        } = rx.recv().await.unwrap()
        else {
            panic!("matching updated_at does not replace the file list");
        };
        assert_eq!(pr_number, 7);
        assert_eq!(pr.expect("the probe is applied").title, "Title");
        let viewed = viewed.expect("viewed state is refreshed");
        assert_eq!(viewed.get("src/main.rs"), Some(&true));

        let calls = calls_of(&gh);
        assert_eq!(
            calls.len(),
            2,
            "an unchanged reopen is two gh calls: {calls:?}"
        );
        assert_eq!(
            classify(&calls),
            (1, 0, 1, 0),
            "unchanged reopen skips the file list and gh pr diff: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn changed_updated_at_does_the_full_fetch() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json(NEW_UPDATED_AT),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Success { pr, files, .. } = rx.recv().await.unwrap() else {
            panic!("a changed timestamp replaces the cached payload");
        };
        assert_eq!(pr.updated_at, NEW_UPDATED_AT);
        assert_eq!(files[0].patch.as_deref(), Some(PATCH));
        assert!(files[0].viewed);

        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (2, 1, 1, 0),
            "changed reopen is the updated_at probe plus a full fetch: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn updated_at_check_failure_falls_back_to_the_full_fetch() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json(NEW_UPDATED_AT),
            &viewed_json("UNVIEWED"),
            "",
        );
        gh.fail_first(1);
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Success { pr, files, .. } = rx.recv().await.unwrap() else {
            panic!("a failed updated_at read still loads the PR");
        };
        assert_eq!(pr.updated_at, NEW_UPDATED_AT);
        assert_eq!(files[0].patch.as_deref(), Some(PATCH));
        assert!(!files[0].viewed);

        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (2, 1, 1, 0),
            "failed probe is followed by a full fetch: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn viewed_state_failure_keeps_the_unchanged_result() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json(UPDATED_AT),
            r#"{"errors":[{"message":"nope"}]}"#,
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Unchanged { viewed, .. } = rx.recv().await.unwrap() else {
            panic!("a failed viewed-state read does not refetch files");
        };
        assert!(viewed.is_none());
        let calls = calls_of(&gh);
        assert_eq!(classify(&calls), (1, 0, 1, 0), "{calls:?}");
    }

    #[tokio::test]
    #[serial]
    async fn missing_node_id_skips_the_viewed_state_query() {
        let pr = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "id": null,
                        "number": 7,
                        "title": "Title",
                        "body": null,
                        "state": "OPEN",
                        "updatedAt": UPDATED_AT,
                        "headRefName": "feature",
                        "headRefOid": "abc",
                        "baseRefName": "main",
                        "baseRefOid": "def",
                        "author": { "login": "octocat" },
                        "baseRef": { "target": { "oid": BASE_TIP } }
                    }
                }
            }
        })
        .to_string();
        let gh = open_gh(&files_json(Some(PATCH)), &pr, &viewed_json("VIEWED"), "");
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Unchanged { viewed, .. } = rx.recv().await.unwrap() else {
            panic!("matching updated_at still skips the file list");
        };
        assert!(viewed.is_none());
        let calls = calls_of(&gh);
        assert_eq!(classify(&calls), (1, 0, 0, 0), "{calls:?}");
    }

    #[tokio::test]
    #[serial]
    async fn same_timestamp_with_a_new_title_keeps_the_file_list_and_returns_the_title() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json_parts(UPDATED_AT, "Renamed", "abc", "main", "def", Some(BASE_TIP)),
            &viewed_json("UNVIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Unchanged { pr, .. } = rx.recv().await.unwrap() else {
            panic!("a title change with the same commits does not refetch files");
        };
        assert_eq!(pr.expect("fresh metadata").title, "Renamed");
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (1, 0, 1, 0),
            "title comes from the probe: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn same_timestamp_with_a_new_head_sha_does_the_full_fetch() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json_parts(UPDATED_AT, "Title", "fff", "main", "def", Some(BASE_TIP)),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Success { pr, files, .. } = rx.recv().await.unwrap() else {
            panic!("a new head with the same updated_at replaces the file list");
        };
        assert_eq!(pr.head.sha, "fff");
        assert_eq!(files[0].patch.as_deref(), Some(PATCH));
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (2, 1, 1, 0),
            "head mismatch is a full fetch: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn same_timestamp_with_a_new_base_ref_does_the_full_fetch() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json_parts(UPDATED_AT, "Title", "abc", "develop", "def", Some(BASE_TIP)),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Success { pr, .. } = rx.recv().await.unwrap() else {
            panic!("a new base ref with the same updated_at replaces the file list");
        };
        assert_eq!(pr.base.ref_name, "develop");
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (2, 1, 1, 0),
            "base ref mismatch is a full fetch: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn same_timestamp_with_a_new_base_sha_does_the_full_fetch() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json_parts(UPDATED_AT, "Title", "abc", "main", "fff", Some(BASE_TIP)),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Success { pr, .. } = rx.recv().await.unwrap() else {
            panic!("a new base sha with the same updated_at replaces the file list");
        };
        assert_eq!(pr.base.sha, "fff");
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (2, 1, 1, 0),
            "base sha mismatch is a full fetch: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn same_base_tip_stays_on_the_light_path() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json_parts(UPDATED_AT, "Title", "abc", "main", "def", Some(BASE_TIP)),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        assert!(
            matches!(rx.recv().await.unwrap(), DataLoadResult::Unchanged { .. }),
            "the same base tip keeps the cached file list"
        );
        let calls = calls_of(&gh);
        assert_eq!(
            calls.len(),
            2,
            "an unchanged reopen is two gh calls: {calls:?}"
        );
        assert_eq!(classify(&calls), (1, 0, 1, 0), "{calls:?}");
    }

    #[tokio::test]
    #[serial]
    async fn moved_base_tip_with_unchanged_commits_does_the_full_fetch() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json_parts(UPDATED_AT, "Title", "abc", "main", "def", Some("deadbeef")),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Success {
            pr,
            files,
            base_tip_sha,
        } = rx.recv().await.unwrap()
        else {
            panic!("a moved base tip replaces the file list");
        };
        assert_eq!(pr.updated_at, UPDATED_AT);
        assert_eq!(pr.head.sha, "abc");
        assert_eq!(pr.base.sha, "def");
        assert_eq!(base_tip_sha.as_deref(), Some("deadbeef"));
        assert_eq!(files[0].patch.as_deref(), Some(PATCH));
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (2, 1, 1, 0),
            "base tip mismatch is a full fetch: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn missing_base_ref_does_the_full_fetch() {
        let gh = open_gh(
            &files_json(Some(PATCH)),
            &pr_json_parts(UPDATED_AT, "Title", "abc", "main", "def", None),
            &viewed_json("VIEWED"),
            "",
        );
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data(
            "owner/repo".to_string(),
            7,
            FetchMode::CheckUpdate(cached_check(UPDATED_AT)),
            tx,
        )
        .await;
        let DataLoadResult::Success { base_tip_sha, .. } = rx.recv().await.unwrap() else {
            panic!("a missing base ref replaces the file list");
        };
        assert!(base_tip_sha.is_none());
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (2, 1, 1, 0),
            "a missing base ref is a full fetch: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn metadata_graphql_failure_still_loads_the_pull_request() {
        let rest = serde_json::json!({
            "number": 7,
            "node_id": "PR_node",
            "title": "From REST",
            "body": "body",
            "state": "open",
            "updated_at": UPDATED_AT,
            "head": {"ref": "feature", "sha": "abc"},
            "base": {"ref": "main", "sha": "def"},
            "user": {"login": "octocat"}
        })
        .to_string();
        let gh = FakeGh::install(&files_json(Some(PATCH)), &rest, &viewed_json("VIEWED"), "");
        gh.set_pr_check("{");
        let (tx, mut rx) = mpsc::channel(1);
        fetch_pr_data("owner/repo".to_string(), 7, FetchMode::Fresh, tx).await;
        let DataLoadResult::Success {
            pr, base_tip_sha, ..
        } = rx.recv().await.unwrap()
        else {
            panic!("a failed base-tip read still loads the pull request");
        };
        assert_eq!(pr.title, "From REST");
        assert!(base_tip_sha.is_none());
        let calls = calls_of(&gh);
        assert_eq!(
            classify(&calls),
            (2, 1, 1, 0),
            "the REST pull request replaces the failed metadata read: {calls:?}"
        );
    }
}
