use anyhow::Error;
use serde::Deserialize;
use thiserror::Error;

use super::client::{classify_gh_failure, error_chain_lowercased, gh_command, GhFailureKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitPullRequestState {
    Open,
    Closed,
    Merged,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPullRequest {
    pub number: u32,
    pub title: String,
    pub state: CommitPullRequestState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitPrResolution {
    Confirmed { pulls: Vec<CommitPullRequest> },
    Inferred { pull: CommitPullRequest },
    NotFound,
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum CommitPrLookupError {
    #[error("GitHub CLI is not installed or not on PATH")]
    CliMissing,
    #[error("GitHub CLI is not authenticated — run gh auth login")]
    Unauthenticated,
    #[error("GitHub API rate limit exceeded — try again later")]
    RateLimited,
    #[error("GitHub does not know this commit — it may not be pushed yet")]
    CommitNotOnGitHub,
    #[error("GitHub API failed while looking up this commit")]
    ApiFailure,
    #[error("GitHub returned an empty response for this commit")]
    EmptyResponse,
    #[error("GitHub returned malformed pull request data for this commit")]
    MalformedResponse,
}

#[derive(Deserialize)]
struct WirePullRequest {
    number: u32,
    title: String,
    state: String,
    merged_at: Option<String>,
}

pub fn commit_pulls_endpoint(repo: &str, sha: &str) -> String {
    format!("repos/{repo}/commits/{sha}/pulls?per_page=100")
}

fn commit_pull_request_api_args(repo: &str, sha: &str) -> [String; 4] {
    [
        "api".to_string(),
        "--paginate".to_string(),
        "--slurp".to_string(),
        commit_pulls_endpoint(repo, sha),
    ]
}

pub async fn fetch_commit_pull_requests(
    repo: &str,
    sha: &str,
    subject: &str,
) -> Result<CommitPrResolution, CommitPrLookupError> {
    let args = commit_pull_request_api_args(repo, sha);
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let response = gh_command(&arg_refs)
        .await
        .map_err(|error| classify_gh_command_error(&error))?;
    resolve_paginated_commit_pulls_response(&response, subject)
}

pub fn resolve_commit_pulls_response(
    response: &str,
    subject: &str,
) -> Result<CommitPrResolution, CommitPrLookupError> {
    if response.trim().is_empty() {
        return Err(CommitPrLookupError::EmptyResponse);
    }

    let wire: Vec<WirePullRequest> =
        serde_json::from_str(response).map_err(|_| CommitPrLookupError::MalformedResponse)?;
    resolve_commit_pulls(wire, subject)
}

pub fn resolve_paginated_commit_pulls_response(
    response: &str,
    subject: &str,
) -> Result<CommitPrResolution, CommitPrLookupError> {
    if response.trim().is_empty() {
        return Err(CommitPrLookupError::EmptyResponse);
    }

    let pages: Vec<serde_json::Value> =
        serde_json::from_str(response).map_err(|_| CommitPrLookupError::MalformedResponse)?;
    let flattened =
        super::client::flatten_pages(pages).map_err(|_| CommitPrLookupError::MalformedResponse)?;
    let wire: Vec<WirePullRequest> =
        serde_json::from_value(flattened).map_err(|_| CommitPrLookupError::MalformedResponse)?;
    resolve_commit_pulls(wire, subject)
}

fn resolve_commit_pulls(
    wire: Vec<WirePullRequest>,
    subject: &str,
) -> Result<CommitPrResolution, CommitPrLookupError> {
    let pulls = wire
        .into_iter()
        .map(commit_pull_request_from_wire)
        .collect::<Result<Vec<_>, _>>()?;

    if !pulls.is_empty() {
        return Ok(CommitPrResolution::Confirmed { pulls });
    }

    Ok(match trailing_pr_reference(subject) {
        Some((title, number)) => CommitPrResolution::Inferred {
            pull: CommitPullRequest {
                number,
                title: title.to_string(),
                state: CommitPullRequestState::Unknown,
            },
        },
        None => CommitPrResolution::NotFound,
    })
}

fn commit_pull_request_from_wire(
    wire: WirePullRequest,
) -> Result<CommitPullRequest, CommitPrLookupError> {
    if wire.number == 0 {
        return Err(CommitPrLookupError::MalformedResponse);
    }
    let state = if wire.merged_at.is_some() {
        CommitPullRequestState::Merged
    } else {
        match wire.state.as_str() {
            "open" => CommitPullRequestState::Open,
            "closed" => CommitPullRequestState::Closed,
            _ => return Err(CommitPrLookupError::MalformedResponse),
        }
    };
    Ok(CommitPullRequest {
        number: wire.number,
        title: wire.title,
        state,
    })
}

fn trailing_pr_reference(subject: &str) -> Option<(&str, u32)> {
    let without_closing = subject.strip_suffix(')')?;
    let (title, digits) = without_closing.rsplit_once(" (#")?;
    if title.is_empty() || digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = digits.parse::<u32>().ok().filter(|number| *number > 0)?;
    Some((title, number))
}

pub fn classify_gh_command_error(error: &Error) -> CommitPrLookupError {
    match classify_gh_failure(error) {
        GhFailureKind::CliMissing => CommitPrLookupError::CliMissing,
        GhFailureKind::Unauthenticated => CommitPrLookupError::Unauthenticated,
        GhFailureKind::RateLimited => CommitPrLookupError::RateLimited,
        // 404 is commit-specific and is not a GhFailureKind.
        GhFailureKind::Offline | GhFailureKind::Other => {
            if commit_not_on_github(&error_chain_lowercased(error)) {
                CommitPrLookupError::CommitNotOnGitHub
            } else {
                CommitPrLookupError::ApiFailure
            }
        }
    }
}

fn commit_not_on_github(message: &str) -> bool {
    message.contains("http 404") || message.contains("no commit found for")
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::io;

    const SUBJECT_WITH_PR: &str = "fix parser edge cases (#123)";

    #[test]
    fn test_commit_pulls_endpoint_uses_repository_and_full_sha() {
        assert_eq!(
            commit_pulls_endpoint("owner/repo", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "repos/owner/repo/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/pulls?per_page=100"
        );
    }

    #[test]
    fn test_commit_pull_request_api_args_request_every_page() {
        assert_eq!(
            commit_pull_request_api_args("owner/repo", "abc123").as_slice(),
            [
                "api",
                "--paginate",
                "--slurp",
                "repos/owner/repo/commits/abc123/pulls?per_page=100",
            ]
        );
    }

    #[test]
    fn test_paginated_api_response_keeps_pull_requests_from_later_pages() {
        let response = r#"[[
            {"number": 1, "title": "first page", "state": "open", "merged_at": null}
        ], [
            {
                "number": 2,
                "title": "second page",
                "state": "closed",
                "merged_at": "2026-07-27T12:00:00Z"
            }
        ]]"#;

        assert_eq!(
            resolve_paginated_commit_pulls_response(response, "ordinary subject").unwrap(),
            CommitPrResolution::Confirmed {
                pulls: vec![
                    CommitPullRequest {
                        number: 1,
                        title: "first page".to_string(),
                        state: CommitPullRequestState::Open,
                    },
                    CommitPullRequest {
                        number: 2,
                        title: "second page".to_string(),
                        state: CommitPullRequestState::Merged,
                    },
                ],
            }
        );
    }

    #[test]
    fn test_api_answer_is_source_of_truth_even_when_subject_names_another_pr() {
        let response = r#"[{
            "number": 456,
            "title": "confirmed by GitHub",
            "state": "open",
            "merged_at": null
        }]"#;

        assert_eq!(
            resolve_commit_pulls_response(response, SUBJECT_WITH_PR).unwrap(),
            CommitPrResolution::Confirmed {
                pulls: vec![CommitPullRequest {
                    number: 456,
                    title: "confirmed by GitHub".to_string(),
                    state: CommitPullRequestState::Open,
                }],
            }
        );
    }

    #[test]
    fn test_api_states_surface_open_closed_and_merged_pull_requests() {
        let response = r#"[
            {"number": 1, "title": "open", "state": "open", "merged_at": null},
            {"number": 2, "title": "closed", "state": "closed", "merged_at": null},
            {
                "number": 3,
                "title": "merged",
                "state": "closed",
                "merged_at": "2026-07-27T12:00:00Z"
            }
        ]"#;

        let CommitPrResolution::Confirmed { pulls } =
            resolve_commit_pulls_response(response, "ordinary subject").unwrap()
        else {
            panic!("expected confirmed API result");
        };

        assert_eq!(
            pulls.iter().map(|pull| pull.state).collect::<Vec<_>>(),
            vec![
                CommitPullRequestState::Open,
                CommitPullRequestState::Closed,
                CommitPullRequestState::Merged,
            ]
        );
    }

    #[test]
    fn test_valid_empty_api_result_uses_only_trailing_pr_reference_and_marks_inferred() {
        assert_eq!(
            resolve_commit_pulls_response("[]", SUBJECT_WITH_PR).unwrap(),
            CommitPrResolution::Inferred {
                pull: CommitPullRequest {
                    number: 123,
                    title: "fix parser edge cases".to_string(),
                    state: CommitPullRequestState::Unknown,
                },
            }
        );
    }

    #[test]
    fn test_subject_fallback_accepts_unicode_before_the_trailing_reference() {
        assert_eq!(
            resolve_commit_pulls_response("[]", "日本語の不具合を修正 (#321)").unwrap(),
            CommitPrResolution::Inferred {
                pull: CommitPullRequest {
                    number: 321,
                    title: "日本語の不具合を修正".to_string(),
                    state: CommitPullRequestState::Unknown,
                },
            }
        );
    }

    #[test]
    fn test_subject_fallback_rejects_numbers_that_are_not_trailing_pr_references() {
        for subject in [
            "fix issue 42",
            "bump to v1.2.3",
            "#123 fix parser",
            "fix #123 in parser",
            "fix parser (#123) afterward",
            "fix parser(#123)",
            "fix parser (#0)",
            "fix parser (#999999999999999999999999)",
        ] {
            assert_eq!(
                resolve_commit_pulls_response("[]", subject).unwrap(),
                CommitPrResolution::NotFound,
                "{subject}"
            );
        }
    }

    #[test]
    fn test_malformed_or_empty_api_response_never_falls_back_to_the_subject() {
        let cases = [
            ("", CommitPrLookupError::EmptyResponse),
            ("   \n", CommitPrLookupError::EmptyResponse),
            ("not json", CommitPrLookupError::MalformedResponse),
            ("null", CommitPrLookupError::MalformedResponse),
            (
                r#"[{"title":"missing number","state":"open","merged_at":null}]"#,
                CommitPrLookupError::MalformedResponse,
            ),
            (
                r#"[{"number":1,"title":"valid","state":"open","merged_at":null},
                    {"title":"invalid","state":"closed","merged_at":null}]"#,
                CommitPrLookupError::MalformedResponse,
            ),
            (
                r#"[{"number":1,"title":"unknown","state":"draft","merged_at":null}]"#,
                CommitPrLookupError::MalformedResponse,
            ),
        ];

        for (response, expected) in cases {
            assert_eq!(
                resolve_commit_pulls_response(response, SUBJECT_WITH_PR),
                Err(expected),
                "{response:?}"
            );
        }
    }

    #[test]
    fn test_malformed_or_empty_paginated_response_keeps_exact_error_classification() {
        let cases = [
            ("", CommitPrLookupError::EmptyResponse),
            (" \n", CommitPrLookupError::EmptyResponse),
            ("not json", CommitPrLookupError::MalformedResponse),
            ("null", CommitPrLookupError::MalformedResponse),
            ("[{}]", CommitPrLookupError::MalformedResponse),
            (
                r#"[[{"number":1,"title":"valid","state":"open","merged_at":null}],{}]"#,
                CommitPrLookupError::MalformedResponse,
            ),
        ];

        for (response, expected) in cases {
            assert_eq!(
                resolve_paginated_commit_pulls_response(response, SUBJECT_WITH_PR),
                Err(expected),
                "{response:?}"
            );
        }
    }

    #[test]
    fn test_gh_failure_classifier_distinguishes_missing_auth_rate_limit_and_api_failure() {
        let missing = anyhow::Error::new(io::Error::from(io::ErrorKind::NotFound))
            .context("Failed to execute gh CLI - is it installed?");
        let permission = anyhow::Error::new(io::Error::from(io::ErrorKind::PermissionDenied))
            .context("Failed to execute gh CLI - is it installed?");

        assert_eq!(
            classify_gh_command_error(&missing),
            CommitPrLookupError::CliMissing
        );
        assert_eq!(
            classify_gh_command_error(&anyhow!(
                "GitHub CLI command failed: not logged into any GitHub hosts; run gh auth login"
            )),
            CommitPrLookupError::Unauthenticated
        );
        assert_eq!(
            classify_gh_command_error(&anyhow!(
                "GitHub CLI command failed: HTTP 401: Bad credentials"
            )),
            CommitPrLookupError::Unauthenticated
        );
        assert_eq!(
            classify_gh_command_error(&anyhow!(
                "GitHub CLI command failed: API rate limit exceeded"
            )),
            CommitPrLookupError::RateLimited
        );
        assert_eq!(
            classify_gh_command_error(&anyhow!("GitHub CLI command failed: secondary rate limit")),
            CommitPrLookupError::RateLimited
        );
        for message in [
            "gh command failed: gh: No commit found for SHA: deadbeef (HTTP 404)",
            "gh command failed: gh: No commit found for SHA: deadbeef",
            "gh command failed: HTTP 404: Not Found \
             (https://api.github.com/repos/owner/repo/commits/deadbeef/pulls)",
        ] {
            let classified = classify_gh_command_error(&anyhow!(message));
            assert_eq!(classified, CommitPrLookupError::CommitNotOnGitHub);
            assert_ne!(classified, CommitPrLookupError::ApiFailure);
        }
        assert_eq!(
            classify_gh_command_error(&permission),
            CommitPrLookupError::ApiFailure
        );
        assert_eq!(
            classify_gh_command_error(&anyhow!(
                "GitHub CLI command failed: HTTP 403: SSO authorization required"
            )),
            CommitPrLookupError::ApiFailure
        );
        assert_eq!(
            classify_gh_command_error(&anyhow!("GitHub CLI command failed: server unavailable")),
            CommitPrLookupError::ApiFailure
        );
    }

    #[test]
    fn test_shared_classifier_keeps_404_on_the_commit_lookup_map() {
        use crate::github::{classify_gh_failure, GhFailureKind};

        for message in [
            "gh command failed: gh: No commit found for SHA: deadbeef (HTTP 404)",
            "gh command failed: gh: No commit found for SHA: deadbeef",
            "gh command failed: HTTP 404: Not Found \
             (https://api.github.com/repos/owner/repo/commits/deadbeef/pulls)",
        ] {
            let error = anyhow!(message);
            assert_eq!(
                classify_gh_failure(&error),
                GhFailureKind::Other,
                "{message}"
            );
            assert_eq!(
                classify_gh_command_error(&error),
                CommitPrLookupError::CommitNotOnGitHub,
                "{message}"
            );
        }

        let rate_limited_404 = anyhow!("API rate limit exceeded (HTTP 404)");
        assert_eq!(
            classify_gh_failure(&rate_limited_404),
            GhFailureKind::RateLimited
        );
        assert_eq!(
            classify_gh_command_error(&rate_limited_404),
            CommitPrLookupError::RateLimited
        );
    }
}
