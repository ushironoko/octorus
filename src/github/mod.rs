mod client;
pub mod comment;
mod commit;
mod dashboard;

macro_rules! define_state_filter {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub enum $name {
            #[default]
            Open,
            Closed,
            All,
        }

        impl $name {
            pub fn as_gh_arg(&self) -> &'static str {
                match self {
                    Self::Open => "open",
                    Self::Closed => "closed",
                    Self::All => "all",
                }
            }

            pub fn display_name(&self) -> &'static str {
                match self {
                    Self::Open => "open",
                    Self::Closed => "closed",
                    Self::All => "all",
                }
            }

            pub fn next(&self) -> Self {
                match self {
                    Self::Open => Self::Closed,
                    Self::Closed => Self::All,
                    Self::All => Self::Open,
                }
            }
        }
    };
}

mod issue;
mod pr;

pub use client::{detect_repo, gh_command, DetectRepoError};
pub use comment::{create_multiline_review_comment, create_reply_comment, create_review_comment};
pub use commit::{
    fetch_commit_diff, fetch_local_commit_diff, fetch_local_commits, fetch_pr_commits,
    format_relative_time, CommitListPage, PrCommit,
};
pub use dashboard::{fetch_mentioned_issues_count, fetch_review_requested_prs_count};

pub use issue::{
    build_reply_template, create_issue_comment, fetch_issue_detail, fetch_issue_list,
    fetch_issue_list_with_offset, fetch_linked_prs, parse_issue_comments, IssueComment,
    IssueDetail, IssueListPage, IssueStateFilter, IssueSummary, LinkedPr,
};

pub use pr::{
    fetch_changed_files, fetch_files_viewed_state, fetch_pr, fetch_pr_checks, fetch_pr_diff,
    fetch_pr_list, fetch_pr_list_with_offset, set_file_viewed, submit_review, Branch, ChangedFile,
    CheckItem, CiStatus, Label, PrListPage, PrStateFilter, PullRequest, PullRequestSummary,
    StatusCheckRollupItem, User,
};
