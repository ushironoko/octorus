mod client;
pub mod comment;
mod commit;
mod pr;

// Explicit re-exports - only export what is actually used
pub use client::{detect_repo, gh_command, DetectRepoError};
pub use comment::{create_multiline_review_comment, create_reply_comment, create_review_comment};
pub use commit::{
    fetch_commit_diff, fetch_local_commit_diff, fetch_local_commits, fetch_pr_commits,
    format_relative_time, CommitListPage, PrCommit,
};

pub use pr::{
    fetch_changed_files, fetch_files_viewed_state, fetch_pr, fetch_pr_checks, fetch_pr_diff,
    fetch_pr_list, fetch_pr_list_with_offset, mark_file_as_viewed, submit_review,
    unmark_file_as_viewed, Branch, ChangedFile, CheckItem, CiStatus, Label, PrListPage,
    PrStateFilter, PullRequest, PullRequestSummary, StatusCheckRollupItem, User,
};
