use anyhow::Result;
use chrono::DateTime;
use serde::Serialize;

use octorus::cache::LocalReviewComment;
use octorus::{cache, github};

#[derive(Debug, Clone, Serialize)]
struct LocalCommentsOutput {
    repo: String,
    working_dir: String,
    total_comments: usize,
    open_comments: usize,
    resolved_comments: usize,
    shown_comments: usize,
    filter: LocalCommentsFilter,
    comments: Vec<LocalReviewComment>,
}

#[derive(Debug, Clone, Serialize)]
struct UpdateLocalCommentsOutput {
    repo: String,
    working_dir: String,
    action: LocalCommentAction,
    updated_ids: Vec<u64>,
    missing_ids: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LocalCommentAction {
    Resolve,
    Reopen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LocalCommentsFilter {
    Open,
    Resolved,
    All,
}

impl LocalCommentAction {
    fn past_tense(self) -> &'static str {
        match self {
            Self::Resolve => "Resolved",
            Self::Reopen => "Reopened",
        }
    }
}

pub async fn show_local_comments_command(
    repo: Option<String>,
    working_dir: Option<String>,
    limit: usize,
    json: bool,
    all: bool,
    resolved: bool,
) -> Result<()> {
    let repo = resolve_repo(repo).await;
    let working_dir = cache::effective_working_dir(working_dir.as_deref())?;
    let filter = local_comments_filter(all, resolved);

    let comments = cache::load_local_review_comments(&repo, Some(&working_dir))?;
    let total_comments = comments.len();
    let open_comments = comments
        .iter()
        .filter(|comment| !comment.meta.is_resolved)
        .count();
    let resolved_comments = total_comments.saturating_sub(open_comments);
    let comments = select_latest_local_comments(filter_local_comments(comments, filter), limit);

    if json {
        let payload = LocalCommentsOutput {
            repo,
            working_dir,
            total_comments,
            open_comments,
            resolved_comments,
            shown_comments: comments.len(),
            filter,
            comments,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    print!(
        "{}",
        format_local_comments_text(
            &repo,
            &working_dir,
            total_comments,
            open_comments,
            filter,
            &comments,
        )
    );
    Ok(())
}

pub async fn purge_local_comments_command(
    repo: Option<String>,
    working_dir: Option<String>,
) -> Result<()> {
    let repo = resolve_repo(repo).await;
    let working_dir = cache::effective_working_dir(working_dir.as_deref())?;
    let removed = cache::delete_local_review_comments(&repo, Some(&working_dir))?;
    println!(
        "Purged {} local comment{} for {} ({})",
        removed,
        if removed == 1 { "" } else { "s" },
        repo,
        working_dir,
    );
    Ok(())
}

pub async fn update_local_comments_command(
    repo: Option<String>,
    working_dir: Option<String>,
    resolve: bool,
    reopen: bool,
    ids: Vec<u64>,
) -> Result<()> {
    let action = match (resolve, reopen) {
        (true, false) => LocalCommentAction::Resolve,
        (false, true) => LocalCommentAction::Reopen,
        _ => anyhow::bail!("Specify exactly one action: --resolve or --reopen"),
    };

    let repo = resolve_repo(repo).await;
    let working_dir = cache::effective_working_dir(working_dir.as_deref())?;

    let mut comments = cache::load_local_review_comments(&repo, Some(&working_dir))?;
    let result = update_local_comments(&mut comments, &ids, action);
    cache::save_local_review_comments(&repo, Some(&working_dir), &comments)?;

    let payload = UpdateLocalCommentsOutput {
        repo,
        working_dir,
        action,
        updated_ids: result.updated_ids,
        missing_ids: result.missing_ids,
    };
    print!("{}", format_update_local_comments_text(&payload));
    if !payload.missing_ids.is_empty() {
        anyhow::bail!(
            "{} unknown local comment ID{}: {}",
            payload.missing_ids.len(),
            if payload.missing_ids.len() == 1 {
                ""
            } else {
                "s"
            },
            join_ids(&payload.missing_ids),
        );
    }
    Ok(())
}

async fn resolve_repo(repo: Option<String>) -> String {
    match repo {
        Some(repo) => repo,
        None => github::detect_repo()
            .await
            .unwrap_or_else(|_| "local".to_string()),
    }
}

fn local_comments_filter(all: bool, resolved: bool) -> LocalCommentsFilter {
    if all {
        LocalCommentsFilter::All
    } else if resolved {
        LocalCommentsFilter::Resolved
    } else {
        LocalCommentsFilter::Open
    }
}

fn filter_local_comments(
    comments: Vec<LocalReviewComment>,
    filter: LocalCommentsFilter,
) -> Vec<LocalReviewComment> {
    comments
        .into_iter()
        .filter(|comment| match filter {
            LocalCommentsFilter::Open => !comment.meta.is_resolved,
            LocalCommentsFilter::Resolved => comment.meta.is_resolved,
            LocalCommentsFilter::All => true,
        })
        .collect()
}

fn select_latest_local_comments(
    mut comments: Vec<LocalReviewComment>,
    limit: usize,
) -> Vec<LocalReviewComment> {
    comments.sort_by(|a, b| {
        parse_comment_timestamp(&b.comment.created_at)
            .cmp(&parse_comment_timestamp(&a.comment.created_at))
            .then_with(|| b.comment.id.cmp(&a.comment.id))
    });
    comments.truncate(limit);
    comments
}

fn parse_comment_timestamp(created_at: &str) -> Option<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(created_at).ok()
}

fn format_local_comments_text(
    repo: &str,
    working_dir: &str,
    total_comments: usize,
    open_comments: usize,
    filter: LocalCommentsFilter,
    comments: &[LocalReviewComment],
) -> String {
    if total_comments == 0 {
        return format!("No local comments found for {} ({})\n", repo, working_dir);
    }

    let resolved_comments = total_comments.saturating_sub(open_comments);
    let filter_label = match filter {
        LocalCommentsFilter::Open => "open",
        LocalCommentsFilter::Resolved => "resolved",
        LocalCommentsFilter::All => "all",
    };

    if comments.is_empty() {
        return format!(
            "No {} local comments for {} ({}) [open: {}, resolved: {}, total: {}]\n",
            filter_label, repo, working_dir, open_comments, resolved_comments, total_comments,
        );
    }

    let mut out = format!(
        "Showing {} comment{} ({}) for {} ({}) [open: {}, resolved: {}, total: {}]\n\n",
        comments.len(),
        if comments.len() == 1 { "" } else { "s" },
        filter_label,
        repo,
        working_dir,
        open_comments,
        resolved_comments,
        total_comments,
    );

    for entry in comments {
        let comment = &entry.comment;
        let line = comment
            .line
            .map(|line| line.to_string())
            .unwrap_or_else(|| "-".to_string());
        let status = if entry.meta.is_resolved {
            "resolved"
        } else {
            "open"
        };
        out.push_str(&format!(
            "#{} [{}] {} {}:{} {}\n",
            comment.id, status, comment.created_at, comment.path, line, comment.user.login
        ));
        for body_line in comment.body.lines() {
            out.push_str("  ");
            out.push_str(body_line);
            out.push('\n');
        }
        if comment.body.is_empty() {
            out.push_str("  \n");
        }
        out.push('\n');
    }

    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalCommentUpdateResult {
    updated_ids: Vec<u64>,
    missing_ids: Vec<u64>,
}

fn update_local_comments(
    comments: &mut [LocalReviewComment],
    ids: &[u64],
    action: LocalCommentAction,
) -> LocalCommentUpdateResult {
    let mut updated_ids = Vec::new();
    let mut missing_ids = Vec::new();

    for id in ids {
        let Some(entry) = comments.iter_mut().find(|entry| entry.comment.id == *id) else {
            missing_ids.push(*id);
            continue;
        };

        match action {
            LocalCommentAction::Resolve => {
                entry.meta.is_resolved = true;
                entry.meta.resolved_at = Some(chrono::Utc::now().to_rfc3339());
            }
            LocalCommentAction::Reopen => {
                entry.meta.is_resolved = false;
                entry.meta.resolved_at = None;
            }
        }
        updated_ids.push(*id);
    }

    LocalCommentUpdateResult {
        updated_ids,
        missing_ids,
    }
}

fn format_update_local_comments_text(payload: &UpdateLocalCommentsOutput) -> String {
    let mut out = format!(
        "{} {} local comment{} for {} ({})\n",
        payload.action.past_tense(),
        payload.updated_ids.len(),
        if payload.updated_ids.len() == 1 {
            ""
        } else {
            "s"
        },
        payload.repo,
        payload.working_dir
    );

    if !payload.updated_ids.is_empty() {
        out.push_str(&format!(
            "Updated IDs: {}\n",
            join_ids(&payload.updated_ids)
        ));
    }
    if !payload.missing_ids.is_empty() {
        out.push_str(&format!(
            "Missing IDs: {}\n",
            join_ids(&payload.missing_ids)
        ));
    }

    out
}

fn join_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use octorus::cache::{LocalCommentMeta, LocalReviewComment};
    use octorus::github::comment::ReviewComment;
    use octorus::github::User;

    fn open_comment(
        id: u64,
        path: &str,
        line: u32,
        body: &str,
        user: &str,
        created_at: &str,
    ) -> LocalReviewComment {
        LocalReviewComment::new(ReviewComment {
            id,
            path: path.to_string(),
            line: Some(line),
            start_line: None,
            body: body.to_string(),
            user: User {
                login: user.to_string(),
            },
            created_at: created_at.to_string(),
            in_reply_to_id: None,
        })
    }

    fn resolved_comment(
        id: u64,
        path: &str,
        line: u32,
        body: &str,
        user: &str,
        created_at: &str,
        resolved_at: &str,
    ) -> LocalReviewComment {
        LocalReviewComment::with_meta(
            ReviewComment {
                id,
                path: path.to_string(),
                line: Some(line),
                start_line: None,
                body: body.to_string(),
                user: User {
                    login: user.to_string(),
                },
                created_at: created_at.to_string(),
                in_reply_to_id: None,
            },
            LocalCommentMeta {
                is_resolved: true,
                resolved_at: Some(resolved_at.to_string()),
            },
        )
    }

    #[test]
    fn test_select_latest_local_comments_orders_newest_first() {
        let comments = vec![
            open_comment(
                1,
                "src/a.rs",
                10,
                "older",
                "alice",
                "2026-03-25T01:00:00+00:00",
            ),
            open_comment(
                2,
                "src/b.rs",
                20,
                "newer",
                "bob",
                "2026-03-25T02:00:00+00:00",
            ),
        ];

        let selected = select_latest_local_comments(comments, 10);

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].comment.id, 2);
        assert_eq!(selected[1].comment.id, 1);
    }

    #[test]
    fn test_select_latest_local_comments_applies_limit() {
        let comments = vec![
            open_comment(
                1,
                "src/a.rs",
                10,
                "first",
                "alice",
                "2026-03-25T01:00:00+00:00",
            ),
            open_comment(
                2,
                "src/b.rs",
                20,
                "second",
                "bob",
                "2026-03-25T02:00:00+00:00",
            ),
        ];

        let selected = select_latest_local_comments(comments, 1);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].comment.id, 2);
    }

    #[test]
    fn test_format_local_comments_text_includes_comment_details() {
        let comments = vec![open_comment(
            7,
            "src/main.rs",
            42,
            "why is this here?",
            "dacuna",
            "2026-03-25T02:00:00+00:00",
        )];

        let output = format_local_comments_text(
            "owner/repo",
            "/tmp/worktree",
            1,
            1,
            LocalCommentsFilter::Open,
            &comments,
        );

        assert!(output.contains(
            "Showing 1 comment (open) for owner/repo (/tmp/worktree) [open: 1, resolved: 0, total: 1]"
        ));
        assert!(output.contains("#7 [open] 2026-03-25T02:00:00+00:00 src/main.rs:42 dacuna"));
        assert!(output.contains("  why is this here?"));
    }

    #[test]
    fn test_format_local_comments_text_handles_empty_state() {
        let output = format_local_comments_text(
            "owner/repo",
            "/tmp/worktree",
            0,
            0,
            LocalCommentsFilter::Open,
            &[],
        );

        assert_eq!(
            output,
            "No local comments found for owner/repo (/tmp/worktree)\n"
        );
    }

    #[test]
    fn test_format_local_comments_text_handles_empty_filtered_state() {
        let output = format_local_comments_text(
            "owner/repo",
            "/tmp/worktree",
            3,
            0,
            LocalCommentsFilter::Open,
            &[],
        );

        assert_eq!(
            output,
            "No open local comments for owner/repo (/tmp/worktree) [open: 0, resolved: 3, total: 3]\n"
        );
    }

    #[test]
    fn test_update_local_comments_resolves_and_reopens() {
        let mut comments = vec![open_comment(
            7,
            "src/main.rs",
            42,
            "why is this here?",
            "dacuna",
            "2026-03-25T02:00:00+00:00",
        )];

        let resolved = update_local_comments(&mut comments, &[7], LocalCommentAction::Resolve);
        assert_eq!(resolved.updated_ids, vec![7]);
        assert!(resolved.missing_ids.is_empty());
        assert!(comments[0].meta.is_resolved);
        assert!(comments[0].meta.resolved_at.is_some());

        let reopened = update_local_comments(&mut comments, &[7], LocalCommentAction::Reopen);
        assert_eq!(reopened.updated_ids, vec![7]);
        assert!(reopened.missing_ids.is_empty());
        assert!(!comments[0].meta.is_resolved);
        assert!(comments[0].meta.resolved_at.is_none());
    }

    #[test]
    fn test_update_local_comments_reports_missing_ids() {
        let mut comments = vec![open_comment(
            1,
            "src/main.rs",
            1,
            "hello",
            "dacuna",
            "2026-03-25T02:00:00+00:00",
        )];

        let result = update_local_comments(&mut comments, &[1, 2], LocalCommentAction::Resolve);

        assert_eq!(result.updated_ids, vec![1]);
        assert_eq!(result.missing_ids, vec![2]);
    }

    #[test]
    fn test_filter_local_comments_defaults_to_open() {
        let comments = vec![
            open_comment(
                1,
                "src/main.rs",
                1,
                "open",
                "dacuna",
                "2026-03-25T01:00:00+00:00",
            ),
            resolved_comment(
                2,
                "src/main.rs",
                2,
                "resolved",
                "dacuna",
                "2026-03-25T02:00:00+00:00",
                "2026-03-25T03:00:00+00:00",
            ),
        ];

        let filtered = filter_local_comments(comments, LocalCommentsFilter::Open);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].comment.id, 1);
    }

    #[test]
    fn test_filter_local_comments_resolved_only() {
        let comments = vec![
            open_comment(
                1,
                "src/main.rs",
                1,
                "open",
                "dacuna",
                "2026-03-25T01:00:00+00:00",
            ),
            resolved_comment(
                2,
                "src/main.rs",
                2,
                "resolved",
                "dacuna",
                "2026-03-25T02:00:00+00:00",
                "2026-03-25T03:00:00+00:00",
            ),
        ];

        let filtered = filter_local_comments(comments, LocalCommentsFilter::Resolved);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].comment.id, 2);
    }

    #[test]
    fn test_snapshot_format_local_comments_text_with_comments() {
        let comments = vec![open_comment(
            7,
            "src/main.rs",
            42,
            "why is this here?",
            "dacuna",
            "2026-03-25T02:00:00+00:00",
        )];

        assert_snapshot!(
            format_local_comments_text(
                "owner/repo",
                "/tmp/worktree",
                1,
                1,
                LocalCommentsFilter::Open,
                &comments,
            ),
            @"
        Showing 1 comment (open) for owner/repo (/tmp/worktree) [open: 1, resolved: 0, total: 1]

        #7 [open] 2026-03-25T02:00:00+00:00 src/main.rs:42 dacuna
          why is this here?
        "
        );
    }

    #[test]
    fn test_snapshot_format_update_local_comments_text() {
        let payload = UpdateLocalCommentsOutput {
            repo: "owner/repo".to_string(),
            working_dir: "/tmp/worktree".to_string(),
            action: LocalCommentAction::Resolve,
            updated_ids: vec![3, 7],
            missing_ids: vec![99],
        };

        assert_snapshot!(format_update_local_comments_text(&payload), @"
        Resolved 2 local comments for owner/repo (/tmp/worktree)
        Updated IDs: 3, 7
        Missing IDs: 99
        ");
    }
}
