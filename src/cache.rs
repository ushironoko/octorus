use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use xdg::BaseDirectories;

use crate::github::comment::{DiscussionComment, ReviewComment};
use crate::github::{ChangedFile, PullRequest};

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
        if !c.is_alphanumeric() && c != '_' && c != '-' && c != '.' {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrCacheKey {
    pub repo: String,
    pub pr_number: u32,
}

pub struct PrData {
    pub pr: Box<PullRequest>,
    pub files: Vec<ChangedFile>,
    pub pr_updated_at: String,
}

pub struct SessionCache {
    pr_data: HashMap<PrCacheKey, PrData>,
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
            review_comments: HashMap::new(),
            discussion_comments: HashMap::new(),
        }
    }

    pub fn get_pr_data(&self, key: &PrCacheKey) -> Option<&PrData> {
        self.pr_data.get(key)
    }

    pub fn put_pr_data(&mut self, key: PrCacheKey, data: PrData) {
        self.pr_data.insert(key, data);
    }

    pub fn get_review_comments(&self, key: &PrCacheKey) -> Option<&[ReviewComment]> {
        self.review_comments.get(key).map(|v| v.as_slice())
    }

    pub fn put_review_comments(&mut self, key: PrCacheKey, comments: Vec<ReviewComment>) {
        self.review_comments.insert(key, comments);
    }

    pub fn remove_review_comments(&mut self, key: &PrCacheKey) {
        self.review_comments.remove(key);
    }

    pub fn get_discussion_comments(&self, key: &PrCacheKey) -> Option<&[DiscussionComment]> {
        self.discussion_comments.get(key).map(|v| v.as_slice())
    }

    pub fn put_discussion_comments(
        &mut self,
        key: PrCacheKey,
        comments: Vec<DiscussionComment>,
    ) {
        self.discussion_comments.insert(key, comments);
    }

    pub fn remove_discussion_comments(&mut self, key: &PrCacheKey) {
        self.discussion_comments.remove(key);
    }

    pub fn invalidate_all(&mut self) {
        self.pr_data.clear();
        self.review_comments.clear();
        self.discussion_comments.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Branch, User};

    fn make_test_pr(title: &str, updated_at: &str) -> PullRequest {
        PullRequest {
            number: 1,
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
        // Note: The current implementation uses is_alphanumeric() which accepts
        // Unicode alphanumeric characters. This is intentional to support
        // international repository names on GitHub.
        // Japanese characters are alphanumeric in Unicode
        assert!(sanitize_repo_name("owner/日本語").is_ok());

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
}
