use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::warn;

use crate::config::{AiConfig, ProposalPostStrategy};
use crate::github;
use crate::github::comment::{fetch_discussion_comments, fetch_review_comments};

use super::adapter::{
    AgentAdapter, Context, ExternalComment, ReviewAction, RevieweeOutput, RevieweeProposal,
    RevieweeStatus, ReviewerOutput,
};
use super::adapters::create_adapter;
use super::prompt_loader::PromptLoader;
use super::prompts::{
    build_clarification_prompt, build_clarification_skipped_prompt, build_permission_denied_prompt,
    build_permission_granted_prompt,
};
use super::session::{write_history_entry, write_session, HistoryEntryType, RallySession};

/// Bot suffixes to identify bot users
const BOT_SUFFIXES: &[&str] = &["[bot]"];
/// Exact bot user names
const BOT_EXACT_MATCHES: &[&str] = &["github-actions", "dependabot"];
/// Maximum number of external comments to include in context
const MAX_EXTERNAL_COMMENTS: usize = 20;

/// Git subcommands that are safe (read-only or local-only operations)
/// for the reviewee to execute. Any git subcommand not in this list
/// is blocked when validating permission requests in local mode.
const ALLOWED_GIT_SUBCOMMANDS: &[&str] = &[
    "status", "diff", "add", "commit", "log", "show", "branch", "switch", "stash",
];

/// Rally state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RallyState {
    Initializing,
    ReviewerReviewing,
    RevieweeFix,
    /// Reviewee is designing a fix proposal (review_only / proposal iteration mode).
    /// No code mutation is performed in this state.
    RevieweeProposing,
    WaitingForClarification,
    WaitingForPermission,
    WaitingForPostConfirmation,
    Completed,
    Aborted,
    Error,
}

impl RallyState {
    /// Rally が実行中（完了・エラー・中断以外）かどうか
    // Kept for state inspection in tests and future multi-agent coordination
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        match self {
            Self::Initializing
            | Self::ReviewerReviewing
            | Self::RevieweeFix
            | Self::RevieweeProposing
            | Self::WaitingForClarification
            | Self::WaitingForPermission
            | Self::WaitingForPostConfirmation => true,
            Self::Completed | Self::Aborted | Self::Error => false,
        }
    }

    /// Rally が完了、中断、またはエラーで終了したかどうか
    // Kept for state inspection in tests and future multi-agent coordination
    #[allow(dead_code)]
    pub fn is_finished(&self) -> bool {
        match self {
            Self::Completed | Self::Aborted | Self::Error => true,
            Self::Initializing
            | Self::ReviewerReviewing
            | Self::RevieweeFix
            | Self::RevieweeProposing
            | Self::WaitingForClarification
            | Self::WaitingForPermission
            | Self::WaitingForPostConfirmation => false,
        }
    }
}

/// Event emitted during rally for TUI updates
///
/// Variants are used by TUI handlers (ui/ai_rally.rs) via mpsc channel
// Variants constructed by the orchestrator run loop, fields read only in specific flows
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RallyEvent {
    StateChanged(RallyState),
    /// Rally has just started. Emitted exactly once at the beginning of `run()`,
    /// before any iteration begins. Consumers use this to surface startup-time
    /// context such as review-only mode without polling config.
    RallyStarted {
        review_only: bool,
    },
    IterationStarted(u32),
    ReviewCompleted(ReviewerOutput),
    FixCompleted(RevieweeOutput),
    ClarificationNeeded(String),
    PermissionNeeded(String, String), // action, reason
    Approved(String),                 // summary
    /// Rally finished review-only proposal iteration mode without reaching Approve.
    /// Emitted when `max_iterations` is hit. Carries the final reviewer verdict so
    /// consumers can inspect the last action and summary.
    ReviewOnlyCompleted(ReviewerOutput),
    /// Reviewee produced a fix proposal in review_only mode.
    /// Emitted once per iteration after the reviewee proposal step completes.
    ProposalCompleted(RevieweeProposal),
    ReviewPostConfirmNeeded(ReviewPostInfo),
    FixPostConfirmNeeded(FixPostInfo),
    /// Post confirmation needed before posting a reviewee proposal to the PR.
    ProposalPostConfirmNeeded(ProposalPostInfo),
    Error(String),
    Log(String),
    /// Orchestrator has paused at a checkpoint
    Paused,
    /// Orchestrator has resumed from paused state
    Resumed,
    // Streaming events from Claude
    AgentThinking(String),           // thinking content
    AgentToolUse(String, String),    // tool_name, input_summary
    AgentToolResult(String, String), // tool_name, result_summary
    AgentText(String),               // text output
}

/// Result of the rally process
///
/// Used by app.rs to handle rally completion state
// Constructed at rally completion, consumed by the caller in headless mode
#[derive(Debug)]
#[allow(dead_code)]
pub enum RallyResult {
    Approved {
        iteration: u32,
        summary: String,
    },
    /// Review-only proposal-iteration mode terminated without reaching Approve.
    /// `iteration` is the total number of reviewer cycles executed (1..=max_iterations).
    /// `action` and `summary` come from the final reviewer verdict.
    ReviewOnlyCompleted {
        iteration: u32,
        action: ReviewAction,
        summary: String,
    },
    MaxIterationsReached {
        iteration: u32,
    },
    Aborted {
        iteration: u32,
        reason: String,
    },
    Error {
        iteration: u32,
        error: String,
    },
}

/// Lightweight DTO for review post confirmation (sent via RallyEvent)
#[derive(Debug, Clone)]
pub struct ReviewPostInfo {
    pub action: String,
    pub summary: String,
    pub comment_count: usize,
}

/// Lightweight DTO for fix post confirmation (sent via RallyEvent)
#[derive(Debug, Clone)]
pub struct FixPostInfo {
    pub summary: String,
    pub files_modified: Vec<String>,
}

/// Lightweight DTO for reviewee proposal post confirmation (sent via RallyEvent).
/// Carries enough info for the UI to render a confirmation dialog without owning
/// the full proposal payload.
#[derive(Debug, Clone)]
pub struct ProposalPostInfo {
    pub summary: String,
    pub target_files: Vec<String>,
    pub plan_item_count: usize,
}

/// Command sent from TUI to Orchestrator
#[derive(Debug)]
pub enum OrchestratorCommand {
    /// User provided clarification answer
    ClarificationResponse(String),
    /// User granted or denied permission
    PermissionResponse(bool),
    /// User chose to skip clarification (continue with best judgment)
    SkipClarification,
    /// User approved or skipped post confirmation
    PostConfirmResponse(bool),
    /// User requested abort (stop the rally entirely)
    Abort,
    /// User requested pause (take effect at next checkpoint)
    Pause,
    /// User requested resume from paused state
    Resume,
}

/// Main orchestrator for AI rally
pub struct Orchestrator {
    repo: String,
    pr_number: u32,
    config: AiConfig,
    reviewer_adapter: Box<dyn AgentAdapter>,
    reviewee_adapter: Box<dyn AgentAdapter>,
    session: RallySession,
    context: Option<Context>,
    seed_review: Option<ReviewerOutput>,
    last_review: Option<ReviewerOutput>,
    last_fix: Option<RevieweeOutput>,
    /// Last reviewee proposal (review_only mode). Used to feed back into the
    /// next reviewer iteration as the re-review subject.
    last_proposal: Option<RevieweeProposal>,
    /// Buffered proposal for `ProposalPostStrategy::Final`. Holds the most
    /// recent proposal so it can be posted exactly once at rally termination
    /// (either on Approve or on max_iterations).
    last_unposted_proposal: Option<RevieweeProposal>,
    event_sender: mpsc::Sender<RallyEvent>,
    prompt_loader: PromptLoader,
    /// Command receiver for TUI commands
    command_receiver: Option<mpsc::Receiver<OrchestratorCommand>>,
    /// Whether a pause has been requested (checked at checkpoints)
    paused: bool,
}

/// Result of running a single iteration in review_only proposal-iteration mode.
enum ReviewOnlyOutcome {
    /// Continue to the next reviewer cycle.
    Continue,
    /// Terminate the rally with this result.
    Terminate(RallyResult),
}

impl Orchestrator {
    pub fn new(
        repo: &str,
        pr_number: u32,
        config: AiConfig,
        event_sender: mpsc::Sender<RallyEvent>,
        command_receiver: Option<mpsc::Receiver<OrchestratorCommand>>,
        prompt_loader: PromptLoader,
    ) -> Result<Self> {
        let mut reviewer_adapter = create_adapter(&config.reviewer, &config)?;
        let mut reviewee_adapter = create_adapter(&config.reviewee, &config)?;

        // Set event sender for streaming events
        reviewer_adapter.set_event_sender(event_sender.clone());
        reviewee_adapter.set_event_sender(event_sender.clone());

        let session = RallySession::new(repo, pr_number);

        Ok(Self {
            repo: repo.to_string(),
            pr_number,
            config,
            reviewer_adapter,
            reviewee_adapter,
            session,
            context: None,
            seed_review: None,
            last_review: None,
            last_fix: None,
            last_proposal: None,
            last_unposted_proposal: None,
            event_sender,
            prompt_loader,
            command_receiver,
            paused: false,
        })
    }

    /// Set the context for the rally
    pub fn set_context(&mut self, context: Context) {
        // Propagate local_mode to both adapters so they can enforce
        // git write restrictions at the tool level
        self.reviewer_adapter.set_local_mode(context.local_mode);
        self.reviewee_adapter.set_local_mode(context.local_mode);
        self.context = Some(context);
    }

    /// Seed the first review step with an existing review result.
    pub fn set_seed_review(&mut self, review: ReviewerOutput) {
        self.seed_review = Some(review);
    }

    /// Run the rally process
    pub async fn run(&mut self) -> Result<RallyResult> {
        let context = self
            .context
            .as_ref()
            .ok_or_else(|| anyhow!("Context not set"))?
            .clone();

        self.send_event(RallyEvent::RallyStarted {
            review_only: self.config.review_only,
        })
        .await;
        self.send_event(RallyEvent::StateChanged(RallyState::Initializing))
            .await;

        // Main loop
        while self.session.iteration < self.config.max_iterations {
            self.session.increment_iteration();
            let iteration = self.session.iteration;

            self.send_event(RallyEvent::IterationStarted(iteration))
                .await;

            // Update head_sha at start of each iteration.
            // Note: The reviewee does NOT push changes; commits are local only.
            // This update is primarily for when the user manually pushes changes between iterations,
            // or when external tools/CI update the PR branch.
            if iteration > 1 {
                if let Err(e) = self.update_head_sha().await {
                    warn!("Failed to update head_sha: {}", e);
                }
            }

            let (review_result, seeded_review) = if iteration == 1 && self.seed_review.is_some() {
                let review = self.seed_review.take().unwrap();
                self.send_event(RallyEvent::Log(format!(
                    "Using {} local comment{} as the initial review seed",
                    review.comments.len(),
                    if review.comments.len() == 1 { "" } else { "s" }
                )))
                .await;
                (review, true)
            } else {
                let review = self.run_reviewer_step(&context, iteration).await?;
                (review, false)
            };

            // Store the review for later use
            if let Err(e) = write_history_entry(
                &self.repo,
                self.pr_number,
                iteration,
                &HistoryEntryType::Review(review_result.clone()),
            ) {
                warn!("Failed to write review history: {}", e);
                self.send_event(RallyEvent::Log(format!(
                    "Warning: Failed to write review history: {}",
                    e
                )))
                .await;
            }

            self.send_event(RallyEvent::ReviewCompleted(review_result.clone()))
                .await;
            self.last_review = Some(review_result.clone());

            if !seeded_review {
                // Update head_sha before posting review (ensure we have the latest commit)
                if let Err(e) = self.update_head_sha().await {
                    warn!("Failed to update head_sha before posting review: {}", e);
                }

                // Post review to PR (with confirmation if auto_post is false)
                if let Err(e) = self.maybe_post_review_to_pr(&review_result).await {
                    // Check if abort was triggered during post confirmation
                    if self.session.state == RallyState::Aborted {
                        return Ok(RallyResult::Aborted {
                            iteration,
                            reason: e.to_string(),
                        });
                    }
                    warn!("Failed to post review to PR: {}", e);
                    self.send_event(RallyEvent::Log(format!(
                        "Warning: Failed to post review to PR: {}",
                        e
                    )))
                    .await;
                }
            }

            // Check for approval
            if review_result.action == ReviewAction::Approve {
                // Clear any pending pause before entering terminal state
                self.paused = false;

                // In review_only Final mode, flush any buffered proposal so the
                // last accepted plan is posted to the PR before the Approved
                // event terminates the rally.
                if self.config.review_only {
                    self.flush_final_proposal_if_buffered().await;
                }

                self.session.update_state(RallyState::Completed);
                if let Err(e) = write_session(&self.session) {
                    warn!("Failed to write session: {}", e);
                }

                self.send_event(RallyEvent::Approved(review_result.summary.clone()))
                    .await;
                self.send_event(RallyEvent::StateChanged(RallyState::Completed))
                    .await;

                return Ok(RallyResult::Approved {
                    iteration,
                    summary: review_result.summary,
                });
            }

            // Review-only mode: enter the proposal iteration sub-flow.
            // The reviewee fix phase is never entered; instead the reviewee
            // produces a RevieweeProposal that the reviewer re-evaluates on
            // the next iteration.
            if self.config.review_only {
                match self
                    .run_review_only_iteration(iteration, &review_result)
                    .await?
                {
                    ReviewOnlyOutcome::Continue => continue,
                    ReviewOnlyOutcome::Terminate(result) => return Ok(result),
                }
            }

            // Checkpoint: pause before starting reviewee if requested
            if let Some(result) = self.check_pause_at_checkpoint(iteration).await {
                return Ok(result);
            }

            // Run reviewee to fix issues
            self.session.update_state(RallyState::RevieweeFix);
            self.send_event(RallyEvent::StateChanged(RallyState::RevieweeFix))
                .await;
            if let Err(e) = write_session(&self.session) {
                warn!("Failed to write session: {}", e);
                self.send_event(RallyEvent::Log(format!(
                    "Warning: Failed to write session: {}",
                    e
                )))
                .await;
            }

            // Fetch external comments before reviewee starts
            let external_comments = self.fetch_external_comments().await;
            if !external_comments.is_empty() {
                self.send_event(RallyEvent::Log(format!(
                    "Fetched {} external bot comments",
                    external_comments.len()
                )))
                .await;
            }
            if let Some(ref mut ctx) = self.context {
                ctx.external_comments = external_comments;
            }

            // Get updated context with external comments
            let context = self
                .context
                .as_ref()
                .ok_or_else(|| anyhow!("Context not set"))?
                .clone();

            let fix_result = match self
                .run_reviewee_with_timeout(&context, &review_result, iteration)
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    self.session.update_state(RallyState::Error);
                    let _ = write_session(&self.session);
                    self.send_event(RallyEvent::Error(format!("Reviewee failed: {:#}", e)))
                        .await;
                    self.send_event(RallyEvent::StateChanged(RallyState::Error))
                        .await;
                    return Err(e);
                }
            };

            if let Err(e) = write_history_entry(
                &self.repo,
                self.pr_number,
                iteration,
                &HistoryEntryType::Fix(fix_result.clone()),
            ) {
                warn!("Failed to write fix history: {}", e);
                self.send_event(RallyEvent::Log(format!(
                    "Warning: Failed to write fix history: {}",
                    e
                )))
                .await;
            }

            self.send_event(RallyEvent::FixCompleted(fix_result.clone()))
                .await;

            // Handle reviewee status
            match fix_result.status {
                RevieweeStatus::Completed => {
                    // Store the fix result for the next re-review
                    self.last_fix = Some(fix_result.clone());

                    // Post fix summary to PR (with confirmation if auto_post is false)
                    if let Err(e) = self.maybe_post_fix_comment(&fix_result).await {
                        // Check if abort was triggered during post confirmation
                        if self.session.state == RallyState::Aborted {
                            return Ok(RallyResult::Aborted {
                                iteration,
                                reason: e.to_string(),
                            });
                        }
                        warn!("Failed to post fix comment to PR: {}", e);
                        self.send_event(RallyEvent::Log(format!(
                            "Warning: Failed to post fix comment to PR: {}",
                            e
                        )))
                        .await;
                    }

                    // Checkpoint: pause before next iteration if requested
                    if let Some(result) = self.check_pause_at_checkpoint(iteration).await {
                        return Ok(result);
                    }

                    // Continue to next iteration
                }
                RevieweeStatus::NeedsClarification => {
                    if let Some(question) = &fix_result.question {
                        self.session
                            .update_state(RallyState::WaitingForClarification);
                        if let Err(e) = write_session(&self.session) {
                            warn!("Failed to write session: {}", e);
                        }

                        self.send_event(RallyEvent::ClarificationNeeded(question.clone()))
                            .await;
                        self.send_event(RallyEvent::StateChanged(
                            RallyState::WaitingForClarification,
                        ))
                        .await;

                        // Wait for user command (loop to skip stale/invalid commands)
                        loop {
                            match self.wait_for_command().await {
                                Some(OrchestratorCommand::ClarificationResponse(answer)) => {
                                    // Handle clarification response
                                    if let Err(e) =
                                        self.handle_clarification_response(&answer).await
                                    {
                                        self.session.update_state(RallyState::Error);
                                        let _ = write_session(&self.session);
                                        self.send_event(RallyEvent::Error(e.to_string())).await;
                                        self.send_event(RallyEvent::StateChanged(
                                            RallyState::Error,
                                        ))
                                        .await;
                                        return Ok(RallyResult::Error {
                                            iteration,
                                            error: e.to_string(),
                                        });
                                    }
                                    // Continue to next iteration
                                    break;
                                }
                                Some(OrchestratorCommand::SkipClarification) => {
                                    // Clarification skipped - continue with best judgment
                                    self.send_event(RallyEvent::Log(format!(
                                        "Clarification skipped for: {}. Continuing with best judgment...",
                                        question
                                    )))
                                    .await;

                                    let prompt = build_clarification_skipped_prompt(question);
                                    match self.reviewee_adapter.continue_reviewee(&prompt).await {
                                        Ok(output) => {
                                            // Write history entry for the follow-up fix
                                            if let Err(e) = write_history_entry(
                                                &self.repo,
                                                self.pr_number,
                                                iteration,
                                                &HistoryEntryType::Fix(output.clone()),
                                            ) {
                                                warn!(
                                                    "Failed to write follow-up fix history: {}",
                                                    e
                                                );
                                            }

                                            // Post fix comment to PR (with confirmation if auto_post is false)
                                            if let Err(e) =
                                                self.maybe_post_fix_comment(&output).await
                                            {
                                                // Check if abort was triggered during post confirmation
                                                if self.session.state == RallyState::Aborted {
                                                    return Ok(RallyResult::Aborted {
                                                        iteration,
                                                        reason: e.to_string(),
                                                    });
                                                }
                                                warn!(
                                                    "Failed to post follow-up fix comment to PR: {}",
                                                    e
                                                );
                                            }

                                            self.send_event(RallyEvent::FixCompleted(
                                                output.clone(),
                                            ))
                                            .await;
                                            self.last_fix = Some(output);
                                        }
                                        Err(e) => {
                                            self.last_fix = None;
                                            self.send_event(RallyEvent::Log(format!(
                                                "Error continuing after clarification skip: {}. Proceeding to re-review.",
                                                e
                                            )))
                                            .await;
                                        }
                                    }

                                    // Notify TUI of state change
                                    self.session.update_state(RallyState::RevieweeFix);
                                    self.send_event(RallyEvent::StateChanged(
                                        RallyState::RevieweeFix,
                                    ))
                                    .await;
                                    let _ = write_session(&self.session);
                                    // Continue loop
                                    break;
                                }
                                Some(OrchestratorCommand::Abort) | None => {
                                    // True abort - user cancelled or channel closed
                                    let reason = "Clarification cancelled by user".to_string();
                                    self.session.update_state(RallyState::Aborted);
                                    let _ = write_session(&self.session);
                                    self.send_event(RallyEvent::Log(reason.clone())).await;
                                    self.send_event(RallyEvent::StateChanged(RallyState::Aborted))
                                        .await;
                                    return Ok(RallyResult::Aborted { iteration, reason });
                                }
                                _ => {
                                    // Stale/invalid command for this state (e.g. PostConfirmResponse) - ignore and re-wait
                                    warn!("Received invalid command during WaitingForClarification, ignoring");
                                    self.send_event(RallyEvent::Log(
                                        "Received invalid command, still waiting for clarification...".to_string(),
                                    ))
                                    .await;
                                    continue;
                                }
                            }
                        }
                    }
                }
                RevieweeStatus::NeedsPermission => {
                    if let Some(perm) = &fix_result.permission_request {
                        self.session.update_state(RallyState::WaitingForPermission);
                        let _ = write_session(&self.session);

                        self.send_event(RallyEvent::PermissionNeeded(
                            perm.action.clone(),
                            perm.reason.clone(),
                        ))
                        .await;
                        self.send_event(RallyEvent::StateChanged(RallyState::WaitingForPermission))
                            .await;

                        // Wait for user command (loop to skip stale/invalid commands)
                        loop {
                            match self.wait_for_command().await {
                                Some(OrchestratorCommand::PermissionResponse(approved)) => {
                                    if approved {
                                        // Handle permission granted
                                        if let Err(e) =
                                            self.handle_permission_granted(&perm.action).await
                                        {
                                            self.session.update_state(RallyState::Error);
                                            let _ = write_session(&self.session);
                                            self.send_event(RallyEvent::Error(e.to_string())).await;
                                            self.send_event(RallyEvent::StateChanged(
                                                RallyState::Error,
                                            ))
                                            .await;
                                            return Ok(RallyResult::Error {
                                                iteration,
                                                error: e.to_string(),
                                            });
                                        }
                                        // Continue to next iteration
                                    } else {
                                        // Permission denied - continue without this permission
                                        self.send_event(RallyEvent::Log(format!(
                                            "Permission denied for: {}. Continuing without it...",
                                            perm.action
                                        )))
                                        .await;

                                        let prompt = build_permission_denied_prompt(
                                            &perm.action,
                                            &perm.reason,
                                        );
                                        match self.reviewee_adapter.continue_reviewee(&prompt).await
                                        {
                                            Ok(output) => {
                                                // Write history entry for the follow-up fix
                                                if let Err(e) = write_history_entry(
                                                    &self.repo,
                                                    self.pr_number,
                                                    iteration,
                                                    &HistoryEntryType::Fix(output.clone()),
                                                ) {
                                                    warn!(
                                                        "Failed to write follow-up fix history: {}",
                                                        e
                                                    );
                                                }

                                                // Post fix comment to PR (with confirmation if auto_post is false)
                                                if let Err(e) =
                                                    self.maybe_post_fix_comment(&output).await
                                                {
                                                    // Check if abort was triggered during post confirmation
                                                    if self.session.state == RallyState::Aborted {
                                                        return Ok(RallyResult::Aborted {
                                                            iteration,
                                                            reason: e.to_string(),
                                                        });
                                                    }
                                                    warn!("Failed to post follow-up fix comment to PR: {}", e);
                                                }

                                                self.send_event(RallyEvent::FixCompleted(
                                                    output.clone(),
                                                ))
                                                .await;
                                                self.last_fix = Some(output);
                                            }
                                            Err(e) => {
                                                // Clear last_fix to prevent referencing stale value
                                                self.last_fix = None;
                                                self.send_event(RallyEvent::Log(format!(
                                                    "Error continuing after permission denial: {}. Proceeding to re-review.",
                                                    e
                                                )))
                                                .await;
                                            }
                                        }

                                        // Notify TUI of state change
                                        self.session.update_state(RallyState::RevieweeFix);
                                        self.send_event(RallyEvent::StateChanged(
                                            RallyState::RevieweeFix,
                                        ))
                                        .await;
                                        let _ = write_session(&self.session);
                                        // Continue loop
                                    }
                                    break;
                                }
                                Some(OrchestratorCommand::Abort) | None => {
                                    let reason = format!("Permission aborted: {}", perm.action);
                                    self.session.update_state(RallyState::Aborted);
                                    let _ = write_session(&self.session);
                                    self.send_event(RallyEvent::Log(reason.clone())).await;
                                    self.send_event(RallyEvent::StateChanged(RallyState::Aborted))
                                        .await;
                                    return Ok(RallyResult::Aborted { iteration, reason });
                                }
                                _ => {
                                    // Stale/invalid command for this state (e.g. PostConfirmResponse) - ignore and re-wait
                                    warn!("Received invalid command during WaitingForPermission, ignoring");
                                    self.send_event(RallyEvent::Log(
                                        "Received invalid command, still waiting for permission..."
                                            .to_string(),
                                    ))
                                    .await;
                                    continue;
                                }
                            }
                        }
                    }
                }
                RevieweeStatus::Error => {
                    self.session.update_state(RallyState::Error);
                    let _ = write_session(&self.session);

                    let error = fix_result
                        .error_details
                        .unwrap_or_else(|| "Unknown error".to_string());
                    self.send_event(RallyEvent::Error(error.clone())).await;
                    self.send_event(RallyEvent::StateChanged(RallyState::Error))
                        .await;

                    return Ok(RallyResult::Error { iteration, error });
                }
            }
        }

        // Max iterations reached is a terminal state (not an error)
        self.session.update_state(RallyState::Completed);
        if let Err(e) = write_session(&self.session) {
            warn!("Failed to write session: {}", e);
        }

        self.send_event(RallyEvent::Log(format!(
            "Max iterations ({}) reached",
            self.config.max_iterations
        )))
        .await;
        self.send_event(RallyEvent::StateChanged(RallyState::Completed))
            .await;

        Ok(RallyResult::MaxIterationsReached {
            iteration: self.session.iteration,
        })
    }

    /// Wait for a command from the TUI
    async fn wait_for_command(&mut self) -> Option<OrchestratorCommand> {
        let rx = self.command_receiver.as_mut()?;
        rx.recv().await
    }

    /// Check for pause request at a checkpoint and block until resumed if paused.
    ///
    /// Called at natural boundaries (after reviewer completes, after reviewee completes)
    /// to allow the user to inspect state before proceeding.
    /// Returns `Some(RallyResult)` if the rally should terminate (e.g. abort), `None` to continue.
    async fn check_pause_at_checkpoint(&mut self, iteration: u32) -> Option<RallyResult> {
        // 1. Drain Pause/Resume/Abort commands from the channel
        if let Some(ref mut rx) = self.command_receiver {
            loop {
                match rx.try_recv() {
                    Ok(OrchestratorCommand::Pause) => self.paused = true,
                    Ok(OrchestratorCommand::Resume) => self.paused = false,
                    Ok(OrchestratorCommand::Abort) => {
                        self.session.update_state(RallyState::Aborted);
                        let _ = write_session(&self.session);
                        self.send_event(RallyEvent::StateChanged(RallyState::Aborted))
                            .await;
                        return Some(RallyResult::Aborted {
                            iteration,
                            reason: "Aborted by user".to_string(),
                        });
                    }
                    Ok(_) => continue, // stale commands from previous Waiting* states
                    Err(_) => break,   // empty or disconnected
                }
            }
        }

        // 2. Not paused → continue
        if !self.paused {
            return None;
        }

        // 3. Paused → notify TUI and block
        self.send_event(RallyEvent::Paused).await;
        self.send_event(RallyEvent::Log(
            "Rally paused. Press 'p' to resume.".to_string(),
        ))
        .await;

        // 4. Wait for Resume or Abort
        loop {
            match self.wait_for_command().await {
                Some(OrchestratorCommand::Resume) => {
                    self.paused = false;
                    self.send_event(RallyEvent::Resumed).await;
                    self.send_event(RallyEvent::Log("Rally resumed.".to_string()))
                        .await;
                    return None;
                }
                Some(OrchestratorCommand::Abort) | None => {
                    self.session.update_state(RallyState::Aborted);
                    let _ = write_session(&self.session);
                    self.send_event(RallyEvent::StateChanged(RallyState::Aborted))
                        .await;
                    return Some(RallyResult::Aborted {
                        iteration,
                        reason: "Aborted while paused".to_string(),
                    });
                }
                Some(OrchestratorCommand::Pause) => continue, // already paused
                Some(_) => continue,                          // stale commands
            }
        }
    }

    /// Handle clarification response from user
    async fn handle_clarification_response(&mut self, answer: &str) -> Result<()> {
        self.send_event(RallyEvent::Log(format!(
            "User provided clarification: {}",
            answer
        )))
        .await;

        // Ask reviewer for clarification and log the response
        let prompt = build_clarification_prompt(answer);
        let reviewer_response = self.reviewer_adapter.continue_reviewer(&prompt).await?;

        // Log the reviewer's response for debugging/audit purposes
        self.send_event(RallyEvent::Log(format!(
            "Reviewer clarification response: {}",
            reviewer_response.summary
        )))
        .await;

        // Continue reviewee with the answer
        self.reviewee_adapter.continue_reviewee(answer).await?;

        self.session.update_state(RallyState::RevieweeFix);
        self.send_event(RallyEvent::StateChanged(RallyState::RevieweeFix))
            .await;
        let _ = write_session(&self.session);

        Ok(())
    }

    /// Handle permission granted from user
    async fn handle_permission_granted(&mut self, action: &str) -> Result<()> {
        // In local mode, validate that the action doesn't contain blocked git operations.
        // Uses strict token-based parsing to prevent bypasses like
        // `git status && git push` passing a substring-based check.
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            if let Some(reason) = check_blocked_git_operation(action) {
                let msg = format!(
                    "Permission blocked in local mode: {}. Action: {}",
                    reason, action
                );
                warn!("{}", msg);
                self.send_event(RallyEvent::Log(msg.clone())).await;

                // Route to denied flow instead of returning error
                let prompt = build_permission_denied_prompt(action, &reason);
                self.reviewee_adapter.continue_reviewee(&prompt).await?;

                self.session.update_state(RallyState::RevieweeFix);
                self.send_event(RallyEvent::StateChanged(RallyState::RevieweeFix))
                    .await;
                let _ = write_session(&self.session);

                return Ok(());
            }
        }

        self.send_event(RallyEvent::Log(format!(
            "User granted permission for: {}",
            action
        )))
        .await;

        // Add the granted action to reviewee's allowed tools
        // This allows the reviewee to execute the action without being blocked
        self.reviewee_adapter.add_reviewee_allowed_tool(action);

        let prompt = build_permission_granted_prompt(action);
        self.reviewee_adapter.continue_reviewee(&prompt).await?;

        self.session.update_state(RallyState::RevieweeFix);
        self.send_event(RallyEvent::StateChanged(RallyState::RevieweeFix))
            .await;
        let _ = write_session(&self.session);

        Ok(())
    }

    /// Continue after clarification answer (legacy, kept for compatibility)
    #[allow(dead_code)]
    pub async fn continue_with_clarification(&mut self, answer: &str) -> Result<()> {
        self.handle_clarification_response(answer).await
    }

    /// Continue after permission granted (legacy, kept for compatibility)
    #[allow(dead_code)]
    pub async fn continue_with_permission(&mut self, action: &str) -> Result<()> {
        self.handle_permission_granted(action).await
    }

    async fn run_reviewer_step(
        &mut self,
        context: &Context,
        iteration: u32,
    ) -> Result<ReviewerOutput> {
        self.session.update_state(RallyState::ReviewerReviewing);
        self.send_event(RallyEvent::StateChanged(RallyState::ReviewerReviewing))
            .await;
        if let Err(e) = write_session(&self.session) {
            warn!("Failed to write session: {}", e);
            self.send_event(RallyEvent::Log(format!(
                "Warning: Failed to write session: {}",
                e
            )))
            .await;
        }

        match self.run_reviewer_with_timeout(context, iteration).await {
            Ok(result) => Ok(result),
            Err(e) => {
                self.session.update_state(RallyState::Error);
                let _ = write_session(&self.session);
                self.send_event(RallyEvent::Error(format!("Reviewer failed: {:#}", e)))
                    .await;
                self.send_event(RallyEvent::StateChanged(RallyState::Error))
                    .await;
                Err(e)
            }
        }
    }

    async fn run_reviewer_with_timeout(
        &mut self,
        context: &Context,
        iteration: u32,
    ) -> Result<ReviewerOutput> {
        let prompt = if iteration == 1 {
            self.prompt_loader.load_reviewer_prompt(context, iteration)
        } else if self.config.review_only {
            // Proposal-iteration mode: code is unchanged, the reviewer
            // evaluates the previously produced RevieweeProposal. We pass
            // `context.diff` (frozen at session start) as `current_diff`
            // because proposal mode never mutates code, so re-fetching the
            // diff would only introduce drift from external pushes.
            let previous_review = self.last_review.as_ref().ok_or_else(|| {
                anyhow!("review_only re-review: missing previous reviewer output")
            })?;
            let proposal = self
                .last_proposal
                .as_ref()
                .ok_or_else(|| anyhow!("review_only re-review: missing previous proposal"))?;
            self.prompt_loader.load_rereview_proposal_prompt(
                context,
                iteration,
                previous_review,
                proposal,
                &context.diff,
            )
        } else {
            // Normal re-review after fixes - fetch updated diff and include fix summary
            let updated_diff = self.fetch_current_diff().await.unwrap_or_else(|e| {
                warn!("Failed to fetch updated diff: {}", e);
                context.diff.clone()
            });

            let changes_summary = self
                .last_fix
                .as_ref()
                .map(|f| {
                    let files = if f.files_modified.is_empty() {
                        "No files modified".to_string()
                    } else {
                        f.files_modified.join(", ")
                    };
                    format!("{}\n\nFiles modified: {}", f.summary, files)
                })
                .unwrap_or_else(|| "No changes recorded".to_string());
            self.prompt_loader.load_rereview_prompt(
                context,
                iteration,
                &changes_summary,
                &updated_diff,
            )
        };

        let duration = Duration::from_secs(self.config.timeout_secs);

        timeout(
            duration,
            self.reviewer_adapter.run_reviewer(&prompt, context),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "Reviewer timeout after {} seconds",
                self.config.timeout_secs
            )
        })?
    }

    async fn run_reviewee_proposal_with_timeout(
        &mut self,
        context: &Context,
        review: &ReviewerOutput,
        iteration: u32,
    ) -> Result<RevieweeProposal> {
        let prompt = self
            .prompt_loader
            .load_reviewee_proposal_prompt(context, review, iteration);
        let duration = Duration::from_secs(self.config.timeout_secs);

        timeout(
            duration,
            self.reviewee_adapter
                .run_reviewee_proposal(&prompt, context),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "Reviewee proposal timeout after {} seconds",
                self.config.timeout_secs
            )
        })?
    }

    /// One iteration of the review_only proposal flow. Called after the
    /// reviewer has produced a non-Approve verdict for `iteration`.
    ///
    /// - On `iteration >= max_iterations`, terminate with `ReviewOnlyCompleted`
    ///   carrying the final reviewer verdict.
    /// - Otherwise, run the reviewee in proposal mode, write its output to
    ///   history, emit `ProposalCompleted`, store as `last_proposal`, and
    ///   return `Continue` so the main loop moves to the next reviewer cycle.
    /// - On `proposal.status == Error` or adapter `Err`, terminate the rally.
    async fn run_review_only_iteration(
        &mut self,
        iteration: u32,
        review: &ReviewerOutput,
    ) -> Result<ReviewOnlyOutcome> {
        // max_iterations reached: no further proposal, terminate now.
        if iteration >= self.config.max_iterations {
            self.paused = false;

            // Flush any buffered Final proposal before terminating so the
            // best plan reached is posted to the PR.
            self.flush_final_proposal_if_buffered().await;

            self.session.update_state(RallyState::Completed);
            if let Err(e) = write_session(&self.session) {
                warn!("Failed to write session: {}", e);
            }

            let action = review.action;
            let summary = review.summary.clone();
            self.send_event(RallyEvent::ReviewOnlyCompleted(review.clone()))
                .await;
            self.send_event(RallyEvent::StateChanged(RallyState::Completed))
                .await;

            return Ok(ReviewOnlyOutcome::Terminate(
                RallyResult::ReviewOnlyCompleted {
                    iteration,
                    action,
                    summary,
                },
            ));
        }

        // Checkpoint: pause before proposal if requested.
        if let Some(result) = self.check_pause_at_checkpoint(iteration).await {
            return Ok(ReviewOnlyOutcome::Terminate(result));
        }

        // Reviewee designs a fix proposal.
        self.session.update_state(RallyState::RevieweeProposing);
        self.send_event(RallyEvent::StateChanged(RallyState::RevieweeProposing))
            .await;
        if let Err(e) = write_session(&self.session) {
            warn!("Failed to write session: {}", e);
        }

        let external_comments = self.fetch_external_comments().await;
        if let Some(ref mut ctx) = self.context {
            ctx.external_comments = external_comments;
        }
        let context = self
            .context
            .as_ref()
            .ok_or_else(|| anyhow!("Context not set"))?
            .clone();

        let proposal = match self
            .run_reviewee_proposal_with_timeout(&context, review, iteration)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                self.session.update_state(RallyState::Error);
                let _ = write_session(&self.session);
                self.send_event(RallyEvent::Error(format!(
                    "Reviewee proposal failed: {:#}",
                    e
                )))
                .await;
                self.send_event(RallyEvent::StateChanged(RallyState::Error))
                    .await;
                return Err(e);
            }
        };

        if let Err(e) = write_history_entry(
            &self.repo,
            self.pr_number,
            iteration,
            &HistoryEntryType::Proposal(proposal.clone()),
        ) {
            warn!("Failed to write proposal history: {}", e);
            self.send_event(RallyEvent::Log(format!(
                "Warning: Failed to write proposal history: {}",
                e
            )))
            .await;
        }

        self.send_event(RallyEvent::ProposalCompleted(proposal.clone()))
            .await;

        // Branch on proposal status. Error short-circuits the rally; Proposed
        // proceeds to the next reviewer cycle.
        match proposal.status {
            crate::ai::adapter::RevieweeProposalStatus::Proposed => {
                self.last_proposal = Some(proposal.clone());
                // Apply post strategy.
                match self.config.post_reviewee_proposals {
                    ProposalPostStrategy::Each => {
                        if let Err(e) = self.maybe_post_proposal_comment(&proposal).await {
                            if self.session.state == RallyState::Aborted {
                                return Ok(ReviewOnlyOutcome::Terminate(RallyResult::Aborted {
                                    iteration,
                                    reason: e.to_string(),
                                }));
                            }
                            warn!("Failed to post proposal comment: {}", e);
                        }
                    }
                    ProposalPostStrategy::Final => {
                        // Buffer for end-of-rally flush. Overwrites any prior
                        // unposted proposal because only the most recent plan
                        // is relevant when the rally terminates.
                        self.last_unposted_proposal = Some(proposal);
                    }
                    ProposalPostStrategy::None => {
                        // Drop the proposal (kept in history only).
                    }
                }
            }
            crate::ai::adapter::RevieweeProposalStatus::Error => {
                self.session.update_state(RallyState::Error);
                let _ = write_session(&self.session);
                let error = proposal
                    .error_details
                    .clone()
                    .unwrap_or_else(|| "Reviewee proposal returned error status".to_string());
                self.send_event(RallyEvent::Error(error.clone())).await;
                self.send_event(RallyEvent::StateChanged(RallyState::Error))
                    .await;
                return Ok(ReviewOnlyOutcome::Terminate(RallyResult::Error {
                    iteration,
                    error,
                }));
            }
        }

        // Checkpoint: pause before next reviewer cycle if requested.
        if let Some(result) = self.check_pause_at_checkpoint(iteration).await {
            return Ok(ReviewOnlyOutcome::Terminate(result));
        }

        Ok(ReviewOnlyOutcome::Continue)
    }

    /// Flush a buffered proposal (Final strategy) at rally termination.
    /// Best-effort: failures are logged but do not change the terminal result.
    async fn flush_final_proposal_if_buffered(&mut self) {
        if !matches!(
            self.config.post_reviewee_proposals,
            ProposalPostStrategy::Final
        ) {
            return;
        }
        let Some(proposal) = self.last_unposted_proposal.take() else {
            return;
        };
        if let Err(e) = self.maybe_post_proposal_comment(&proposal).await {
            warn!("Failed to flush final proposal at termination: {}", e);
            self.send_event(RallyEvent::Log(format!(
                "Warning: failed to post final proposal: {}",
                e
            )))
            .await;
        }
    }

    async fn run_reviewee_with_timeout(
        &mut self,
        context: &Context,
        review: &ReviewerOutput,
        iteration: u32,
    ) -> Result<RevieweeOutput> {
        let prompt = self
            .prompt_loader
            .load_reviewee_prompt(context, review, iteration);
        let duration = Duration::from_secs(self.config.timeout_secs);

        timeout(
            duration,
            self.reviewee_adapter.run_reviewee(&prompt, context),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "Reviewee timeout after {} seconds",
                self.config.timeout_secs
            )
        })?
    }

    async fn send_event(&self, event: RallyEvent) {
        let _ = self.event_sender.send(event).await;
    }

    /// Wrapper that optionally asks for user confirmation before posting review.
    /// - local_mode: skip posting entirely
    /// - auto_post: post directly without confirmation
    /// - otherwise: send confirmation event and wait for user response
    async fn maybe_post_review_to_pr(&mut self, review: &ReviewerOutput) -> Result<()> {
        // local_mode is handled inside post_review_to_pr
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            return self.post_review_to_pr(review).await;
        }

        if self.config.auto_post {
            return self.post_review_to_pr(review).await;
        }

        // Send confirmation event with lightweight DTO
        let info = ReviewPostInfo {
            action: format!("{:?}", review.action),
            summary: review.summary.clone(),
            comment_count: review.comments.len(),
        };

        self.session
            .update_state(RallyState::WaitingForPostConfirmation);
        let _ = write_session(&self.session);
        self.send_event(RallyEvent::ReviewPostConfirmNeeded(info))
            .await;
        self.send_event(RallyEvent::StateChanged(
            RallyState::WaitingForPostConfirmation,
        ))
        .await;

        // Wait for user response (loop to ignore invalid commands)
        loop {
            match self.wait_for_command().await {
                Some(OrchestratorCommand::PostConfirmResponse(true)) => {
                    self.send_event(RallyEvent::Log("User approved review posting".to_string()))
                        .await;
                    return self.post_review_to_pr(review).await;
                }
                Some(OrchestratorCommand::PostConfirmResponse(false)) => {
                    self.send_event(RallyEvent::Log("User skipped review posting".to_string()))
                        .await;
                    return Ok(());
                }
                Some(OrchestratorCommand::Abort) | None => {
                    self.session.update_state(RallyState::Aborted);
                    let _ = write_session(&self.session);
                    self.send_event(RallyEvent::StateChanged(RallyState::Aborted))
                        .await;
                    return Err(anyhow!("Review posting aborted by user"));
                }
                _ => {
                    // Invalid command for this state - warn and re-wait
                    warn!("Received invalid command during WaitingForPostConfirmation, ignoring");
                    continue;
                }
            }
        }
    }

    /// Wrapper that optionally asks for user confirmation before posting fix comment.
    async fn maybe_post_fix_comment(&mut self, fix: &RevieweeOutput) -> Result<()> {
        // local_mode is handled inside post_fix_comment
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            return self.post_fix_comment(fix).await;
        }

        if self.config.auto_post {
            return self.post_fix_comment(fix).await;
        }

        // Send confirmation event with lightweight DTO
        let info = FixPostInfo {
            summary: fix.summary.clone(),
            files_modified: fix.files_modified.clone(),
        };

        self.session
            .update_state(RallyState::WaitingForPostConfirmation);
        let _ = write_session(&self.session);
        self.send_event(RallyEvent::FixPostConfirmNeeded(info))
            .await;
        self.send_event(RallyEvent::StateChanged(
            RallyState::WaitingForPostConfirmation,
        ))
        .await;

        // Wait for user response (loop to ignore invalid commands)
        loop {
            match self.wait_for_command().await {
                Some(OrchestratorCommand::PostConfirmResponse(true)) => {
                    self.send_event(RallyEvent::Log(
                        "User approved fix comment posting".to_string(),
                    ))
                    .await;
                    return self.post_fix_comment(fix).await;
                }
                Some(OrchestratorCommand::PostConfirmResponse(false)) => {
                    self.send_event(RallyEvent::Log(
                        "User skipped fix comment posting".to_string(),
                    ))
                    .await;
                    return Ok(());
                }
                Some(OrchestratorCommand::Abort) | None => {
                    self.session.update_state(RallyState::Aborted);
                    let _ = write_session(&self.session);
                    self.send_event(RallyEvent::StateChanged(RallyState::Aborted))
                        .await;
                    return Err(anyhow!("Fix comment posting aborted by user"));
                }
                _ => {
                    // Invalid command for this state - warn and re-wait
                    warn!("Received invalid command during WaitingForPostConfirmation, ignoring");
                    continue;
                }
            }
        }
    }

    /// Post review to PR (summary comment + inline comments)
    async fn post_review_to_pr(&self, review: &ReviewerOutput) -> Result<()> {
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            self.send_event(RallyEvent::Log(
                "Local mode: skipping review posting to PR".to_string(),
            ))
            .await;
            return Ok(());
        }

        let context = self
            .context
            .as_ref()
            .ok_or_else(|| anyhow!("Context not set"))?;

        // Map AI ReviewAction to App ReviewAction
        let app_action = match review.action {
            ReviewAction::Approve => crate::app::ReviewAction::Approve,
            ReviewAction::RequestChanges => crate::app::ReviewAction::RequestChanges,
            ReviewAction::Comment => crate::app::ReviewAction::Comment,
        };

        // Copy for potential fallback use (app_action is moved into submit_review)
        let app_action_for_fallback = app_action;

        // Add prefix to summary
        let summary_with_prefix = format!("[AI Rally - Reviewer]\n\n{}", review.summary);

        // Post summary comment using gh pr review
        // If approve fails (e.g., can't approve own PR), fall back to comment
        let result =
            github::submit_review(&self.repo, self.pr_number, app_action, &summary_with_prefix)
                .await;

        if result.is_err() && matches!(app_action_for_fallback, crate::app::ReviewAction::Approve) {
            warn!("Approve failed, falling back to comment");
            github::submit_review(
                &self.repo,
                self.pr_number,
                crate::app::ReviewAction::Comment,
                &summary_with_prefix,
            )
            .await?;
        } else {
            result?;
        }

        // Post inline comments with rate limit handling
        for comment in &review.comments {
            // Convert line number to patch position
            let patch = context
                .file_patches
                .iter()
                .find(|(name, _)| name == &comment.path)
                .map(|(_, p)| p.as_str());

            let Some(patch) = patch else {
                warn!("No patch found for {}, skipping comment", comment.path);
                continue;
            };

            let Some(position) = crate::diff::line_number_to_position(patch, comment.line) else {
                warn!(
                    "Could not convert line {} to position for {}, skipping comment",
                    comment.line, comment.path
                );
                continue;
            };

            // Add prefix to inline comment
            let body_with_prefix = format!("[AI Rally - Reviewer]\n\n{}", comment.body);
            if let Err(e) = github::create_review_comment(
                &self.repo,
                self.pr_number,
                &context.head_sha,
                &comment.path,
                position,
                &body_with_prefix,
            )
            .await
            {
                warn!(
                    "Failed to post inline comment on {}:{} (position {}): {}",
                    comment.path, comment.line, position, e
                );
            }
            // Rate limit mitigation: small delay between API calls
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Ok(())
    }

    /// Wrapper that optionally asks for user confirmation before posting
    /// a reviewee proposal as a PR comment. Mirrors `maybe_post_fix_comment`.
    async fn maybe_post_proposal_comment(&mut self, proposal: &RevieweeProposal) -> Result<()> {
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            return self.post_proposal_comment(proposal).await;
        }

        if self.config.auto_post {
            return self.post_proposal_comment(proposal).await;
        }

        let info = ProposalPostInfo {
            summary: proposal.summary.clone(),
            target_files: proposal
                .target_files()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            plan_item_count: proposal.plan.len(),
        };

        self.session
            .update_state(RallyState::WaitingForPostConfirmation);
        let _ = write_session(&self.session);
        self.send_event(RallyEvent::ProposalPostConfirmNeeded(info))
            .await;
        self.send_event(RallyEvent::StateChanged(
            RallyState::WaitingForPostConfirmation,
        ))
        .await;

        loop {
            match self.wait_for_command().await {
                Some(OrchestratorCommand::PostConfirmResponse(true)) => {
                    self.send_event(RallyEvent::Log(
                        "User approved proposal comment posting".to_string(),
                    ))
                    .await;
                    return self.post_proposal_comment(proposal).await;
                }
                Some(OrchestratorCommand::PostConfirmResponse(false)) => {
                    self.send_event(RallyEvent::Log(
                        "User skipped proposal comment posting".to_string(),
                    ))
                    .await;
                    return Ok(());
                }
                Some(OrchestratorCommand::Abort) | None => {
                    self.session.update_state(RallyState::Aborted);
                    let _ = write_session(&self.session);
                    self.send_event(RallyEvent::StateChanged(RallyState::Aborted))
                        .await;
                    return Err(anyhow!("Proposal comment posting aborted by user"));
                }
                _ => {
                    warn!(
                        "Received invalid command during WaitingForPostConfirmation (proposal), ignoring"
                    );
                    continue;
                }
            }
        }
    }

    /// Post a reviewee proposal as a single PR comment.
    /// In local_mode this is a no-op (only a Log event is emitted).
    async fn post_proposal_comment(&self, proposal: &RevieweeProposal) -> Result<()> {
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            self.send_event(RallyEvent::Log(
                "Local mode: skipping proposal comment posting".to_string(),
            ))
            .await;
            return Ok(());
        }

        let plan_md = if proposal.plan.is_empty() {
            "(no plan items)".to_string()
        } else {
            proposal
                .plan
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let files = if item.target_files.is_empty() {
                        "(none)".to_string()
                    } else {
                        item.target_files
                            .iter()
                            .map(|f| format!("`{}`", f))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    format!(
                        "### Plan {}\n\n**Files:** {}\n\n**Description:** {}\n\n**Rationale:** {}",
                        i + 1,
                        files,
                        item.description,
                        item.rationale,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };

        let body = format!(
            "[AI Rally - Reviewee Proposal]\n\n{}\n\n## Plan\n\n{}\n\n## Overall Rationale\n\n{}",
            proposal.summary, plan_md, proposal.rationale,
        );

        github::submit_review(
            &self.repo,
            self.pr_number,
            crate::app::ReviewAction::Comment,
            &body,
        )
        .await?;

        self.send_event(RallyEvent::Log(format!(
            "Posted proposal comment to PR #{} ({} plan item(s))",
            self.pr_number,
            proposal.plan.len()
        )))
        .await;

        Ok(())
    }

    /// Post fix summary comment to PR
    async fn post_fix_comment(&self, fix: &RevieweeOutput) -> Result<()> {
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            self.send_event(RallyEvent::Log(
                "Local mode: skipping fix comment posting".to_string(),
            ))
            .await;
            return Ok(());
        }

        // Build comment body with files modified
        let files_list = if fix.files_modified.is_empty() {
            "No files modified".to_string()
        } else {
            fix.files_modified
                .iter()
                .map(|f| format!("- `{}`", f))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let comment_body = format!(
            "[AI Rally - Reviewee]\n\n{}\n\n**Files modified:**\n{}",
            fix.summary, files_list
        );

        // Post as a comment (not a review)
        github::submit_review(
            &self.repo,
            self.pr_number,
            crate::app::ReviewAction::Comment,
            &comment_body,
        )
        .await?;

        Ok(())
    }

    /// Fetch external comments from bots (Copilot, CodeRabbit, etc.)
    async fn fetch_external_comments(&self) -> Vec<ExternalComment> {
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            return Vec::new();
        }

        let mut comments = Vec::new();

        // Fetch review comments (inline comments on diff)
        if let Ok(review_comments) = fetch_review_comments(&self.repo, self.pr_number).await {
            for c in review_comments {
                if is_bot_user(&c.user.login) {
                    comments.push(ExternalComment {
                        source: c.user.login.clone(),
                        path: Some(c.path.clone()),
                        line: c.line,
                        body: c.body.clone(),
                    });
                }
            }
        }

        // Fetch discussion comments (general PR comments)
        if let Ok(discussion) = fetch_discussion_comments(&self.repo, self.pr_number).await {
            for c in discussion {
                if is_bot_user(&c.user.login) {
                    comments.push(ExternalComment {
                        source: c.user.login.clone(),
                        path: None,
                        line: None,
                        body: c.body.clone(),
                    });
                }
            }
        }

        // Limit the number of comments
        comments.truncate(MAX_EXTERNAL_COMMENTS);
        comments
    }

    /// Update head_sha from PR
    ///
    /// Note: The reviewee does NOT push changes; commits are local only.
    /// This update is for when the user manually pushes between iterations,
    /// or when external tools/CI update the PR branch.
    async fn update_head_sha(&mut self) -> Result<()> {
        if self.context.as_ref().is_some_and(|c| c.local_mode) {
            return Ok(());
        }

        let pr = github::fetch_pr(&self.repo, self.pr_number).await?;
        if let Some(ref mut ctx) = self.context {
            ctx.head_sha = pr.head.sha.clone();
        }
        Ok(())
    }

    /// Fetch current diff, preferring local git diff over GitHub API.
    ///
    /// This allows the reviewer to see uncommitted/unpushed changes made by the reviewee.
    /// Falls back to GitHub API if local git diff fails or returns empty.
    async fn fetch_current_diff(&self) -> Result<String> {
        // ローカルモードでは git fetch をスキップし、直接 diff を取得
        if let Some(ref ctx) = self.context {
            if ctx.local_mode {
                return self.fetch_local_working_diff(ctx).await;
            }
        }

        // Timeout for git operations (30 seconds)
        const GIT_TIMEOUT_SECS: u64 = 30;

        // Try local git diff first if we have working_dir and base_branch
        if let Some(ref ctx) = self.context {
            if let Some(ref working_dir) = ctx.working_dir {
                let base_branch = &ctx.base_branch;

                // Fetch latest base branch reference to ensure accurate diff
                // Use timeout to prevent hanging on slow remotes or credential prompts
                let fetch_future = tokio::process::Command::new("git")
                    .args(["fetch", "origin", base_branch])
                    .current_dir(working_dir)
                    .output();

                match timeout(Duration::from_secs(GIT_TIMEOUT_SECS), fetch_future).await {
                    Ok(Ok(output)) if output.status.success() => {
                        // Fetch succeeded
                    }
                    Ok(Ok(_)) => {
                        warn!("git fetch failed, continuing with potentially stale ref");
                    }
                    Ok(Err(e)) => {
                        warn!(
                            "git fetch command failed: {}, continuing with potentially stale ref",
                            e
                        );
                    }
                    Err(_) => {
                        warn!(
                            "git fetch timed out after {} seconds, continuing with potentially stale ref",
                            GIT_TIMEOUT_SECS
                        );
                    }
                }

                // Try git diff against origin/base_branch using merge-base (three-dot) comparison
                // This matches GitHub PR diff semantics and avoids including unrelated base-branch changes
                // Wrap in timeout to prevent hanging on network issues or auth prompts
                let git_diff_future = tokio::process::Command::new("git")
                    .args(["diff", &format!("origin/{}...HEAD", base_branch)])
                    .current_dir(working_dir)
                    .output();

                match timeout(Duration::from_secs(GIT_TIMEOUT_SECS), git_diff_future).await {
                    Ok(Ok(output)) if output.status.success() => {
                        let diff = String::from_utf8_lossy(&output.stdout).to_string();
                        if !diff.trim().is_empty() {
                            self.send_event(RallyEvent::Log(
                                "Using local git diff for re-review".to_string(),
                            ))
                            .await;
                            return Ok(diff);
                        }
                    }
                    Ok(Ok(_)) => {
                        // git diff failed, fall through to GitHub API
                    }
                    Ok(Err(e)) => {
                        warn!("git diff command failed: {}", e);
                    }
                    Err(_) => {
                        warn!(
                            "git diff timed out after {} seconds, falling back to GitHub API",
                            GIT_TIMEOUT_SECS
                        );
                    }
                }

                self.send_event(RallyEvent::Log(
                    "Local git diff empty or failed, falling back to GitHub API".to_string(),
                ))
                .await;
            }
        }

        // Fallback to GitHub API
        github::fetch_pr_diff(&self.repo, self.pr_number).await
    }

    /// ローカルモード専用の diff 取得
    ///
    /// `git diff HEAD` を最優先し、working tree + staged の最新変更を取得。
    /// 空の場合は `origin/{base}...HEAD` でコミット済み差分を試行。
    /// どちらも空の場合は空文字列を返す（stale な初期 diff にフォールバックしない）。
    async fn fetch_local_working_diff(&self, ctx: &super::adapter::Context) -> Result<String> {
        const GIT_TIMEOUT_SECS: u64 = 30;

        let working_dir = ctx.working_dir.as_deref().unwrap_or(".");
        let base_branch = &ctx.base_branch;

        // 1. git diff HEAD（working tree + staged の最新変更を優先）
        let git_diff_future = tokio::process::Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(working_dir)
            .output();

        match timeout(Duration::from_secs(GIT_TIMEOUT_SECS), git_diff_future).await {
            Ok(Ok(output)) if output.status.success() => {
                let diff = String::from_utf8_lossy(&output.stdout).to_string();
                if !diff.trim().is_empty() {
                    self.send_event(RallyEvent::Log(
                        "Using local git diff HEAD for re-review".to_string(),
                    ))
                    .await;
                    return Ok(diff);
                }
            }
            _ => {}
        }

        // 2. Fallback: origin/{base}...HEAD（コミット済み差分）
        let origin_ref = format!("origin/{}...HEAD", base_branch);
        let git_diff_future = tokio::process::Command::new("git")
            .args(["diff", &origin_ref])
            .current_dir(working_dir)
            .output();

        if let Ok(Ok(output)) =
            timeout(Duration::from_secs(GIT_TIMEOUT_SECS), git_diff_future).await
        {
            if output.status.success() {
                let diff = String::from_utf8_lossy(&output.stdout).to_string();
                if !diff.trim().is_empty() {
                    self.send_event(RallyEvent::Log(
                        "Using local git diff (origin base) for re-review".to_string(),
                    ))
                    .await;
                    return Ok(diff);
                }
            }
        }

        // 両方空の場合は空文字列を返す（stale な ctx.diff にフォールバックしない）
        self.send_event(RallyEvent::Log(
            "Local diff is empty (no changes detected)".to_string(),
        ))
        .await;
        Ok(String::new())
    }

    // For debugging and session inspection
    #[allow(dead_code)]
    pub fn session(&self) -> &RallySession {
        &self.session
    }
}

/// Check if a user is a bot
fn is_bot_user(login: &str) -> bool {
    BOT_SUFFIXES.iter().any(|suffix| login.ends_with(suffix)) || BOT_EXACT_MATCHES.contains(&login)
}

/// Extract the shell command from a `Bash(command:*)` tool pattern.
///
/// Returns `Some(command)` if the action matches the pattern, `None` otherwise.
fn extract_bash_command(action: &str) -> Option<&str> {
    let rest = action.trim().strip_prefix("Bash(")?;
    // Handle both Bash(cmd:*) and Bash(cmd) formats
    let inner = rest.strip_suffix(')')?;
    Some(inner.strip_suffix(":*").unwrap_or(inner))
}

/// Split a shell command string by command separators (`&&`, `||`, `;`, `|`).
///
/// Handles `||` before `|` to avoid incorrect splitting.
fn split_shell_commands(command: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let mut start = 0;
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Check for two-character separators (&& and ||) first
        let is_double = i + 1 < len
            && ((bytes[i] == b'&' && bytes[i + 1] == b'&')
                || (bytes[i] == b'|' && bytes[i + 1] == b'|'));
        if is_double {
            results.push(&command[start..i]);
            i += 2;
            start = i;
        } else if bytes[i] == b';' || bytes[i] == b'|' {
            results.push(&command[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }

    if start <= len {
        results.push(&command[start..]);
    }

    results
}

/// Validate whether a tool/action string contains blocked git operations.
///
/// Uses strict token-based parsing instead of substring matching to prevent
/// bypasses like `git status && git push` passing a `contains("git status")` check.
///
/// Returns `Some(reason)` if the action is blocked, `None` if allowed.
fn check_blocked_git_operation(action: &str) -> Option<String> {
    // For non-Bash tools (Read, Edit, Write, Glob, Grep, etc.), always allow
    let command = match extract_bash_command(action) {
        Some(cmd) => cmd,
        None => {
            // Not a Bash() pattern — check if it looks like a raw git command
            let trimmed = action.trim();
            if trimmed.starts_with("git ") || trimmed == "git" {
                trimmed
            } else {
                return None;
            }
        }
    };

    if command.is_empty() {
        return None;
    }

    // Split by shell command separators to detect chained commands
    let individual_commands = split_shell_commands(command);

    for cmd in &individual_commands {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            continue;
        }

        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        // Check if this is a git command
        if tokens[0] == "git" {
            if tokens.len() < 2 {
                return Some("Bare 'git' command without subcommand is not allowed".to_string());
            }

            let subcommand = tokens[1];

            // Reject flags before subcommand (e.g., git -C /path push)
            // as they can be used to obfuscate the actual operation
            if subcommand.starts_with('-') {
                return Some(format!(
                    "Git command with flags before subcommand is not allowed: '{}'",
                    trimmed
                ));
            }

            if !ALLOWED_GIT_SUBCOMMANDS.contains(&subcommand) {
                return Some(format!(
                    "Git subcommand '{}' is not in the allowed list ({:?})",
                    subcommand, ALLOWED_GIT_SUBCOMMANDS
                ));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn test_orchestrator_command_variants() {
        // Test ClarificationResponse
        let cmd = OrchestratorCommand::ClarificationResponse("test answer".to_string());
        match cmd {
            OrchestratorCommand::ClarificationResponse(answer) => {
                assert_eq!(answer, "test answer");
            }
            _ => panic!("Expected ClarificationResponse"),
        }

        // Test PermissionResponse approved
        let cmd = OrchestratorCommand::PermissionResponse(true);
        match cmd {
            OrchestratorCommand::PermissionResponse(approved) => {
                assert!(approved);
            }
            _ => panic!("Expected PermissionResponse"),
        }

        // Test PermissionResponse denied
        let cmd = OrchestratorCommand::PermissionResponse(false);
        match cmd {
            OrchestratorCommand::PermissionResponse(approved) => {
                assert!(!approved);
            }
            _ => panic!("Expected PermissionResponse"),
        }

        // Test SkipClarification
        let cmd = OrchestratorCommand::SkipClarification;
        assert!(matches!(cmd, OrchestratorCommand::SkipClarification));

        // Test PostConfirmResponse approved
        let cmd = OrchestratorCommand::PostConfirmResponse(true);
        match cmd {
            OrchestratorCommand::PostConfirmResponse(approved) => {
                assert!(approved);
            }
            _ => panic!("Expected PostConfirmResponse"),
        }

        // Test PostConfirmResponse skipped
        let cmd = OrchestratorCommand::PostConfirmResponse(false);
        match cmd {
            OrchestratorCommand::PostConfirmResponse(approved) => {
                assert!(!approved);
            }
            _ => panic!("Expected PostConfirmResponse"),
        }

        // Test Abort
        let cmd = OrchestratorCommand::Abort;
        assert!(matches!(cmd, OrchestratorCommand::Abort));
    }

    #[tokio::test]
    async fn test_command_channel_clarification() {
        let (tx, mut rx) = mpsc::channel::<OrchestratorCommand>(1);

        // Send clarification response
        tx.send(OrchestratorCommand::ClarificationResponse(
            "user's answer".to_string(),
        ))
        .await
        .unwrap();

        // Receive and verify
        let cmd = rx.recv().await.unwrap();
        match cmd {
            OrchestratorCommand::ClarificationResponse(answer) => {
                assert_eq!(answer, "user's answer");
            }
            _ => panic!("Expected ClarificationResponse"),
        }
    }

    #[tokio::test]
    async fn test_command_channel_permission_granted() {
        let (tx, mut rx) = mpsc::channel::<OrchestratorCommand>(1);

        tx.send(OrchestratorCommand::PermissionResponse(true))
            .await
            .unwrap();

        let cmd = rx.recv().await.unwrap();
        match cmd {
            OrchestratorCommand::PermissionResponse(approved) => {
                assert!(approved, "Permission should be granted");
            }
            _ => panic!("Expected PermissionResponse"),
        }
    }

    #[tokio::test]
    async fn test_command_channel_permission_denied() {
        let (tx, mut rx) = mpsc::channel::<OrchestratorCommand>(1);

        tx.send(OrchestratorCommand::PermissionResponse(false))
            .await
            .unwrap();

        let cmd = rx.recv().await.unwrap();
        match cmd {
            OrchestratorCommand::PermissionResponse(approved) => {
                assert!(!approved, "Permission should be denied");
            }
            _ => panic!("Expected PermissionResponse"),
        }
    }

    #[tokio::test]
    async fn test_command_channel_skip_clarification() {
        let (tx, mut rx) = mpsc::channel::<OrchestratorCommand>(1);

        tx.send(OrchestratorCommand::SkipClarification)
            .await
            .unwrap();

        let cmd = rx.recv().await.unwrap();
        assert!(matches!(cmd, OrchestratorCommand::SkipClarification));
    }

    #[tokio::test]
    async fn test_command_channel_abort() {
        let (tx, mut rx) = mpsc::channel::<OrchestratorCommand>(1);

        tx.send(OrchestratorCommand::Abort).await.unwrap();

        let cmd = rx.recv().await.unwrap();
        assert!(matches!(cmd, OrchestratorCommand::Abort));
    }

    #[tokio::test]
    async fn test_command_channel_closed_returns_none() {
        let (tx, mut rx) = mpsc::channel::<OrchestratorCommand>(1);

        // Drop sender to close channel
        drop(tx);

        // Receive should return None
        let cmd = rx.recv().await;
        assert!(cmd.is_none());
    }

    #[test]
    fn test_is_bot_user() {
        // Bot suffixes
        assert!(is_bot_user("copilot[bot]"));
        assert!(is_bot_user("coderabbitai[bot]"));
        assert!(is_bot_user("renovate[bot]"));

        // Exact matches
        assert!(is_bot_user("github-actions"));
        assert!(is_bot_user("dependabot"));

        // Non-bot users
        assert!(!is_bot_user("ushironoko"));
        assert!(!is_bot_user("octocat"));
        assert!(!is_bot_user("bot")); // "bot" alone is not a bot suffix
    }

    #[tokio::test]
    async fn test_command_channel_post_confirm_approved() {
        let (tx, mut rx) = mpsc::channel::<OrchestratorCommand>(1);

        tx.send(OrchestratorCommand::PostConfirmResponse(true))
            .await
            .unwrap();

        let cmd = rx.recv().await.unwrap();
        match cmd {
            OrchestratorCommand::PostConfirmResponse(approved) => {
                assert!(approved, "Post should be approved");
            }
            _ => panic!("Expected PostConfirmResponse"),
        }
    }

    #[tokio::test]
    async fn test_command_channel_post_confirm_skipped() {
        let (tx, mut rx) = mpsc::channel::<OrchestratorCommand>(1);

        tx.send(OrchestratorCommand::PostConfirmResponse(false))
            .await
            .unwrap();

        let cmd = rx.recv().await.unwrap();
        match cmd {
            OrchestratorCommand::PostConfirmResponse(approved) => {
                assert!(!approved, "Post should be skipped");
            }
            _ => panic!("Expected PostConfirmResponse"),
        }
    }

    #[test]
    fn test_rally_state_is_active() {
        assert!(RallyState::Initializing.is_active());
        assert!(RallyState::ReviewerReviewing.is_active());
        assert!(RallyState::RevieweeFix.is_active());
        assert!(RallyState::WaitingForClarification.is_active());
        assert!(RallyState::WaitingForPermission.is_active());
        assert!(RallyState::WaitingForPostConfirmation.is_active());
        assert!(!RallyState::Completed.is_active());
        assert!(!RallyState::Aborted.is_active());
        assert!(!RallyState::Error.is_active());
    }

    #[test]
    fn test_rally_state_is_finished() {
        assert!(!RallyState::Initializing.is_finished());
        assert!(!RallyState::ReviewerReviewing.is_finished());
        assert!(!RallyState::RevieweeFix.is_finished());
        assert!(!RallyState::WaitingForClarification.is_finished());
        assert!(!RallyState::WaitingForPermission.is_finished());
        assert!(!RallyState::WaitingForPostConfirmation.is_finished());
        assert!(RallyState::Completed.is_finished());
        assert!(RallyState::Aborted.is_finished());
        assert!(RallyState::Error.is_finished());
    }

    #[test]
    fn test_review_post_info() {
        let info = ReviewPostInfo {
            action: "Approve".to_string(),
            summary: "Looks good".to_string(),
            comment_count: 3,
        };
        assert_eq!(info.action, "Approve");
        assert_eq!(info.summary, "Looks good");
        assert_eq!(info.comment_count, 3);
    }

    #[test]
    fn test_fix_post_info() {
        let info = FixPostInfo {
            summary: "Fixed issues".to_string(),
            files_modified: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
        };
        assert_eq!(info.summary, "Fixed issues");
        assert_eq!(info.files_modified.len(), 2);
    }

    /// Mock adapter for orchestrator integration tests. Counts invocations
    /// so tests can assert which phases ran.
    ///
    /// `reviewer_actions` is consumed by index: the Nth `run_reviewer` call
    /// returns `reviewer_actions[N]` (0-indexed). Exhausting it is a test bug,
    /// so the mock panics with the call index for diagnosis.
    ///
    /// `proposal_statuses` is consumed similarly. If exhausted, it falls back
    /// to `Proposed` so tests that don't care about the status don't have to
    /// pre-populate it.
    struct MockAdapter {
        name: &'static str,
        reviewer_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        reviewee_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        proposal_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        reviewer_actions: Vec<ReviewAction>,
        proposal_statuses: Vec<crate::ai::adapter::RevieweeProposalStatus>,
        /// When the Nth `run_reviewee_proposal` call is true, the adapter
        /// returns `Err(...)` instead of an Ok proposal. Used to test the
        /// adapter-failure path independently from `RevieweeProposalStatus::Error`.
        proposal_adapter_errors: Vec<bool>,
    }

    #[async_trait::async_trait]
    impl AgentAdapter for MockAdapter {
        fn name(&self) -> &str {
            self.name
        }
        fn set_event_sender(&mut self, _sender: mpsc::Sender<RallyEvent>) {}
        async fn run_reviewer(
            &mut self,
            _prompt: &str,
            _context: &Context,
        ) -> Result<ReviewerOutput> {
            let n = self
                .reviewer_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let action = *self.reviewer_actions.get(n).unwrap_or_else(|| {
                panic!(
                    "MockAdapter: reviewer_actions exhausted at call #{} (len={})",
                    n,
                    self.reviewer_actions.len()
                )
            });
            Ok(ReviewerOutput {
                action,
                summary: "mock review summary".to_string(),
                comments: vec![],
                blocking_issues: vec![],
            })
        }
        async fn run_reviewee(
            &mut self,
            _prompt: &str,
            _context: &Context,
        ) -> Result<RevieweeOutput> {
            self.reviewee_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(RevieweeOutput {
                status: RevieweeStatus::Completed,
                summary: "mock fix summary".to_string(),
                files_modified: vec![],
                question: None,
                permission_request: None,
                error_details: None,
            })
        }
        async fn run_reviewee_proposal(
            &mut self,
            _prompt: &str,
            _context: &Context,
        ) -> Result<crate::ai::adapter::RevieweeProposal> {
            let n = self
                .proposal_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .proposal_adapter_errors
                .get(n)
                .copied()
                .unwrap_or(false)
            {
                return Err(anyhow!("simulated proposal adapter failure at call #{}", n));
            }
            let status = self
                .proposal_statuses
                .get(n)
                .copied()
                .unwrap_or(crate::ai::adapter::RevieweeProposalStatus::Proposed);
            let error_details = matches!(status, crate::ai::adapter::RevieweeProposalStatus::Error)
                .then(|| "mock error".to_string());
            Ok(crate::ai::adapter::RevieweeProposal {
                status,
                summary: "mock proposal summary".to_string(),
                plan: vec![],
                rationale: "mock rationale".to_string(),
                open_questions: None,
                error_details,
            })
        }
        async fn continue_reviewer(&mut self, _msg: &str) -> Result<ReviewerOutput> {
            unreachable!("continue_reviewer is not exercised in these tests")
        }
        async fn continue_reviewee(&mut self, _msg: &str) -> Result<RevieweeOutput> {
            unreachable!("continue_reviewee is not exercised in these tests")
        }
        fn add_reviewee_allowed_tool(&mut self, _tool: &str) {}
        fn set_local_mode(&mut self, _local_mode: bool) {}
    }

    /// Counters returned by `make_orchestrator_with_mocks`.
    struct MockCounters {
        reviewer_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        reviewee_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        proposal_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    /// Build orchestrator with mock adapters configurable per test.
    /// `reviewer_actions` cycles through each `run_reviewer` invocation.
    /// `proposal_statuses` does the same for `run_reviewee_proposal`; if empty
    /// or exhausted, defaults to `Proposed`.
    fn make_orchestrator_with_mocks_full(
        review_only: bool,
        reviewer_actions: Vec<ReviewAction>,
        proposal_statuses: Vec<crate::ai::adapter::RevieweeProposalStatus>,
        max_iterations: u32,
    ) -> (Orchestrator, MockCounters, mpsc::Receiver<RallyEvent>) {
        make_orchestrator_with_mocks_extended(
            review_only,
            reviewer_actions,
            proposal_statuses,
            vec![],
            max_iterations,
        )
    }

    fn make_orchestrator_with_mocks_extended(
        review_only: bool,
        reviewer_actions: Vec<ReviewAction>,
        proposal_statuses: Vec<crate::ai::adapter::RevieweeProposalStatus>,
        proposal_adapter_errors: Vec<bool>,
        max_iterations: u32,
    ) -> (Orchestrator, MockCounters, mpsc::Receiver<RallyEvent>) {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let reviewer_calls = Arc::new(AtomicUsize::new(0));
        let reviewee_calls = Arc::new(AtomicUsize::new(0));
        let proposal_calls = Arc::new(AtomicUsize::new(0));

        let reviewer = MockAdapter {
            name: "mock-reviewer",
            reviewer_calls: reviewer_calls.clone(),
            reviewee_calls: reviewee_calls.clone(),
            proposal_calls: proposal_calls.clone(),
            reviewer_actions: reviewer_actions.clone(),
            proposal_statuses: proposal_statuses.clone(),
            proposal_adapter_errors: proposal_adapter_errors.clone(),
        };
        let reviewee = MockAdapter {
            name: "mock-reviewee",
            reviewer_calls: reviewer_calls.clone(),
            reviewee_calls: reviewee_calls.clone(),
            proposal_calls: proposal_calls.clone(),
            reviewer_actions,
            proposal_statuses,
            proposal_adapter_errors,
        };

        let (event_tx, event_rx) = mpsc::channel(256);
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);

        let config = AiConfig {
            review_only,
            max_iterations,
            ..Default::default()
        };

        let prompt_loader = PromptLoader::new(&config, std::path::Path::new("."));
        let session = RallySession::new("owner/repo", 1);

        let orchestrator = Orchestrator {
            repo: "owner/repo".to_string(),
            pr_number: 1,
            config,
            reviewer_adapter: Box::new(reviewer),
            reviewee_adapter: Box::new(reviewee),
            session,
            context: None,
            seed_review: None,
            last_review: None,
            last_fix: None,
            last_proposal: None,
            last_unposted_proposal: None,
            event_sender: event_tx,
            prompt_loader,
            command_receiver: Some(cmd_rx),
            paused: false,
        };

        (
            orchestrator,
            MockCounters {
                reviewer_calls,
                reviewee_calls,
                proposal_calls,
            },
            event_rx,
        )
    }

    /// Convenience wrapper preserving the original signature for tests that
    /// don't need fine-grained control. Defaults `max_iterations=3` and a
    /// single reviewer action (which is reused across all reviewer calls by
    /// padding the Vec).
    fn make_orchestrator_with_mocks(
        review_only: bool,
        reviewer_action: ReviewAction,
    ) -> (
        Orchestrator,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        mpsc::Receiver<RallyEvent>,
    ) {
        // Repeat the single action enough times to cover max_iterations cycles,
        // so existing tests that loop until termination don't panic.
        let actions = vec![reviewer_action; 8];
        let (orch, counters, rx) =
            make_orchestrator_with_mocks_full(review_only, actions, vec![], 3);
        (orch, counters.reviewer_calls, counters.reviewee_calls, rx)
    }

    fn make_local_context() -> Context {
        Context {
            repo: "owner/repo".to_string(),
            pr_number: 1,
            pr_title: "test".to_string(),
            pr_body: None,
            diff: String::new(),
            working_dir: None,
            head_sha: "deadbeef".to_string(),
            base_branch: "main".to_string(),
            external_comments: vec![],
            local_mode: true,
            file_patches: vec![],
        }
    }

    #[tokio::test]
    async fn test_rally_started_event_emitted_first_with_review_only_flag() {
        // The orchestrator must emit RallyStarted as its very first event,
        // carrying the configured review_only flag so consumers can show a
        // startup banner. Verified for both review_only=true and false.
        for review_only in [true, false] {
            let (mut orchestrator, _reviewer_calls, _reviewee_calls, mut event_rx) =
                make_orchestrator_with_mocks(review_only, ReviewAction::Approve);
            orchestrator.set_context(make_local_context());

            let _ = orchestrator.run().await.unwrap();

            let first = event_rx.try_recv().expect("expected at least one event");
            match first {
                RallyEvent::RallyStarted { review_only: flag } => {
                    assert_eq!(
                        flag, review_only,
                        "RallyStarted must carry the configured review_only flag"
                    );
                }
                other => panic!("expected RallyStarted first, got {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn test_review_only_skips_reviewee_on_request_changes() {
        // Safety invariant: in review_only mode the reviewee fix phase MUST
        // NEVER run, regardless of how many proposal iterations happen.
        // Under the new proposal-iteration spec, reviewer is called up to
        // max_iterations times and proposal up to max_iterations-1 times;
        // run_reviewee (Edit/Write-capable) must stay at 0.
        let (mut orchestrator, counters, mut event_rx) = make_orchestrator_with_mocks_full(
            true,
            vec![ReviewAction::RequestChanges; 8],
            vec![],
            3,
        );
        orchestrator.set_context(make_local_context());

        let result = orchestrator.run().await.unwrap();

        assert_eq!(
            counters
                .reviewee_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "reviewee fix phase must NEVER be called in review_only mode"
        );
        assert_eq!(
            counters
                .reviewer_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            3,
            "reviewer runs max_iterations times"
        );
        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "proposal runs max_iterations-1 times (last iteration terminates without proposal)"
        );
        match result {
            RallyResult::ReviewOnlyCompleted {
                iteration, action, ..
            } => {
                assert_eq!(iteration, 3);
                assert_eq!(action, ReviewAction::RequestChanges);
            }
            other => panic!("expected ReviewOnlyCompleted, got {:?}", other),
        }

        // Confirm a ReviewOnlyCompleted event was emitted at termination.
        let mut saw_review_only_event = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, RallyEvent::ReviewOnlyCompleted(_)) {
                saw_review_only_event = true;
            }
        }
        assert!(
            saw_review_only_event,
            "ReviewOnlyCompleted event must be emitted at max_iterations"
        );
    }

    #[tokio::test]
    async fn test_review_only_runs_proposal_once_after_reviewer_request_changes() {
        // Slice 1 minimum-flow guarantee: when the reviewer requests changes
        // on the first iteration and max_iterations=2, the reviewee proposal
        // phase runs exactly once, then the second reviewer cycle hits
        // max_iterations and terminates.
        let (mut orchestrator, counters, _event_rx) = make_orchestrator_with_mocks_full(
            true,
            vec![ReviewAction::RequestChanges, ReviewAction::RequestChanges],
            vec![],
            2,
        );
        orchestrator.set_context(make_local_context());

        let _ = orchestrator.run().await.unwrap();

        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "proposal runs exactly once with max_iterations=2"
        );
        assert_eq!(
            counters
                .reviewee_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "reviewee fix phase must not run"
        );
        assert_eq!(
            counters
                .reviewer_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "reviewer runs twice (once per iteration)"
        );
    }

    #[tokio::test]
    async fn test_review_only_proposal_history_written() {
        // The reviewee proposal must be persisted to session history as
        // `HistoryEntryType::Proposal`.
        use crate::ai::session::{history_dir, read_history, HistoryEntryType};

        let pr_number = 8_017_001; // unlikely to collide with other tests
        let repo = "owner/proposal-history-test";

        // Clean leftover history from previous runs
        if let Ok(dir) = history_dir(repo, pr_number) {
            let _ = std::fs::remove_dir_all(dir);
        }

        let (mut orchestrator, _counters, _event_rx) = make_orchestrator_with_mocks_full(
            true,
            vec![ReviewAction::RequestChanges; 4],
            vec![],
            2,
        );
        // Override repo/pr_number on the orchestrator
        orchestrator.repo = repo.to_string();
        orchestrator.pr_number = pr_number;
        orchestrator.session = crate::ai::session::RallySession::new(repo, pr_number);
        orchestrator.set_context(make_local_context());

        let _ = orchestrator.run().await.unwrap();

        let history = read_history(repo, pr_number).expect("history must be readable");
        let proposal_entries: Vec<_> = history
            .iter()
            .filter(|e| matches!(e.entry_type, HistoryEntryType::Proposal(_)))
            .collect();
        assert_eq!(
            proposal_entries.len(),
            1,
            "exactly one Proposal entry must be written for max_iterations=2"
        );

        // Cleanup
        if let Ok(dir) = history_dir(repo, pr_number) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn test_review_only_with_approve_uses_normal_approved_path() {
        // When review_only is true and reviewer approves, the existing
        // Approve path handles it (emits Approved event, not ReviewOnlyCompleted),
        // and no proposal is generated.
        let (mut orchestrator, counters, mut event_rx) =
            make_orchestrator_with_mocks_full(true, vec![ReviewAction::Approve], vec![], 3);
        orchestrator.set_context(make_local_context());

        let _ = orchestrator.run().await.unwrap();

        assert_eq!(
            counters
                .reviewer_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            counters
                .reviewee_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "reviewee fix phase must NOT be called when reviewer approves"
        );
        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "proposal must NOT be called when reviewer approves"
        );

        let mut saw_approved = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, RallyEvent::Approved(_)) {
                saw_approved = true;
            }
        }
        assert!(saw_approved, "Approved event must be emitted on approve");
    }

    #[tokio::test]
    async fn test_review_only_loops_until_approve() {
        // reviewer returns RC, RC, Approve in sequence. With max_iterations=5,
        // the loop terminates at iteration 3 via the Approve path, having run
        // proposal exactly twice.
        let (mut orchestrator, counters, mut event_rx) = make_orchestrator_with_mocks_full(
            true,
            vec![
                ReviewAction::RequestChanges,
                ReviewAction::RequestChanges,
                ReviewAction::Approve,
            ],
            vec![],
            5,
        );
        orchestrator.set_context(make_local_context());

        let result = orchestrator.run().await.unwrap();

        assert_eq!(
            counters
                .reviewer_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            3,
            "reviewer runs three times: RC, RC, Approve"
        );
        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "proposal runs twice: after the two RC verdicts"
        );
        assert_eq!(
            counters
                .reviewee_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "reviewee fix phase never runs in review_only mode"
        );
        match result {
            RallyResult::Approved { iteration, .. } => assert_eq!(iteration, 3),
            other => panic!("expected Approved at iteration 3, got {:?}", other),
        }

        let mut saw_approved = false;
        let mut saw_proposal_completed_count = 0;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                RallyEvent::Approved(_) => saw_approved = true,
                RallyEvent::ProposalCompleted(_) => saw_proposal_completed_count += 1,
                _ => {}
            }
        }
        assert!(saw_approved, "Approved event must be emitted");
        assert_eq!(
            saw_proposal_completed_count, 2,
            "ProposalCompleted must be emitted twice"
        );
    }

    #[tokio::test]
    async fn test_review_only_max_iterations_one_terminates_without_proposal() {
        // max_iterations=1 with reviewer returning RC: the loop must terminate
        // on the first reviewer cycle without ever invoking proposal. This
        // preserves backward compatibility with the old single-shot review_only
        // behavior for users who deliberately set max_iterations=1.
        let (mut orchestrator, counters, _event_rx) =
            make_orchestrator_with_mocks_full(true, vec![ReviewAction::RequestChanges], vec![], 1);
        orchestrator.set_context(make_local_context());

        let result = orchestrator.run().await.unwrap();

        assert_eq!(
            counters
                .reviewer_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "proposal must NOT run when max_iterations=1"
        );
        match result {
            RallyResult::ReviewOnlyCompleted {
                iteration, action, ..
            } => {
                assert_eq!(iteration, 1);
                assert_eq!(action, ReviewAction::RequestChanges);
            }
            other => panic!("expected ReviewOnlyCompleted, got {:?}", other),
        }
    }

    /// Helper: build orchestrator with explicit post strategy.
    fn make_with_strategy(
        review_only: bool,
        reviewer_actions: Vec<ReviewAction>,
        max_iterations: u32,
        strategy: crate::config::ProposalPostStrategy,
    ) -> (Orchestrator, MockCounters, mpsc::Receiver<RallyEvent>) {
        let (mut orch, counters, rx) = make_orchestrator_with_mocks_full(
            review_only,
            reviewer_actions,
            vec![],
            max_iterations,
        );
        orch.config.post_reviewee_proposals = strategy;
        (orch, counters, rx)
    }

    /// Count occurrences of the local-mode "skipping proposal comment posting"
    /// log in the event stream. This is the test proxy for "post was attempted"
    /// because in local_mode `post_proposal_comment` short-circuits with this
    /// log line.
    fn count_proposal_post_attempts(rx: &mut mpsc::Receiver<RallyEvent>) -> usize {
        let mut n = 0;
        while let Ok(event) = rx.try_recv() {
            if let RallyEvent::Log(msg) = event {
                if msg.contains("Local mode: skipping proposal comment posting") {
                    n += 1;
                }
            }
        }
        n
    }

    #[tokio::test]
    async fn test_review_only_post_strategy_final_posts_only_last_proposal() {
        // Final strategy: proposals are buffered, and only the most recent one
        // is posted when the rally terminates via Approve. Two RC iterations
        // produce two proposals; only the latest is flushed at Approve.
        let (mut orch, _counters, mut rx) = make_with_strategy(
            true,
            vec![
                ReviewAction::RequestChanges,
                ReviewAction::RequestChanges,
                ReviewAction::Approve,
            ],
            5,
            crate::config::ProposalPostStrategy::Final,
        );
        orch.set_context(make_local_context());

        let _ = orch.run().await.unwrap();

        assert_eq!(
            count_proposal_post_attempts(&mut rx),
            1,
            "Final strategy must post exactly once (at Approve)"
        );
    }

    #[tokio::test]
    async fn test_review_only_post_strategy_final_on_max_iterations_posts_last() {
        // Final strategy at max_iterations: the buffered proposal is flushed
        // even when terminating via ReviewOnlyCompleted (no Approve).
        let (mut orch, _counters, mut rx) = make_with_strategy(
            true,
            vec![ReviewAction::RequestChanges; 4],
            3,
            crate::config::ProposalPostStrategy::Final,
        );
        orch.set_context(make_local_context());

        let _ = orch.run().await.unwrap();

        assert_eq!(
            count_proposal_post_attempts(&mut rx),
            1,
            "Final strategy must post the last buffered proposal at max_iterations"
        );
    }

    #[tokio::test]
    async fn test_review_only_post_strategy_each_posts_every_iteration() {
        // Each strategy: every proposal is posted immediately after it is
        // produced. With two RC iterations followed by Approve, two posts.
        let (mut orch, counters, mut rx) = make_with_strategy(
            true,
            vec![
                ReviewAction::RequestChanges,
                ReviewAction::RequestChanges,
                ReviewAction::Approve,
            ],
            5,
            crate::config::ProposalPostStrategy::Each,
        );
        orch.set_context(make_local_context());

        let _ = orch.run().await.unwrap();

        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            count_proposal_post_attempts(&mut rx),
            2,
            "Each strategy must post every proposal as it is produced"
        );
    }

    #[tokio::test]
    async fn test_review_only_post_strategy_none_skips_all() {
        // None strategy: proposals are kept in history only; no PR posts.
        let (mut orch, _counters, mut rx) = make_with_strategy(
            true,
            vec![
                ReviewAction::RequestChanges,
                ReviewAction::RequestChanges,
                ReviewAction::Approve,
            ],
            5,
            crate::config::ProposalPostStrategy::None,
        );
        orch.set_context(make_local_context());

        let _ = orch.run().await.unwrap();

        assert_eq!(
            count_proposal_post_attempts(&mut rx),
            0,
            "None strategy must never post proposals"
        );
    }

    #[tokio::test]
    async fn test_review_only_state_transitions_include_proposing() {
        // The orchestrator must transition through RevieweeProposing while
        // designing the proposal so the UI can render the correct status.
        let (mut orchestrator, _counters, mut event_rx) = make_orchestrator_with_mocks_full(
            true,
            vec![ReviewAction::RequestChanges, ReviewAction::RequestChanges],
            vec![],
            2,
        );
        orchestrator.set_context(make_local_context());

        let _ = orchestrator.run().await.unwrap();

        let mut saw_proposing = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(
                event,
                RallyEvent::StateChanged(RallyState::RevieweeProposing)
            ) {
                saw_proposing = true;
            }
        }
        assert!(
            saw_proposing,
            "StateChanged(RevieweeProposing) must appear in the event stream"
        );
    }

    #[tokio::test]
    async fn test_review_only_proposal_error_status_terminates_with_error() {
        // When the reviewee proposal returns status=error, the rally must
        // terminate immediately with RallyResult::Error and emit a
        // RallyEvent::Error so the UI/headless can surface it.
        let (mut orchestrator, counters, mut event_rx) = make_orchestrator_with_mocks_full(
            true,
            vec![ReviewAction::RequestChanges, ReviewAction::RequestChanges],
            vec![crate::ai::adapter::RevieweeProposalStatus::Error],
            5,
        );
        orchestrator.set_context(make_local_context());

        let result = orchestrator.run().await.unwrap();

        match result {
            RallyResult::Error { iteration, error } => {
                assert_eq!(iteration, 1);
                assert!(
                    !error.is_empty(),
                    "error message must be propagated from proposal.error_details"
                );
            }
            other => panic!("expected RallyResult::Error, got {:?}", other),
        }

        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one proposal call before termination"
        );
        assert_eq!(
            counters
                .reviewer_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the first reviewer cycle ran"
        );

        let mut saw_error = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, RallyEvent::Error(_)) {
                saw_error = true;
            }
        }
        assert!(
            saw_error,
            "RallyEvent::Error must be emitted when proposal status is Error"
        );
    }

    #[tokio::test]
    async fn test_review_only_proposal_adapter_failure_returns_err() {
        // When the proposal adapter itself errors (timeout, transport, etc.),
        // run() propagates the Err so callers know it was a hard failure
        // rather than a model-reported one.
        let (mut orchestrator, counters, _event_rx) = make_orchestrator_with_mocks_extended(
            true,
            vec![ReviewAction::RequestChanges, ReviewAction::RequestChanges],
            vec![],
            vec![true],
            5,
        );
        orchestrator.set_context(make_local_context());

        let result = orchestrator.run().await;
        let err = result.expect_err("run() must return Err on adapter failure");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("simulated proposal adapter failure"),
            "error must propagate adapter failure message, got: {}",
            msg
        );

        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn test_normal_mode_invokes_reviewee_on_request_changes() {
        // Sanity check: without review_only, reviewee fix runs and proposal does not.
        let (mut orchestrator, counters, _event_rx) = make_orchestrator_with_mocks_full(
            false,
            vec![ReviewAction::RequestChanges; 8],
            vec![],
            3,
        );
        orchestrator.set_context(make_local_context());

        let _ = orchestrator.run().await.unwrap();

        assert!(
            counters
                .reviewer_calls
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 1
        );
        assert!(
            counters
                .reviewee_calls
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 1,
            "reviewee must run in normal mode when reviewer requests changes"
        );
        assert_eq!(
            counters
                .proposal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "proposal must NOT be called in normal mode"
        );
    }

    #[test]
    fn test_extract_bash_command() {
        // Standard Bash(cmd:*) format
        assert_eq!(extract_bash_command("Bash(git push:*)"), Some("git push"));
        assert_eq!(
            extract_bash_command("Bash(git status:*)"),
            Some("git status")
        );

        // Without wildcard suffix
        assert_eq!(extract_bash_command("Bash(git push)"), Some("git push"));

        // Not a Bash pattern
        assert_eq!(extract_bash_command("Read"), None);
        assert_eq!(extract_bash_command("Edit"), None);
        assert_eq!(extract_bash_command("git push"), None);

        // Complex commands
        assert_eq!(
            extract_bash_command("Bash(git status && git push:*)"),
            Some("git status && git push")
        );
    }

    #[test]
    fn test_split_shell_commands() {
        // Single command
        assert_eq!(split_shell_commands("git status"), vec!["git status"]);

        // && separator
        let result = split_shell_commands("git status && git push");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].trim(), "git status");
        assert_eq!(result[1].trim(), "git push");

        // || separator
        let result = split_shell_commands("git status || git push");
        assert_eq!(result.len(), 2);

        // ; separator
        let result = split_shell_commands("git status; git push");
        assert_eq!(result.len(), 2);

        // | pipe
        let result = split_shell_commands("echo test | git push");
        assert_eq!(result.len(), 2);

        // Multiple separators
        let result = split_shell_commands("git status && git diff; git push");
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_check_blocked_git_operation_allows_safe_commands() {
        // Allowed git subcommands
        assert!(check_blocked_git_operation("Bash(git status:*)").is_none());
        assert!(check_blocked_git_operation("Bash(git diff:*)").is_none());
        assert!(check_blocked_git_operation("Bash(git add:*)").is_none());
        assert!(check_blocked_git_operation("Bash(git commit:*)").is_none());
        assert!(check_blocked_git_operation("Bash(git log:*)").is_none());
        assert!(check_blocked_git_operation("Bash(git show:*)").is_none());
        assert!(check_blocked_git_operation("Bash(git branch:*)").is_none());
        assert!(check_blocked_git_operation("Bash(git switch:*)").is_none());
        assert!(check_blocked_git_operation("Bash(git stash:*)").is_none());

        // Non-Bash tools are always allowed
        assert!(check_blocked_git_operation("Read").is_none());
        assert!(check_blocked_git_operation("Edit").is_none());
        assert!(check_blocked_git_operation("Write").is_none());
        assert!(check_blocked_git_operation("Glob").is_none());
        assert!(check_blocked_git_operation("Grep").is_none());
        assert!(check_blocked_git_operation("Skill").is_none());

        // Non-git Bash commands are allowed
        assert!(check_blocked_git_operation("Bash(cargo test:*)").is_none());
        assert!(check_blocked_git_operation("Bash(npm run build:*)").is_none());
    }

    #[test]
    fn test_check_blocked_git_operation_blocks_write_operations() {
        // git push
        assert!(check_blocked_git_operation("Bash(git push:*)").is_some());
        assert!(check_blocked_git_operation("Bash(git push origin main:*)").is_some());

        // git reset
        assert!(check_blocked_git_operation("Bash(git reset --hard:*)").is_some());

        // git checkout (can discard changes)
        assert!(check_blocked_git_operation("Bash(git checkout:*)").is_some());

        // git restore (can discard changes)
        assert!(check_blocked_git_operation("Bash(git restore:*)").is_some());

        // git clean
        assert!(check_blocked_git_operation("Bash(git clean:*)").is_some());

        // git rebase
        assert!(check_blocked_git_operation("Bash(git rebase:*)").is_some());

        // git merge
        assert!(check_blocked_git_operation("Bash(git merge:*)").is_some());
    }

    #[test]
    fn test_check_blocked_git_operation_blocks_mixed_commands() {
        // Mixed read + write via &&
        assert!(
            check_blocked_git_operation("Bash(git status && git push:*)").is_some(),
            "Should block git push hidden after git status via &&"
        );

        // Mixed read + write via ;
        assert!(
            check_blocked_git_operation("Bash(git status; git push -f origin main:*)").is_some(),
            "Should block git push hidden after git status via ;"
        );

        // Mixed read + write via ||
        assert!(
            check_blocked_git_operation("Bash(git status || git reset --hard:*)").is_some(),
            "Should block git reset hidden after git status via ||"
        );

        // Mixed read + write via pipe
        assert!(
            check_blocked_git_operation("Bash(echo test | git push:*)").is_some(),
            "Should block git push via pipe"
        );

        // Multiple chained safe commands should be allowed
        assert!(
            check_blocked_git_operation("Bash(git status && git diff:*)").is_none(),
            "Should allow chained safe git commands"
        );
    }

    #[test]
    fn test_check_blocked_git_operation_blocks_flag_obfuscation() {
        // git -C /path push (flag before subcommand to obfuscate)
        assert!(
            check_blocked_git_operation("Bash(git -C /tmp push:*)").is_some(),
            "Should block git commands with flags before subcommand"
        );
    }

    #[test]
    fn test_check_blocked_git_operation_raw_commands() {
        // Raw git commands (not in Bash() format)
        assert!(check_blocked_git_operation("git push").is_some());
        assert!(check_blocked_git_operation("git status").is_none());
        assert!(check_blocked_git_operation("git reset --hard").is_some());
    }

    #[test]
    fn test_check_blocked_bare_git() {
        // Bare 'git' without subcommand
        assert!(check_blocked_git_operation("Bash(git:*)").is_some());
        assert!(check_blocked_git_operation("git").is_some());
    }

    #[test]
    fn test_check_blocked_non_git_with_git_substring() {
        // Words containing "git" that aren't git commands should not be blocked
        // (e.g., "widget", "digit", "legit")
        assert!(check_blocked_git_operation("Bash(widget build:*)").is_none());
        assert!(check_blocked_git_operation("Bash(cargo test --features digit:*)").is_none());
    }
}
