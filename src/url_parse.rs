//! Parse GitHub PR / issue URLs into their components.
//!
//! Supports the common URL shapes that get pasted between people:
//! `https://github.com/owner/repo/pull/123`, with optional scheme,
//! trailing path segments (`/files`, `/commits`, ...), query strings,
//! and URL fragments.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubRef {
    Pr {
        owner: String,
        repo: String,
        number: u32,
    },
    Issue {
        owner: String,
        repo: String,
        number: u32,
    },
}

impl GithubRef {
    pub fn repo_slug(&self) -> String {
        match self {
            GithubRef::Pr { owner, repo, .. } | GithubRef::Issue { owner, repo, .. } => {
                format!("{owner}/{repo}")
            }
        }
    }

    pub fn number(&self) -> u32 {
        match self {
            GithubRef::Pr { number, .. } | GithubRef::Issue { number, .. } => *number,
        }
    }
}

/// Parse a GitHub PR or issue URL. Returns `None` if the input doesn't look
/// like a github.com PR/issue URL.
pub fn parse_github_url(input: &str) -> Option<GithubRef> {
    let s = input.trim();

    // Strip query string and fragment.
    let s = s.split(['?', '#']).next().unwrap_or(s);

    // Strip scheme.
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);

    // Require github.com host (allow optional `www.`).
    let s = s.strip_prefix("www.").unwrap_or(s);
    let s = s.strip_prefix("github.com/")?;

    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 4 {
        return None;
    }

    let owner = parts[0];
    let repo = parts[1];
    let kind = parts[2];
    let number: u32 = parts[3].parse().ok()?;

    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    match kind {
        "pull" => Some(GithubRef::Pr {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }),
        "issues" => Some(GithubRef::Issue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(owner: &str, repo: &str, number: u32) -> GithubRef {
        GithubRef::Pr {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }
    }

    fn issue(owner: &str, repo: &str, number: u32) -> GithubRef {
        GithubRef::Issue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }
    }

    #[test]
    fn parses_canonical_pr_url() {
        assert_eq!(
            parse_github_url("https://github.com/ushironoko/octorus/pull/123"),
            Some(pr("ushironoko", "octorus", 123))
        );
    }

    #[test]
    fn parses_pr_url_with_files_suffix() {
        assert_eq!(
            parse_github_url("https://github.com/ushironoko/octorus/pull/123/files"),
            Some(pr("ushironoko", "octorus", 123))
        );
    }

    #[test]
    fn parses_pr_url_with_fragment() {
        assert_eq!(
            parse_github_url(
                "https://github.com/ushironoko/octorus/pull/123#discussion_r987654321"
            ),
            Some(pr("ushironoko", "octorus", 123))
        );
    }

    #[test]
    fn parses_pr_url_with_query_string() {
        assert_eq!(
            parse_github_url("https://github.com/ushironoko/octorus/pull/123?diff=split"),
            Some(pr("ushironoko", "octorus", 123))
        );
    }

    #[test]
    fn parses_pr_url_without_scheme() {
        assert_eq!(
            parse_github_url("github.com/ushironoko/octorus/pull/123"),
            Some(pr("ushironoko", "octorus", 123))
        );
    }

    #[test]
    fn parses_pr_url_with_http_scheme() {
        assert_eq!(
            parse_github_url("http://github.com/ushironoko/octorus/pull/123"),
            Some(pr("ushironoko", "octorus", 123))
        );
    }

    #[test]
    fn parses_pr_url_with_trailing_slash() {
        assert_eq!(
            parse_github_url("https://github.com/ushironoko/octorus/pull/123/"),
            Some(pr("ushironoko", "octorus", 123))
        );
    }

    #[test]
    fn parses_issue_url() {
        assert_eq!(
            parse_github_url("https://github.com/ushironoko/octorus/issues/161"),
            Some(issue("ushironoko", "octorus", 161))
        );
    }

    #[test]
    fn rejects_plain_repo_url() {
        assert_eq!(
            parse_github_url("https://github.com/ushironoko/octorus"),
            None
        );
    }

    #[test]
    fn rejects_non_github_host() {
        assert_eq!(
            parse_github_url("https://gitlab.com/ushironoko/octorus/pull/123"),
            None
        );
    }

    #[test]
    fn rejects_unknown_kind() {
        assert_eq!(
            parse_github_url("https://github.com/ushironoko/octorus/wiki/123"),
            None
        );
    }

    #[test]
    fn rejects_non_numeric_number() {
        assert_eq!(
            parse_github_url("https://github.com/ushironoko/octorus/pull/abc"),
            None
        );
    }

    #[test]
    fn rejects_repo_slug() {
        assert_eq!(parse_github_url("ushironoko/octorus"), None);
    }

    #[test]
    fn repo_slug_helper() {
        assert_eq!(pr("a", "b", 1).repo_slug(), "a/b");
        assert_eq!(issue("a", "b", 1).repo_slug(), "a/b");
    }
}
