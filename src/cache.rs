use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use xdg::BaseDirectories;

use crate::github::comment::{DiscussionComment, ReviewComment};
use crate::github::{ChangedFile, PullRequest};

/// セッションキャッシュが保持するPRデータの最大エントリ数。
/// 超過時は最も古いエントリ（LRU）を削除してメモリ増加を防止する。
const MAX_PR_CACHE_ENTRIES: usize = 5;

/// Sanitize repository name to prevent path traversal attacks.
/// Only allows alphanumeric characters, underscores, hyphens, and single dots (not ".." sequences).
/// Returns a sanitized string with '/' replaced by '_'.
pub fn sanitize_repo_name(repo: &str) -> Result<String> {
    // Check for path traversal patterns
    if repo.contains("..") || repo.starts_with('/') || repo.starts_with('\\') {
        return Err(anyhow::anyhow!(
            "Invalid repository name: contains path traversal pattern"
        ));
    }

    // Replace forward slash with underscore (for owner/repo format)
    let sanitized = repo.replace('/', "_");

    // Validate that the result contains only safe characters
    // Allow: alphanumeric, underscore, hyphen, single dot (for names like "foo.js")
    for c in sanitized.chars() {
        if !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.' {
            return Err(anyhow::anyhow!(
                "Invalid repository name: contains invalid character '{}'",
                c
            ));
        }
    }

    // Ensure it doesn't start with a dot (hidden file/directory)
    if sanitized.starts_with('.') {
        return Err(anyhow::anyhow!(
            "Invalid repository name: cannot start with a dot"
        ));
    }

    Ok(sanitized)
}

/// キャッシュディレクトリ: ~/.cache/octorus/
/// AI Rally セッション等で使用
pub fn cache_dir() -> PathBuf {
    BaseDirectories::with_prefix("octorus")
        .map(|dirs| dirs.get_cache_home())
        .unwrap_or_else(|_| PathBuf::from(".cache"))
}

/// Rally セッションディレクトリ内のデータをクリーンアップ
pub fn cleanup_rally_sessions() {
    let rally_dir = cache_dir().join("rally");
    if !rally_dir.exists() {
        return;
    }
    let entries = match std::fs::read_dir(&rally_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

const LOCAL_REVIEW_COMMENTS_VERSION: u32 = 1;

/// Local-only state attached to a review comment. Lives outside [`ReviewComment`]
/// because GitHub never returns these fields and forcing them onto the API type
/// pollutes every construction site.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCommentMeta {
    #[serde(default)]
    pub is_resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
}

/// On-disk representation of a local review comment: the GitHub-shaped
/// [`ReviewComment`] plus locally-tracked [`LocalCommentMeta`] flattened into the
/// same JSON object. Backwards-compatible with the v1 file format that stored
/// `is_resolved` / `resolved_at` directly on the comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalReviewComment {
    #[serde(flatten)]
    pub comment: ReviewComment,
    #[serde(flatten)]
    pub meta: LocalCommentMeta,
}

impl LocalReviewComment {
    pub fn new(comment: ReviewComment) -> Self {
        Self {
            comment,
            meta: LocalCommentMeta::default(),
        }
    }

    pub fn with_meta(comment: ReviewComment, meta: LocalCommentMeta) -> Self {
        Self { comment, meta }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalReviewCommentsFile {
    version: u32,
    comments: Vec<LocalReviewComment>,
}

fn hash_path_for_filename(path: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

pub fn effective_working_dir(working_dir: Option<&str>) -> Result<String> {
    if let Some(dir) = working_dir {
        return Ok(dir.to_owned());
    }
    std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to determine working directory: {}", e))
}

pub fn local_review_comments_path(repo: &str, working_dir: Option<&str>) -> Result<PathBuf> {
    local_review_comments_path_with_base(repo, working_dir, &cache_dir())
}

fn local_review_comments_path_with_base(
    repo: &str,
    working_dir: Option<&str>,
    base: &std::path::Path,
) -> Result<PathBuf> {
    let repo = sanitize_repo_name(repo)?;
    let workdir = effective_working_dir(working_dir)?;
    let workdir_hash = hash_path_for_filename(&workdir);
    Ok(base
        .join("local-comments")
        .join(format!("{}-{:016x}.json", repo, workdir_hash)))
}

pub fn load_local_review_comments(
    repo: &str,
    working_dir: Option<&str>,
) -> Result<Vec<LocalReviewComment>> {
    load_local_review_comments_with_base(repo, working_dir, &cache_dir())
}

fn load_local_review_comments_with_base(
    repo: &str,
    working_dir: Option<&str>,
    base: &std::path::Path,
) -> Result<Vec<LocalReviewComment>> {
    let path = local_review_comments_path_with_base(repo, working_dir, base)?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let file: LocalReviewCommentsFile = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;

    if file.version != LOCAL_REVIEW_COMMENTS_VERSION {
        return Err(anyhow::anyhow!(
            "Unsupported local comments version: {}",
            file.version
        ));
    }

    Ok(file.comments)
}

pub fn save_local_review_comments(
    repo: &str,
    working_dir: Option<&str>,
    comments: &[LocalReviewComment],
) -> Result<()> {
    save_local_review_comments_with_base(repo, working_dir, comments, &cache_dir())
}

/// Delete the on-disk local comments file for `(repo, working_dir)` and return
/// the number of comments that were stored before deletion. Returns Ok(0) when
/// no file exists.
pub fn delete_local_review_comments(repo: &str, working_dir: Option<&str>) -> Result<usize> {
    delete_local_review_comments_with_base(repo, working_dir, &cache_dir())
}

fn delete_local_review_comments_with_base(
    repo: &str,
    working_dir: Option<&str>,
    base: &std::path::Path,
) -> Result<usize> {
    let path = local_review_comments_path_with_base(repo, working_dir, base)?;
    if !path.exists() {
        return Ok(0);
    }
    let count = load_local_review_comments_with_base(repo, working_dir, base)
        .map(|c| c.len())
        .unwrap_or(0);
    fs::remove_file(&path)
        .map_err(|e| anyhow::anyhow!("Failed to remove {}: {}", path.display(), e))?;
    Ok(count)
}

fn save_local_review_comments_with_base(
    repo: &str,
    working_dir: Option<&str>,
    comments: &[LocalReviewComment],
    base: &std::path::Path,
) -> Result<()> {
    let path = local_review_comments_path_with_base(repo, working_dir, base)?;

    if comments.is_empty() {
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| anyhow::anyhow!("Failed to remove {}: {}", path.display(), e))?;
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create {}: {}", parent.display(), e))?;
    }

    let payload = LocalReviewCommentsFile {
        version: LOCAL_REVIEW_COMMENTS_VERSION,
        comments: comments.to_vec(),
    };
    let json = serde_json::to_string_pretty(&payload)
        .map_err(|e| anyhow::anyhow!("Failed to serialize local review comments: {}", e))?;
    fs::write(&path, json)
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrCacheKey {
    pub repo: String,
    pub pr_number: u32,
}

/// PRデータのキャッシュエントリ。
///
/// `Arc` ではなく `Box`/`Vec` + `clone()` を使用する設計。
/// `SessionCache` はメインスレッドのイベントループからのみアクセスされるため、
/// スレッド間共有のための `Arc` は不要。`DataState` との間でデータを分配する際は
/// `clone()` で複製する（PR更新時のみ発生するため頻度は低い）。
pub struct PrData {
    pub pr: Box<PullRequest>,
    pub files: Vec<ChangedFile>,
    pub pr_updated_at: String,
}

/// インメモリセッションキャッシュ（LRU eviction 付き）。
///
/// PRデータは最大 `MAX_PR_CACHE_ENTRIES` 件まで保持し、超過時は最も古い
/// エントリを削除する。コメントデータは対応するPRデータが存在するキーにのみ
/// 保存可能で、`pr_data` のライフサイクルと連動して管理される。
pub struct SessionCache {
    pr_data: HashMap<PrCacheKey, PrData>,
    /// アクセス順序リスト（末尾が最新）。LRU eviction に使用。
    access_order: Vec<PrCacheKey>,
    review_comments: HashMap<PrCacheKey, Vec<ReviewComment>>,
    discussion_comments: HashMap<PrCacheKey, Vec<DiscussionComment>>,
}

impl Default for SessionCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionCache {
    pub fn new() -> Self {
        Self {
            pr_data: HashMap::new(),
            access_order: Vec::new(),
            review_comments: HashMap::new(),
            discussion_comments: HashMap::new(),
        }
    }

    /// アクセス順序リストでキーを末尾に移動（最新としてマーク）
    fn touch(&mut self, key: &PrCacheKey) {
        if let Some(pos) = self.access_order.iter().position(|k| k == key) {
            self.access_order.remove(pos);
        }
        self.access_order.push(key.clone());
    }

    /// LRU エントリを削除して容量を `MAX_PR_CACHE_ENTRIES` 以下に保つ
    fn evict_if_needed(&mut self) {
        while self.pr_data.len() > MAX_PR_CACHE_ENTRIES {
            if let Some(oldest_key) = self.access_order.first().cloned() {
                self.access_order.remove(0);
                self.pr_data.remove(&oldest_key);
                self.review_comments.remove(&oldest_key);
                self.discussion_comments.remove(&oldest_key);
            } else {
                break;
            }
        }
    }

    pub fn get_pr_data(&mut self, key: &PrCacheKey) -> Option<&PrData> {
        if self.pr_data.contains_key(key) {
            self.touch(key);
            self.pr_data.get(key)
        } else {
            None
        }
    }

    pub fn put_pr_data(&mut self, key: PrCacheKey, data: PrData) {
        self.touch(&key);
        self.pr_data.insert(key, data);
        self.evict_if_needed();
    }

    pub fn get_review_comments(&self, key: &PrCacheKey) -> Option<&[ReviewComment]> {
        self.review_comments.get(key).map(|v| v.as_slice())
    }

    /// レビューコメントを保存する。対応する `pr_data` が存在しないキーには保存しない。
    pub fn put_review_comments(&mut self, key: PrCacheKey, comments: Vec<ReviewComment>) {
        if self.pr_data.contains_key(&key) {
            self.review_comments.insert(key, comments);
        }
    }

    pub fn remove_review_comments(&mut self, key: &PrCacheKey) {
        self.review_comments.remove(key);
    }

    pub fn get_discussion_comments(&self, key: &PrCacheKey) -> Option<&[DiscussionComment]> {
        self.discussion_comments.get(key).map(|v| v.as_slice())
    }

    /// ディスカッションコメントを保存する。対応する `pr_data` が存在しないキーには保存しない。
    pub fn put_discussion_comments(&mut self, key: PrCacheKey, comments: Vec<DiscussionComment>) {
        if self.pr_data.contains_key(&key) {
            self.discussion_comments.insert(key, comments);
        }
    }

    pub fn remove_discussion_comments(&mut self, key: &PrCacheKey) {
        self.discussion_comments.remove(key);
    }

    /// 特定ファイルの patch を更新（lazy diff ロード結果の反映用）
    pub fn update_file_patch(&mut self, key: &PrCacheKey, filename: &str, patch: Option<String>) {
        if let Some(pr_data) = self.pr_data.get_mut(key) {
            if let Some(file) = pr_data.files.iter_mut().find(|f| f.filename == filename) {
                file.patch = patch;
            }
        }
    }

    pub fn invalidate_all(&mut self) {
        self.pr_data.clear();
        self.access_order.clear();
        self.review_comments.clear();
        self.discussion_comments.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.pr_data.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.pr_data.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Branch, User};
    use serial_test::serial;
    use tempfile::tempdir;

    fn make_test_pr(title: &str, updated_at: &str) -> PullRequest {
        PullRequest {
            number: 1,
            node_id: None,
            title: title.to_string(),
            body: None,
            state: "open".to_string(),
            head: Branch {
                ref_name: "feature".to_string(),
                sha: "abc123".to_string(),
            },
            base: Branch {
                ref_name: "main".to_string(),
                sha: "def456".to_string(),
            },
            user: User {
                login: "testuser".to_string(),
            },
            updated_at: updated_at.to_string(),
        }
    }

    #[test]
    fn test_sanitize_repo_name_valid() {
        // Standard owner/repo format
        assert_eq!(
            sanitize_repo_name("owner/repo").unwrap(),
            "owner_repo".to_string()
        );

        // Repo name with hyphens
        assert_eq!(
            sanitize_repo_name("my-org/my-repo").unwrap(),
            "my-org_my-repo".to_string()
        );

        // Repo name with dots (e.g., config files or versioned repos)
        assert_eq!(
            sanitize_repo_name("owner/repo.js").unwrap(),
            "owner_repo.js".to_string()
        );

        // Repo name with underscores
        assert_eq!(
            sanitize_repo_name("my_org/my_repo").unwrap(),
            "my_org_my_repo".to_string()
        );

        // Alphanumeric only
        assert_eq!(
            sanitize_repo_name("owner123/repo456").unwrap(),
            "owner123_repo456".to_string()
        );
    }

    #[test]
    fn test_sanitize_repo_name_path_traversal() {
        // Path traversal with ..
        assert!(sanitize_repo_name("..").is_err());
        assert!(sanitize_repo_name("../foo").is_err());
        assert!(sanitize_repo_name("foo/../bar").is_err());
        assert!(sanitize_repo_name("foo/..").is_err());

        // Absolute path attempts
        assert!(sanitize_repo_name("/etc/passwd").is_err());
        assert!(sanitize_repo_name("\\Windows\\System32").is_err());
    }

    #[test]
    fn test_sanitize_repo_name_hidden_files() {
        // Starting with dot (hidden file/directory)
        assert!(sanitize_repo_name(".hidden").is_err());
        assert!(sanitize_repo_name(".config/repo").is_err());

        // Note: .github is a valid org name on GitHub, but our function rejects
        // names starting with dots for security. This is intentional.
    }

    #[test]
    fn test_sanitize_repo_name_invalid_characters() {
        // Space
        assert!(sanitize_repo_name("owner/repo name").is_err());

        // Special characters
        assert!(sanitize_repo_name("owner/repo@123").is_err());
        assert!(sanitize_repo_name("owner/repo#123").is_err());
        assert!(sanitize_repo_name("owner/repo$var").is_err());
        assert!(sanitize_repo_name("owner/repo%20").is_err());
        assert!(sanitize_repo_name("owner/repo&foo").is_err());
        assert!(sanitize_repo_name("owner/repo*").is_err());
        assert!(sanitize_repo_name("owner/repo;cmd").is_err());
        assert!(sanitize_repo_name("owner/repo|pipe").is_err());

        // Backtick (command injection)
        assert!(sanitize_repo_name("owner/repo`cmd`").is_err());

        // Parentheses
        assert!(sanitize_repo_name("owner/repo(1)").is_err());
    }

    #[test]
    fn test_sanitize_repo_name_unicode() {
        // Only ASCII alphanumeric characters are allowed to prevent Unicode
        // homoglyph attacks and path traversal via Unicode normalization.
        // Japanese characters are rejected
        assert!(sanitize_repo_name("owner/日本語").is_err());

        // Emoji are not alphanumeric
        assert!(sanitize_repo_name("owner/repo🚀").is_err());

        // Fullwidth dot/period (U+FF0E) is not alphanumeric
        assert!(sanitize_repo_name("owner/．．").is_err());
    }

    #[test]
    fn test_sanitize_repo_name_edge_cases() {
        // Empty components (multiple slashes become multiple underscores)
        // This is acceptable as it doesn't pose a security risk
        let result = sanitize_repo_name("owner//repo");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "owner__repo");

        // Single name without slash
        assert_eq!(
            sanitize_repo_name("simple-repo").unwrap(),
            "simple-repo".to_string()
        );
    }

    #[test]
    #[serial]
    fn test_local_review_comments_roundtrip() {
        let tempdir = tempdir().unwrap();
        let base = tempdir.path().join("cache");
        let workdir = tempdir.path().join("worktree");
        fs::create_dir_all(&workdir).unwrap();

        let comments = vec![LocalReviewComment::new(ReviewComment {
            id: 1,
            path: "src/main.rs".to_string(),
            line: Some(42),
            start_line: None,
            body: "hello".to_string(),
            user: User {
                login: "local".to_string(),
            },
            created_at: "2026-03-24T00:00:00Z".to_string(),
        })];

        save_local_review_comments_with_base(
            "owner/repo",
            Some(workdir.to_string_lossy().as_ref()),
            &comments,
            &base,
        )
        .unwrap();

        let loaded = load_local_review_comments_with_base(
            "owner/repo",
            Some(workdir.to_string_lossy().as_ref()),
            &base,
        )
        .unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].comment.body, "hello");
        assert_eq!(loaded[0].comment.line, Some(42));
        assert!(!loaded[0].meta.is_resolved);
    }

    #[test]
    #[serial]
    fn test_delete_local_review_comments_returns_count_and_removes_file() {
        let tempdir = tempdir().unwrap();
        let base = tempdir.path().join("cache");
        let workdir = tempdir.path().join("worktree");
        fs::create_dir_all(&workdir).unwrap();

        // No file → Ok(0)
        let initial = delete_local_review_comments_with_base(
            "owner/repo",
            Some(workdir.to_string_lossy().as_ref()),
            &base,
        )
        .unwrap();
        assert_eq!(initial, 0);

        // Save two comments
        let comments = vec![
            LocalReviewComment::new(ReviewComment {
                id: 1,
                path: "src/a.rs".to_string(),
                line: Some(1),
                start_line: None,
                body: "first".to_string(),
                user: User {
                    login: "local".to_string(),
                },
                created_at: "2026-04-27T00:00:00Z".to_string(),
            }),
            LocalReviewComment::new(ReviewComment {
                id: 2,
                path: "src/b.rs".to_string(),
                line: Some(2),
                start_line: None,
                body: "second".to_string(),
                user: User {
                    login: "local".to_string(),
                },
                created_at: "2026-04-27T00:01:00Z".to_string(),
            }),
        ];
        save_local_review_comments_with_base(
            "owner/repo",
            Some(workdir.to_string_lossy().as_ref()),
            &comments,
            &base,
        )
        .unwrap();

        let path = local_review_comments_path_with_base(
            "owner/repo",
            Some(workdir.to_string_lossy().as_ref()),
            &base,
        )
        .unwrap();
        assert!(path.exists());

        // Delete returns the prior count and removes the file
        let removed = delete_local_review_comments_with_base(
            "owner/repo",
            Some(workdir.to_string_lossy().as_ref()),
            &base,
        )
        .unwrap();
        assert_eq!(removed, 2);
        assert!(!path.exists());

        // Idempotent
        let again = delete_local_review_comments_with_base(
            "owner/repo",
            Some(workdir.to_string_lossy().as_ref()),
            &base,
        )
        .unwrap();
        assert_eq!(again, 0);
    }

    #[test]
    #[serial]
    fn test_local_review_comments_path_changes_with_workdir() {
        let tempdir = tempdir().unwrap();
        let base = tempdir.path();

        let path_a =
            local_review_comments_path_with_base("owner/repo", Some("/tmp/a"), base).unwrap();
        let path_b =
            local_review_comments_path_with_base("owner/repo", Some("/tmp/b"), base).unwrap();

        assert_ne!(path_a, path_b);
    }

    #[test]
    fn test_session_cache_put_get_pr_data() {
        let mut cache = SessionCache::new();
        let key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 1,
        };

        assert!(cache.get_pr_data(&key).is_none());

        let pr = make_test_pr("test", "2024-01-01");
        cache.put_pr_data(
            key.clone(),
            PrData {
                pr: Box::new(pr),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );

        let data = cache.get_pr_data(&key).unwrap();
        assert_eq!(data.pr.title, "test");
        assert!(data.files.is_empty());
    }

    #[test]
    fn test_session_cache_put_get_review_comments() {
        let mut cache = SessionCache::new();
        let key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 1,
        };

        assert!(cache.get_review_comments(&key).is_none());

        // pr_data が存在しないキーにはコメントを保存できない
        cache.put_review_comments(key.clone(), vec![]);
        assert!(cache.get_review_comments(&key).is_none());

        // pr_data を先に保存すればコメントも保存可能
        cache.put_pr_data(
            key.clone(),
            PrData {
                pr: Box::new(make_test_pr("test", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );
        cache.put_review_comments(key.clone(), vec![]);
        let comments = cache.get_review_comments(&key).unwrap();
        assert!(comments.is_empty());
    }

    #[test]
    fn test_session_cache_put_get_discussion_comments() {
        let mut cache = SessionCache::new();
        let key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 1,
        };

        assert!(cache.get_discussion_comments(&key).is_none());

        // pr_data が存在しないキーにはコメントを保存できない
        cache.put_discussion_comments(key.clone(), vec![]);
        assert!(cache.get_discussion_comments(&key).is_none());

        // pr_data を先に保存すればコメントも保存可能
        cache.put_pr_data(
            key.clone(),
            PrData {
                pr: Box::new(make_test_pr("test", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );
        cache.put_discussion_comments(key.clone(), vec![]);
        let comments = cache.get_discussion_comments(&key).unwrap();
        assert!(comments.is_empty());
    }

    #[test]
    fn test_session_cache_remove_review_comments() {
        let mut cache = SessionCache::new();
        let key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 1,
        };

        cache.put_pr_data(
            key.clone(),
            PrData {
                pr: Box::new(make_test_pr("test", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );
        cache.put_review_comments(key.clone(), vec![]);
        assert!(cache.get_review_comments(&key).is_some());

        cache.remove_review_comments(&key);
        assert!(cache.get_review_comments(&key).is_none());
    }

    #[test]
    fn test_session_cache_remove_discussion_comments() {
        let mut cache = SessionCache::new();
        let key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 1,
        };

        cache.put_pr_data(
            key.clone(),
            PrData {
                pr: Box::new(make_test_pr("test", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );
        cache.put_discussion_comments(key.clone(), vec![]);
        assert!(cache.get_discussion_comments(&key).is_some());

        cache.remove_discussion_comments(&key);
        assert!(cache.get_discussion_comments(&key).is_none());
    }

    #[test]
    fn test_session_cache_invalidate_all() {
        let mut cache = SessionCache::new();
        let key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 1,
        };

        cache.put_pr_data(
            key.clone(),
            PrData {
                pr: Box::new(make_test_pr("test", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );
        cache.put_review_comments(key.clone(), vec![]);
        cache.put_discussion_comments(key.clone(), vec![]);

        cache.invalidate_all();

        assert!(cache.get_pr_data(&key).is_none());
        assert!(cache.get_review_comments(&key).is_none());
        assert!(cache.get_discussion_comments(&key).is_none());
    }

    #[test]
    fn test_session_cache_multiple_prs() {
        let mut cache = SessionCache::new();
        let key1 = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 1,
        };
        let key2 = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 2,
        };

        cache.put_pr_data(
            key1.clone(),
            PrData {
                pr: Box::new(make_test_pr("PR 1", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );
        cache.put_pr_data(
            key2.clone(),
            PrData {
                pr: Box::new(make_test_pr("PR 2", "2024-01-02")),
                files: vec![],
                pr_updated_at: "2024-01-02".to_string(),
            },
        );

        assert_eq!(cache.get_pr_data(&key1).unwrap().pr.title, "PR 1");
        assert_eq!(cache.get_pr_data(&key2).unwrap().pr.title, "PR 2");
    }

    #[test]
    fn test_session_cache_lru_eviction() {
        let mut cache = SessionCache::new();

        // MAX_PR_CACHE_ENTRIES + 1 個のエントリを追加
        for i in 0..=MAX_PR_CACHE_ENTRIES {
            let key = PrCacheKey {
                repo: "owner/repo".to_string(),
                pr_number: i as u32,
            };
            cache.put_pr_data(
                key.clone(),
                PrData {
                    pr: Box::new(make_test_pr(&format!("PR {}", i), "2024-01-01")),
                    files: vec![],
                    pr_updated_at: "2024-01-01".to_string(),
                },
            );
            cache.put_review_comments(key, vec![]);
        }

        // 最大容量を超えないこと
        assert_eq!(cache.len(), MAX_PR_CACHE_ENTRIES);

        // 最初のエントリ（PR #0）が削除されていること
        let evicted_key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 0,
        };
        assert!(cache.get_pr_data(&evicted_key).is_none());
        // 関連コメントも削除されていること
        assert!(cache.get_review_comments(&evicted_key).is_none());

        // 最後のエントリは残っていること
        let last_key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: MAX_PR_CACHE_ENTRIES as u32,
        };
        assert!(cache.get_pr_data(&last_key).is_some());
    }

    #[test]
    fn test_session_cache_lru_access_order() {
        let mut cache = SessionCache::new();

        // MAX_PR_CACHE_ENTRIES 個のエントリを追加
        for i in 0..MAX_PR_CACHE_ENTRIES {
            let key = PrCacheKey {
                repo: "owner/repo".to_string(),
                pr_number: i as u32,
            };
            cache.put_pr_data(
                key,
                PrData {
                    pr: Box::new(make_test_pr(&format!("PR {}", i), "2024-01-01")),
                    files: vec![],
                    pr_updated_at: "2024-01-01".to_string(),
                },
            );
        }

        // PR #0 にアクセスして最新に昇格
        let key0 = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 0,
        };
        assert!(cache.get_pr_data(&key0).is_some());

        // 新しいエントリを追加（PR #1 が evict されるはず）
        let new_key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 100,
        };
        cache.put_pr_data(
            new_key.clone(),
            PrData {
                pr: Box::new(make_test_pr("PR 100", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );

        // PR #0 はアクセスしたため残っている
        assert!(cache.get_pr_data(&key0).is_some());
        // PR #1 が削除されている
        let key1 = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 1,
        };
        assert!(cache.get_pr_data(&key1).is_none());
        // 新しいエントリは存在する
        assert!(cache.get_pr_data(&new_key).is_some());
    }

    #[test]
    fn test_session_cache_comments_rejected_without_pr_data() {
        let mut cache = SessionCache::new();
        let key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 99,
        };

        // pr_data が存在しないキーへのコメント保存は無視される
        cache.put_review_comments(key.clone(), vec![]);
        cache.put_discussion_comments(key.clone(), vec![]);
        assert!(cache.get_review_comments(&key).is_none());
        assert!(cache.get_discussion_comments(&key).is_none());

        // pr_data を追加すればコメント保存可能
        cache.put_pr_data(
            key.clone(),
            PrData {
                pr: Box::new(make_test_pr("test", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );
        cache.put_review_comments(key.clone(), vec![]);
        cache.put_discussion_comments(key.clone(), vec![]);
        assert!(cache.get_review_comments(&key).is_some());
        assert!(cache.get_discussion_comments(&key).is_some());
    }

    #[test]
    fn test_session_cache_evicted_pr_rejects_comments() {
        let mut cache = SessionCache::new();

        // MAX_PR_CACHE_ENTRIES 個のエントリを追加
        for i in 0..MAX_PR_CACHE_ENTRIES {
            let key = PrCacheKey {
                repo: "owner/repo".to_string(),
                pr_number: i as u32,
            };
            cache.put_pr_data(
                key,
                PrData {
                    pr: Box::new(make_test_pr(&format!("PR {}", i), "2024-01-01")),
                    files: vec![],
                    pr_updated_at: "2024-01-01".to_string(),
                },
            );
        }

        // 新しいエントリを追加して PR #0 を evict
        let new_key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 100,
        };
        cache.put_pr_data(
            new_key,
            PrData {
                pr: Box::new(make_test_pr("PR 100", "2024-01-01")),
                files: vec![],
                pr_updated_at: "2024-01-01".to_string(),
            },
        );

        // evict された PR #0 へのコメント保存は無視される
        let evicted_key = PrCacheKey {
            repo: "owner/repo".to_string(),
            pr_number: 0,
        };
        assert!(cache.get_pr_data(&evicted_key).is_none());
        cache.put_review_comments(evicted_key.clone(), vec![]);
        cache.put_discussion_comments(evicted_key.clone(), vec![]);
        assert!(cache.get_review_comments(&evicted_key).is_none());
        assert!(cache.get_discussion_comments(&evicted_key).is_none());
    }
}
