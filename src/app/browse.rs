//! Repository Browser — read the whole repository, not just the diff.
//!
//! Every other screen in octorus is anchored to a change: a PR's changed files,
//! a local diff, a commit. Browse is the missing half — it walks the tracked
//! working tree, opens any file read-only with the same tree-sitter
//! highlighting the diff view uses, and navigates by symbol rather than by
//! filename.
//!
//! Symbol navigation is backed by [`crate::symbols::SymbolIndex`], built once
//! per browse session on a background thread. Until it is ready every other
//! part of the screen stays usable — an index is an accelerator, never a
//! prerequisite.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::code_index::{CodeIndex, CodeIndexBuild};
use crate::diff_store::{DiffScrollState, ScrollMode};
use crate::filter::ListFilter;
use crate::github::{
    blame_file, BlameError, BlameFile, BlameRef, CommitPrLookupError, CommitPrResolution,
    CommitPullRequest,
};
use crate::module_graph::{
    DependencyGuarantee, DependencyResult, DependencyTarget, ModuleGraph, SourceUniverse,
};
use crate::symbols::{CancelSignal, Symbol, SymbolIndex, SymbolRef};
use crate::syntax::ParserPool;
use crate::ui::common::truncate_with_width;

use super::browse_discussion::{
    DiscussionIndex, DiscussionLookupLimit, DiscussionView, LineDiscussionDelivery,
    LineDiscussionFailure, LineDiscussionLoadError, LineDiscussionState, LineOrigin,
    MAX_DISCUSSION_COMMIT_LOOKUPS, MAX_DISCUSSION_PULL_REQUESTS,
};
use super::types::*;
use super::{App, AppState};

/// Upper bound on files listed in the browser.
///
/// A monorepo with a million tracked paths would spend more time building the
/// tree than the user will ever spend scrolling it.
pub const MAX_BROWSE_FILES: usize = 200_000;

/// Files larger than this are shown as a notice instead of being highlighted.
pub const MAX_VIEWABLE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Files above 100,000 source lines are not cached, bounding per-line render state.
pub const MAX_VIEWABLE_FILE_LINES: usize = 100_000;

/// Individual lines above 10,000 bytes are not cached, catching minified bundles.
pub const MAX_VIEWABLE_LINE_BYTES: usize = 10_000;

/// Admission limit for commit-diff caching and syntax highlighting.
///
/// This is not an output-read limit: `git` and `gh` stdout is read completely
/// before this check. Oversized fetched text is dropped instead of being
/// cached, highlighted, or retained by the browser state.
pub const MAX_VIEWABLE_COMMIT_DIFF_BYTES: usize = 32 * 1024 * 1024;

fn admit_commit_diff_for_cache(diff_text: String) -> Result<String, String> {
    if diff_text.len() > MAX_VIEWABLE_COMMIT_DIFF_BYTES {
        Err(format!(
            "commit diff is {}, over the {} browser cache/highlight limit",
            human_bytes(diff_text.len() as u64),
            human_bytes(MAX_VIEWABLE_COMMIT_DIFF_BYTES as u64),
        ))
    } else {
        Ok(diff_text)
    }
}

/// Maximum rows retained per direction in the module-graph pane.
///
/// Hearth 0.1.1 materializes the full sorted query result, but bounding the
/// octorus projection prevents unbounded labels and UI state for high fan-in.
pub const MAX_MODULE_GRAPH_RESULTS: usize = 200;
const MAX_MODULE_GRAPH_LABEL_WIDTH: usize = 240;
const MAX_MODULE_GRAPH_COMPONENT_CHARS: usize = 512;

/// Maximum rows shown in the symbol search overlay.
pub const MAX_SYMBOL_SEARCH_RESULTS: usize = 200;

pub(crate) const BLAME_FULL_WIDTH: usize = 32;
pub(crate) const BLAME_AUTHOR_WIDTH: usize = 23;
pub(crate) const BLAME_IDENTITY_WIDTH: usize = 12;

impl App {
    /// Open the Repository Browser, loading the tracked file list in the background.
    pub fn open_repo_browse(&mut self) {
        let repo_root = self.browse_repo_root();
        if let Some(state) = self.browse_state.as_mut() {
            if state.repo_root == repo_root {
                state.return_state = self.state;
                self.state = AppState::RepoBrowseTree;
                return;
            }
            state.cancel_token.cancel();
        }

        let return_state = self.state;
        let mut state = BrowseState::new(repo_root.clone(), return_state);

        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        let cancel_token = state.cancel_token.clone();
        tokio::task::spawn_blocking(move || {
            deliver_repository_files(&cancel_token, &tx, || list_repository_files(&repo_root));
        });

        self.browse_state = Some(state);
        self.state = AppState::RepoBrowseTree;
    }

    /// Working directory the browser walks — `--working-dir` when given, else the process cwd.
    fn browse_repo_root(&self) -> PathBuf {
        self.working_dir
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// Leave the browser, returning to whichever screen opened it.
    ///
    /// A finished symbol index is still discarded on close; caching it across
    /// browser sessions would require storing it on [`App`] itself.
    pub fn close_repo_browse(&mut self) {
        let return_state = match self.browse_state.take() {
            Some(state) => {
                state.cancel_token.cancel();
                state.return_state
            }
            None => AppState::FileList,
        };
        self.state = return_state;
    }

    /// Drain background browse channels: file list, symbol index, highlighting.
    pub(crate) fn poll_browse_updates(&mut self) {
        let tab_width = self.config.diff.tab_width;
        let browse_file_active = matches!(
            self.state,
            AppState::RepoBrowseFile | AppState::RepoBrowseGraph
        );
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };

        let mut paths_ready = false;
        let mut file_ready = false;
        let mut focus_listing_failure = false;
        let mut completed_pr_lookup = None;
        let mut completed_line_discussion = None;

        let module_graph_refresh_path = state
            .module_graph_pane
            .path()
            .and_then(|pane_path| {
                state
                    .open
                    .as_ref()
                    .map(|open| open.path.as_str())
                    .filter(|open_path| *open_path != pane_path)
            })
            .map(str::to_string);
        if let Some(path) = module_graph_refresh_path {
            if state.open_is_pending() {
                state.cancel_module_graph_query();
                state.module_graph_pane = ModuleGraphPaneState::Waiting { path };
            } else if let Err(message) = state.start_module_graph_query_for_path(path) {
                state.close_module_graph_pane();
                state.status = Some(message.to_string());
                if self.state == AppState::RepoBrowseGraph {
                    self.state = AppState::RepoBrowseFile;
                }
            }
        }

        if let PrLookupState::Loading { .. } = &state.pr_lookup {
            if !browse_file_active || !state.pr_lookup_matches_current_context() {
                state.cancel_pr_lookup_request();
                state.pr_lookup = PrLookupState::Idle;
                state.status = Some(
                    "Pull request lookup abandoned because the context moved off that commit"
                        .to_string(),
                );
            }
        }
        if matches!(
            state.line_discussion,
            LineDiscussionState::ResolvingPullRequests { .. }
                | LineDiscussionState::LoadingComments { .. }
        ) && (!browse_file_active || !state.line_discussion_request_matches_context())
        {
            state.cancel_line_discussion_request();
            state.line_discussion = LineDiscussionState::Idle;
            state.status = Some(
                "Review discussion lookup abandoned because the open file changed".to_string(),
            );
        }
        if let Some(rx) = state.paths_receiver.as_mut() {
            match rx.try_recv() {
                Ok(Ok(listing)) => {
                    state.paths_receiver = None;
                    state.listing_status = repository_listing_status(&listing);
                    state.status = state.listing_status.clone();
                    state.source_universe = listing.source_universe();
                    state.set_paths(listing.paths);
                    paths_ready = true;
                }
                Ok(Err(message)) => {
                    state.paths_receiver = None;
                    install_repository_listing_failure(state, message, tab_width);
                    focus_listing_failure = true;
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.paths_receiver = None;
                    install_repository_listing_failure(
                        state,
                        "file listing task ended".to_string(),
                        tab_width,
                    );
                    focus_listing_failure = true;
                }
            }
        }

        if let Some(rx) = state.index_receiver.as_mut() {
            match rx.try_recv() {
                Ok(IndexDelivery::Ready(code)) => {
                    state.index_receiver = None;
                    let CodeIndex { symbols, modules } = *code;
                    state.index = IndexState::Ready(Arc::new(symbols));
                    state.module_graph = ModuleGraphState::Ready(Arc::new(modules));
                    state.refresh_open_file_symbols();
                }
                Ok(IndexDelivery::Failed(message)) => {
                    state.index_receiver = None;
                    state.index = IndexState::Failed {
                        reason: message.clone(),
                    };
                    state.module_graph = ModuleGraphState::Failed;
                    state.status = Some(message);
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.index_receiver = None;
                    if !state.cancel_token.is_cancelled() {
                        let message = "code indexing task ended".to_string();
                        state.index = IndexState::Failed {
                            reason: message.clone(),
                        };
                        state.module_graph = ModuleGraphState::Failed;
                        state.status = Some(message);
                    }
                }
            }
        }

        if let Some(rx) = state.module_graph_query_receiver.as_mut() {
            match rx.try_recv() {
                Ok(delivery) => {
                    state.module_graph_query_receiver = None;
                    state.module_graph_query_cancel = None;
                    let current = matches!(
                        &state.module_graph_pane,
                        ModuleGraphPaneState::Loading { request_id, path }
                            if *request_id == delivery.request_id && path == &delivery.path
                    ) && state
                        .open
                        .as_ref()
                        .is_some_and(|open| open.path == delivery.path);
                    if current {
                        state.module_graph_pane = ModuleGraphPaneState::Ready(delivery.panel);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.module_graph_query_receiver = None;
                    let cancelled = state
                        .module_graph_query_cancel
                        .take()
                        .is_some_and(|cancel| cancel.is_cancelled());
                    if !cancelled
                        && matches!(
                            state.module_graph_pane,
                            ModuleGraphPaneState::Loading { .. }
                        )
                    {
                        state.module_graph_pane = ModuleGraphPaneState::Closed;
                        state.status = Some("Dependency query task ended".to_string());
                        if self.state == AppState::RepoBrowseGraph {
                            self.state = AppState::RepoBrowseFile;
                        }
                    }
                }
            }
        }

        if let Some(rx) = state.file_receiver.as_mut() {
            match rx.try_recv() {
                Ok(delivery) => {
                    // `open_load` is the authoritative lifecycle state, so it
                    // alone decides whether this delivery is the one being
                    // awaited. Replacing the receiver on every new request
                    // already prevents stale deliveries; this guard is
                    // defence-in-depth against future lifecycle changes, and it
                    // must never strand the request that IS in flight — hence
                    // the receiver survives a delivery we did not ask for.
                    let target = match state.open_load {
                        OpenLoad::Pending {
                            ref path,
                            line,
                            scroll,
                            ..
                        } if path == &delivery.path => Some((line, scroll)),
                        OpenLoad::Idle | OpenLoad::Pending { .. } | OpenLoad::Failed { .. } => None,
                    };
                    if let Some((line, scroll)) = target {
                        state.file_receiver = None;
                        match delivery.result {
                            Ok(open) => {
                                state.open_load = OpenLoad::Idle;
                                install_open_file(state, open, line, scroll);
                                file_ready = true;
                            }
                            Err(message) => {
                                install_file_load_failure(state, delivery.path, message, tab_width);
                            }
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.file_receiver = None;
                    if let OpenLoad::Pending { ref path, .. } = state.open_load {
                        let path = path.clone();
                        let message = format!("{path}: file loading task ended");
                        install_file_load_failure(state, path, message, tab_width);
                    }
                }
            }
        }

        let waiting_graph_file_failed = matches!(
            (&state.module_graph_pane, &state.open_load),
            (
                ModuleGraphPaneState::Waiting { path: waiting },
                OpenLoad::Failed { path: failed, .. }
            ) if waiting == failed
        );
        if waiting_graph_file_failed {
            state.close_module_graph_pane();
            if self.state == AppState::RepoBrowseGraph {
                self.state = AppState::RepoBrowseFile;
            }
        }

        if let Some(rx) = state.blame_receiver.as_mut() {
            match rx.try_recv() {
                Ok(delivery) => {
                    let matches_request = matches!(
                        state.blame,
                        BlameState::Loading { ref path, .. } if path == &delivery.path
                    );
                    if matches_request {
                        match delivery.result {
                            Ok(blame) => {
                                if state.apply_blame_result(&delivery.path, blame) {
                                    state.blame_receiver = None;
                                    state.status = state.listing_status.clone();
                                }
                            }
                            Err(error) => {
                                let matches_open = state
                                    .open
                                    .as_ref()
                                    .is_some_and(|open| open.path == delivery.path);
                                if matches_open {
                                    state.blame_receiver = None;
                                    state.blame = BlameState::Failed;
                                    state.status = Some(error.to_string());
                                }
                            }
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.blame_receiver = None;
                    let cancelled = match state.blame {
                        BlameState::Loading { ref cancel, .. } => cancel.is_cancelled(),
                        _ => true,
                    };
                    if !cancelled {
                        state.blame = BlameState::Failed;
                        state.status = Some("blame task ended".to_string());
                    }
                }
            }
        }

        if let Some(rx) = state.commit_diff_receiver.as_mut() {
            match rx.try_recv() {
                Ok(delivery) => {
                    let matches_request = matches!(
                        state.commit_diff,
                        BrowseCommitDiffState::Loading {
                            request_id,
                            ref annotation,
                            ..
                        } if request_id == delivery.request_id
                            && annotation.sha() == delivery.sha
                    );
                    if matches_request {
                        state.commit_diff_receiver = None;
                        let annotation = match &state.commit_diff {
                            BrowseCommitDiffState::Loading { annotation, .. } => {
                                Arc::clone(annotation)
                            }
                            _ => unreachable!("matching request must still be loading"),
                        };
                        state.commit_diff = match delivery.result {
                            Ok(cache) => {
                                let mut scroll = DiffScrollState::new(ScrollMode::Margin);
                                scroll.set_line_count(cache.lines.len());
                                BrowseCommitDiffState::Ready {
                                    annotation,
                                    cache,
                                    scroll,
                                }
                            }
                            Err(message) => BrowseCommitDiffState::Failed {
                                annotation,
                                message,
                            },
                        };
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.commit_diff_receiver = None;
                    let failed = match &state.commit_diff {
                        BrowseCommitDiffState::Loading {
                            annotation, cancel, ..
                        } if !cancel.is_cancelled() => Some(Arc::clone(annotation)),
                        _ => None,
                    };
                    if let Some(annotation) = failed {
                        state.commit_diff = BrowseCommitDiffState::Failed {
                            annotation,
                            message: "commit diff task ended".to_string(),
                        };
                    }
                }
            }
        }

        if let Some(rx) = state.pr_lookup_receiver.as_mut() {
            match rx.try_recv() {
                Ok(delivery) => {
                    let matches_request = matches!(
                        state.pr_lookup,
                        PrLookupState::Loading {
                            request_id,
                            ref sha,
                            ..
                        } if request_id == delivery.request_id && sha == &delivery.sha
                    );
                    if matches_request {
                        state.pr_lookup_receiver = None;
                        match delivery.result {
                            Ok(resolution) => {
                                completed_pr_lookup = Some((delivery.sha, resolution));
                            }
                            Err(error) => {
                                state.pr_lookup = PrLookupState::Failed {
                                    sha: delivery.sha,
                                    failure: PrLookupFailure::Lookup(error),
                                };
                                state.status = Some(error.to_string());
                            }
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.pr_lookup_receiver = None;
                    let failed_sha = match &state.pr_lookup {
                        PrLookupState::Loading { sha, cancel, .. } if !cancel.is_cancelled() => {
                            Some(sha.clone())
                        }
                        _ => None,
                    };
                    if let Some(sha) = failed_sha {
                        state.pr_lookup = PrLookupState::Failed {
                            sha,
                            failure: PrLookupFailure::Lookup(CommitPrLookupError::ApiFailure),
                        };
                        state.status = Some(CommitPrLookupError::ApiFailure.to_string());
                    }
                }
            }
        }

        if let Some(rx) = state.line_discussion_receiver.as_mut() {
            match rx.try_recv() {
                Ok(delivery) => {
                    let matches_request = match (&state.line_discussion, &delivery) {
                        (
                            LineDiscussionState::ResolvingPullRequests {
                                request_id, path, ..
                            },
                            LineDiscussionDelivery::PullRequests {
                                request_id: delivered_id,
                                path: delivered_path,
                                ..
                            },
                        ) => request_id == delivered_id && path == delivered_path,
                        (
                            LineDiscussionState::LoadingComments {
                                request_id,
                                path,
                                pr_numbers,
                                ..
                            },
                            LineDiscussionDelivery::Comments {
                                request_id: delivered_id,
                                path: delivered_path,
                                pr_numbers: delivered_prs,
                                ..
                            },
                        ) => {
                            request_id == delivered_id
                                && path == delivered_path
                                && pr_numbers == delivered_prs
                        }
                        _ => false,
                    };
                    if matches_request && state.line_discussion_request_matches_context() {
                        state.line_discussion_receiver = None;
                        completed_line_discussion = Some(delivery);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.line_discussion_receiver = None;
                    let failed_path = match &state.line_discussion {
                        LineDiscussionState::ResolvingPullRequests { path, cancel, .. }
                        | LineDiscussionState::LoadingComments { path, cancel, .. }
                            if !cancel.is_cancelled() =>
                        {
                            Some(path.clone())
                        }
                        _ => None,
                    };
                    if failed_path.is_some() {
                        state.status = Some("Review discussion task ended".to_string());
                        state.line_discussion = LineDiscussionState::Failed {
                            failure: LineDiscussionFailure::Api,
                        };
                    }
                }
            }
        }

        if let Some(rx) = state.highlight_receiver.as_mut() {
            match rx.try_recv() {
                Ok((path, cache)) => {
                    state.highlight_receiver = None;
                    state.apply_highlighted_cache(&path, cache);
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.highlight_receiver = None;
                }
            }
        }

        if file_ready {
            let waiting_path = match (&state.module_graph_pane, &state.open) {
                (ModuleGraphPaneState::Waiting { path }, Some(open)) if path == &open.path => {
                    Some(path.clone())
                }
                _ => None,
            };
            if let Some(path) = waiting_path {
                if let Err(message) = state.start_module_graph_query_for_path(path) {
                    state.close_module_graph_pane();
                    state.status = Some(message.to_string());
                    if self.state == AppState::RepoBrowseGraph {
                        self.state = AppState::RepoBrowseFile;
                    }
                }
            }
        }

        if paths_ready {
            self.start_symbol_index_build();
        }
        if file_ready {
            self.start_browse_highlight();
            self.start_browse_blame();
        }
        // A listing error is most useful with its wide, scrollable detail pane
        // focused, but an asynchronous completion must not steal focus from
        // Help or another screen the user has already entered.
        if focus_listing_failure && self.state == AppState::RepoBrowseTree {
            self.state = AppState::RepoBrowseFile;
        }
        if let Some((sha, resolution)) = completed_pr_lookup {
            self.install_pr_lookup_resolution(sha, resolution);
        }
        if let Some(delivery) = completed_line_discussion {
            self.install_line_discussion_delivery(delivery);
        }
    }

    /// Kick off one repository-wide symbol/import analysis on a background thread.
    fn start_symbol_index_build(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let LoadState::Loaded(ref paths) = state.paths else {
            return;
        };
        // `paths_receiver` is installed only by `open_repo_browse` for a fresh
        // `BrowseState` and consumed once by `poll_browse_updates`; same-root
        // reuse installs no second receiver. A second build therefore cannot
        // start within one state today, and this guard keeps that invariant
        // safe if refresh support is added later.
        if matches!(state.index, IndexState::Building)
            || matches!(state.module_graph, ModuleGraphState::Building)
        {
            return;
        }

        let paths = paths.clone();
        let repo_root = state.repo_root.clone();
        let universe = state.source_universe;
        let (tx, rx) = mpsc::channel(1);
        state.index = IndexState::Building;
        state.module_graph = ModuleGraphState::Building;
        state.index_receiver = Some(rx);
        let cancel_token = state.cancel_token.clone();

        tokio::task::spawn_blocking(move || {
            match CodeIndex::build_cancellable(&repo_root, &paths, universe, &cancel_token) {
                CodeIndexBuild::Completed(code) => {
                    let _ = tx.blocking_send(IndexDelivery::Ready(code));
                }
                CodeIndexBuild::Cancelled { .. } => {}
                CodeIndexBuild::Failed { message } => {
                    let _ = tx.blocking_send(IndexDelivery::Failed(message));
                }
            }
        });
    }

    /// Open the file currently selected in the tree.
    pub(crate) fn browse_open_selected(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let Some(path) = state.selected_path().map(str::to_string) else {
            return;
        };
        // Re-selecting the file that is already open (or still loading) is a
        // focus change, not a jump: keep the cursor where it was. A failed
        // load falls through so re-selecting retries it.
        let already_open = match state.open_load {
            OpenLoad::Pending {
                path: ref pending, ..
            } => *pending == path,
            OpenLoad::Idle => state.open.as_ref().is_some_and(|open| open.path == path),
            OpenLoad::Failed { .. } => false,
        };
        if already_open {
            self.state = AppState::RepoBrowseFile;
            return;
        }
        self.browse_open_path(&path, 0);
    }

    /// Open `path` (repository-relative) and place the cursor on `line` (0-based).
    pub(crate) fn browse_open_path(&mut self, path: &str, line: usize) {
        self.browse_open_path_at(path, line, None);
    }

    /// Open a file and optionally restore an exact recorded scroll position.
    ///
    /// The file pane is entered immediately and deliberately remains the
    /// failure pane if the asynchronous load fails: the complete error stays
    /// visible next to the tree and `q`/`Esc` still returns to that tree. The
    /// completion path never changes [`App::state`], so a user who already
    /// returned to the tree is not yanked back by a late failure.
    fn browse_open_path_at(&mut self, path: &str, line: usize, scroll: Option<usize>) {
        let tab_width = self.config.diff.tab_width;

        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let module_graph_visible = state.module_graph_pane.is_visible();
        state.cancel_commit_diff_request();
        state.commit_diff = BrowseCommitDiffState::Off;

        let pending_same_path = match state.open_load {
            OpenLoad::Pending {
                path: ref pending_path,
                line: ref mut pending_line,
                scroll: ref mut pending_scroll,
                ..
            } if pending_path == path => {
                *pending_line = line;
                *pending_scroll = scroll;
                true
            }
            OpenLoad::Idle | OpenLoad::Pending { .. } | OpenLoad::Failed { .. } => false,
        };
        if pending_same_path {
            state.focus_line(line);
            if let Some(scroll) = scroll {
                state.scroll_offset = scroll;
            }
            self.state = AppState::RepoBrowseFile;
            return;
        }

        if matches!(state.open_load, OpenLoad::Idle)
            && state.open.as_ref().is_some_and(|open| open.path == path)
        {
            state.focus_line(line);
            if let Some(scroll) = scroll {
                state.scroll_offset = scroll;
            }
            self.state = AppState::RepoBrowseFile;
            return;
        }

        let blame_active = !matches!(state.blame, BlameState::Off);
        state.cancel_blame_request();
        state.cancel_line_discussion_request();
        state.line_discussion = LineDiscussionState::Idle;
        if blame_active {
            state.blame = BlameState::Waiting {
                path: path.to_string(),
            };
        }

        if let OpenLoad::Pending { ref cancel, .. } = state.open_load {
            cancel.cancel();
        }
        state.file_receiver = None;
        state.highlight_receiver = None;

        let absolute = state.repo_root.join(path);
        state.status = None;
        // `OpenLoad` is the authoritative lifecycle state. This placeholder is
        // only its immediate rendering: `src/ui/browse.rs` renders
        // `OpenFile::notice` to show progress while every filesystem/cache step
        // stays off-thread.
        state.open = Some(unviewable(path, "Loading…".to_string(), tab_width));
        state.focus_line(0);
        state.sync_tree_to_open_file();
        let request = state.cancel_token.child_token();
        state.open_load = OpenLoad::Pending {
            path: path.to_string(),
            line,
            scroll,
            cancel: request.clone(),
        };

        let (tx, rx) = mpsc::channel(1);
        state.file_receiver = Some(rx);
        let delivery_path = path.to_string();
        tokio::task::spawn_blocking(move || {
            let load = load_file(&absolute, &delivery_path, tab_width, &request);
            deliver_file_load(load, delivery_path, &request, &tx);
        });

        if module_graph_visible {
            state.cancel_module_graph_query();
            state.module_graph_pane = ModuleGraphPaneState::Waiting {
                path: path.to_string(),
            };
        }
        self.state = AppState::RepoBrowseFile;
    }

    pub(crate) fn toggle_browse_blame(&mut self) {
        {
            let Some(state) = self.browse_state.as_mut() else {
                return;
            };

            if matches!(
                state.blame,
                BlameState::Waiting { .. } | BlameState::Loading { .. } | BlameState::Ready { .. }
            ) {
                state.cancel_blame_request();
                state.cancel_line_discussion_request();
                state.blame = BlameState::Off;
                state.line_discussion = LineDiscussionState::Idle;
                return;
            }

            let Some(open) = state.open.as_ref() else {
                state.blame = BlameState::Off;
                state.status = Some("Blame is unavailable for this file".to_string());
                return;
            };
            if state.open_is_pending() {
                state.blame = BlameState::Off;
                state.status = Some("Still opening this file".to_string());
                return;
            }
            if !open.viewable {
                state.blame = BlameState::Off;
                state.status = Some("Blame is unavailable for this file".to_string());
                return;
            }

            state.status = None;
            state.blame = BlameState::Waiting {
                path: open.path.clone(),
            };
        }
        self.start_browse_blame();
    }

    fn start_browse_blame(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let BlameState::Waiting { path } = &state.blame else {
            return;
        };
        let path = path.clone();
        let can_blame = state
            .open
            .as_ref()
            .is_some_and(|open| open.path == path && open.viewable);
        if !can_blame {
            state.blame = BlameState::Off;
            state.status = Some("Blame is unavailable for this file".to_string());
            return;
        }

        state.cancel_blame_request();
        let cancel = state.cancel_token.child_token();
        state.blame = BlameState::Loading {
            path: path.clone(),
            cancel: cancel.clone(),
        };
        let (tx, rx) = mpsc::channel(1);
        state.blame_receiver = Some(rx);
        let repo_root = state.repo_root.clone();
        tokio::task::spawn_blocking(move || {
            let result = blame_file(&repo_root, &path);
            deliver_blame_load(result, path, &cancel, &tx);
        });
    }

    pub(crate) fn open_browse_blame_commit(&mut self) {
        let toggle_blame = self.config.keybindings.toggle_blame.display();
        let annotation = {
            let Some(state) = self.browse_state.as_mut() else {
                return;
            };
            if state.open_is_pending() {
                state.status = Some("Still opening this file".to_string());
                return;
            }
            let Some(open) = state.open.as_ref() else {
                state.status = Some("No file is open".to_string());
                return;
            };

            let gutter = match &state.blame {
                BlameState::Waiting { .. } | BlameState::Loading { .. } => {
                    state.status = Some("Blame is still loading".to_string());
                    return;
                }
                BlameState::Ready { path, gutter } if path == &open.path => gutter,
                BlameState::Off => {
                    state.status = Some(format!("Blame is off — press {toggle_blame} to enable"));
                    return;
                }
                BlameState::Failed => {
                    state.status = Some("Blame failed for this file".to_string());
                    return;
                }
                BlameState::Ready { .. } => {
                    state.status = Some("Blame belongs to another file".to_string());
                    return;
                }
            };
            let Some(annotation) = gutter.annotation_at(state.cursor_line) else {
                state.status = Some("Blame is unavailable for this line".to_string());
                return;
            };
            if annotation.is_uncommitted() {
                state.status = Some("Uncommitted line has no commit to open".to_string());
                return;
            }
            Arc::clone(annotation)
        };

        self.browse_push_jump();
        self.start_browse_commit_diff(annotation);
    }

    pub(crate) fn open_browse_blame_pr(&mut self) {
        if !self.repository_availability().is_available() {
            self.set_browse_status("No GitHub repository is associated with this browser session");
            return;
        }

        let toggle_blame = self.config.keybindings.toggle_blame.display();
        let annotation = {
            let Some(state) = self.browse_state.as_mut() else {
                return;
            };
            if state.open_is_pending() {
                state.status = Some("Still opening this file".to_string());
                return;
            }
            let Some(open) = state.open.as_ref() else {
                state.status = Some("No file is open".to_string());
                return;
            };
            let gutter = match &state.blame {
                BlameState::Waiting { .. } | BlameState::Loading { .. } => {
                    state.status = Some("Blame is still loading".to_string());
                    return;
                }
                BlameState::Ready { path, gutter } if path == &open.path => gutter,
                BlameState::Off => {
                    state.status = Some(format!("Blame is off — press {toggle_blame} to enable"));
                    return;
                }
                BlameState::Failed => {
                    state.status = Some("Blame failed for this file".to_string());
                    return;
                }
                BlameState::Ready { .. } => {
                    state.status = Some("Blame belongs to another file".to_string());
                    return;
                }
            };
            let Some(annotation) = gutter.annotation_at(state.cursor_line) else {
                state.status = Some("Blame is unavailable for this line".to_string());
                return;
            };
            if annotation.is_uncommitted() {
                state.status = Some("Uncommitted line has no pull request".to_string());
                return;
            }
            Arc::clone(annotation)
        };

        let sha = annotation.sha().to_string();
        if let Some(resolution) = self.session_cache.get_commit_pr_resolution(&sha).cloned() {
            self.install_pr_lookup_resolution(sha, resolution);
            return;
        }
        self.start_browse_pr_lookup(sha, annotation.summary().to_string());
    }

    pub(crate) fn open_browse_line_discussion(&mut self) {
        if !self.repository_availability().is_available() {
            self.set_browse_status("No GitHub repository is associated with this browser session");
            return;
        }

        let toggle_blame = self.config.keybindings.toggle_blame.display();
        let (path, commits, limit) = {
            let Some(state) = self.browse_state.as_mut() else {
                return;
            };
            if state.open_is_pending() {
                state.status = Some("Still opening this file".to_string());
                return;
            }
            let Some(open) = state.open.as_ref() else {
                state.status = Some("No file is open".to_string());
                return;
            };
            let gutter = match &state.blame {
                BlameState::Waiting { .. } | BlameState::Loading { .. } => {
                    state.status = Some("Blame is still loading".to_string());
                    return;
                }
                BlameState::Ready { path, gutter } if path == &open.path => gutter,
                BlameState::Off => {
                    state.status = Some(format!("Blame is off — press {toggle_blame} to enable"));
                    return;
                }
                BlameState::Failed => {
                    state.status = Some("Blame failed for this file".to_string());
                    return;
                }
                BlameState::Ready { .. } => {
                    state.status = Some("Blame belongs to another file".to_string());
                    return;
                }
            };
            let path = open.path.clone();
            if let LineDiscussionState::Ready {
                path: indexed_path,
                pr_numbers,
                index,
                view,
            } = &mut state.line_discussion
            {
                if indexed_path == &path && !index.thread_indices_at(state.cursor_line).is_empty() {
                    *view = DiscussionView::ThreadList {
                        line: state.cursor_line,
                        selected: 0,
                        scroll: 0,
                    };
                    state.status = None;
                    return;
                } else if indexed_path == &path {
                    state.status = Some(line_discussion_closed_status(&path, pr_numbers, index));
                    return;
                }
            }

            let mut commits = gutter.discussion_commits();
            if commits.is_empty() {
                state.status =
                    Some("This file has no committed lines to look up on GitHub".to_string());
                return;
            }
            let omitted_commits = commits.len().saturating_sub(MAX_DISCUSSION_COMMIT_LOOKUPS);
            commits.truncate(MAX_DISCUSSION_COMMIT_LOOKUPS);
            (
                path,
                commits,
                DiscussionLookupLimit {
                    omitted_commits,
                    omitted_pull_requests: 0,
                },
            )
        };

        self.start_line_discussion_pr_lookups(path, commits, limit);
    }

    fn start_line_discussion_pr_lookups(
        &mut self,
        path: String,
        commits: Vec<(String, String)>,
        limit: DiscussionLookupLimit,
    ) {
        let mut resolutions = Vec::with_capacity(commits.len());
        let mut missing = Vec::new();
        // One file can repeat the same blame commit thousands of times. The
        // distinct-SHA walk below, plus SessionCache's per-SHA resolution,
        // makes every repeat and every later keypress free. The explicit cap
        // bounds the genuinely cold API fan-out for adversarial histories.
        for (sha, subject) in commits {
            if let Some(resolution) = self.session_cache.get_commit_pr_resolution(&sha).cloned() {
                resolutions.push((sha, resolution));
            } else {
                missing.push((sha, subject));
            }
        }

        if missing.is_empty() {
            self.install_line_discussion_pr_resolutions(path, resolutions, limit);
            return;
        }

        let repo = self.repo.clone();
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let (request_id, cancel) = state.begin_line_discussion_resolution(path.clone());
        let (tx, rx) = mpsc::channel(1);
        state.line_discussion_receiver = Some(rx);

        tokio::spawn(async move {
            let result = async {
                for (sha, subject) in missing {
                    let resolution = tokio::select! {
                        _ = cancel.cancelled() => return Err(CommitPrLookupError::ApiFailure),
                        result = crate::github::fetch_commit_pull_requests(&repo, &sha, &subject) => result,
                    }?;
                    resolutions.push((sha, resolution));
                }
                Ok(resolutions)
            }
            .await;
            if cancel.is_cancelled() {
                return;
            }
            let _ = tx
                .send(LineDiscussionDelivery::PullRequests {
                    request_id,
                    path,
                    limit,
                    result,
                })
                .await;
        });
    }

    fn install_line_discussion_pr_resolutions(
        &mut self,
        path: String,
        resolutions: Vec<(String, CommitPrResolution)>,
        mut limit: DiscussionLookupLimit,
    ) {
        let mut seen = HashSet::new();
        let mut pr_numbers = Vec::new();
        for (sha, resolution) in resolutions {
            self.session_cache
                .put_commit_pr_resolution(sha, resolution.clone());
            match resolution {
                CommitPrResolution::Confirmed { pulls } => {
                    for pull in pulls {
                        if seen.insert(pull.number) {
                            pr_numbers.push(pull.number);
                        }
                    }
                }
                CommitPrResolution::Inferred { .. } | CommitPrResolution::NotFound => {}
            }
        }

        limit.omitted_pull_requests = pr_numbers
            .len()
            .saturating_sub(MAX_DISCUSSION_PULL_REQUESTS);
        pr_numbers.truncate(MAX_DISCUSSION_PULL_REQUESTS);
        if pr_numbers.is_empty() {
            if let Some(state) = self.browse_state.as_mut() {
                state.status = Some("No confirmed pull request found for this file".to_string());
                state.line_discussion = LineDiscussionState::Failed {
                    failure: LineDiscussionFailure::NoPullRequest,
                };
            }
            return;
        }

        self.start_line_discussion_comments(path, pr_numbers, limit);
    }

    pub(crate) fn start_line_discussion_comments(
        &mut self,
        path: String,
        pr_numbers: Vec<u32>,
        limit: DiscussionLookupLimit,
    ) {
        let mut cached = Vec::new();
        let mut missing = Vec::new();
        for &pr_number in &pr_numbers {
            let cache_key = crate::cache::PrCacheKey {
                repo: self.repo.clone(),
                pr_number,
            };
            let comments = self
                .session_cache
                .get_browser_review_comments(&cache_key)
                .map(<[crate::github::comment::ReviewComment]>::to_vec)
                .or_else(|| {
                    self.session_cache
                        .get_review_comments(&cache_key)
                        .map(|comments| {
                            comments
                                .iter()
                                .filter(|comment| comment.path != "[PR Review]")
                                .cloned()
                                .collect()
                        })
                });
            if let Some(comments) = comments {
                cached.push((pr_number, comments));
            } else {
                missing.push(pr_number);
            }
        }

        let repo = self.repo.clone();
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let (origins, repo_root) = match &state.blame {
            BlameState::Ready {
                path: blame_path,
                gutter,
            } if blame_path == &path => (gutter.origins(), state.repo_root.clone()),
            _ => {
                state.status = Some("Blame is unavailable for this file".to_string());
                return;
            }
        };
        let (request_id, cancel) =
            state.begin_line_discussion_load(path.clone(), pr_numbers.clone());
        let (tx, rx) = mpsc::channel(1);
        state.line_discussion_receiver = Some(rx);
        let current_path = path.clone();

        tokio::spawn(async move {
            let mut fetched_comments = Vec::new();
            let mut comment_sets = cached;
            for pr_number in missing {
                let result = tokio::select! {
                    _ = cancel.cancelled() => return,
                    result = crate::github::comment::fetch_review_comments(&repo, pr_number) => result,
                };
                let comments = match result {
                    Ok(comments) => comments,
                    Err(error) => {
                        let _ = tx
                            .send(LineDiscussionDelivery::Comments {
                                request_id,
                                path,
                                pr_numbers,
                                fetched_comments,
                                result: Err(LineDiscussionLoadError::Api(format!(
                                    "pull request #{pr_number}: {error}"
                                ))),
                            })
                            .await;
                        return;
                    }
                };
                fetched_comments.push((pr_number, comments.clone()));
                comment_sets.push((pr_number, comments));
            }
            if cancel.is_cancelled() {
                return;
            }
            let mut comment_ids = HashSet::new();
            let comments = comment_sets
                .into_iter()
                .flat_map(|(_, comments)| comments)
                .filter(|comment| comment_ids.insert(comment.id))
                .collect();

            let build_cancel = cancel.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut index = super::browse_discussion::build_discussion_index(
                    comments,
                    &current_path,
                    &origins,
                    |revision, path, start, end| {
                        crate::github::blame_file_at_revision_range(
                            &repo_root, revision, path, start, end,
                        )
                    },
                    || build_cancel.is_cancelled(),
                )
                .map_err(LineDiscussionLoadError::Anchor)?;
                if let Some(outcome) = limit.outcome() {
                    index.outcome = outcome;
                }
                Ok(index)
            })
            .await
            .map_err(|error| {
                LineDiscussionLoadError::Anchor(format!(
                    "review discussion indexing task failed: {error}"
                ))
            })
            .and_then(|result| result);
            if cancel.is_cancelled() {
                return;
            }
            let _ = tx
                .send(LineDiscussionDelivery::Comments {
                    request_id,
                    path,
                    pr_numbers,
                    fetched_comments,
                    result,
                })
                .await;
        });
    }

    fn install_line_discussion_delivery(&mut self, delivery: LineDiscussionDelivery) {
        let matches_runtime_context = {
            let Some(state) = self.browse_state.as_ref() else {
                return;
            };
            let state_matches = match (&state.line_discussion, &delivery) {
                (
                    LineDiscussionState::ResolvingPullRequests {
                        request_id, path, ..
                    },
                    LineDiscussionDelivery::PullRequests {
                        request_id: delivered_id,
                        path: delivered_path,
                        ..
                    },
                ) => request_id == delivered_id && path == delivered_path,
                (
                    LineDiscussionState::LoadingComments {
                        request_id,
                        path,
                        pr_numbers,
                        ..
                    },
                    LineDiscussionDelivery::Comments {
                        request_id: delivered_id,
                        path: delivered_path,
                        pr_numbers: delivered_prs,
                        ..
                    },
                ) => {
                    request_id == delivered_id
                        && path == delivered_path
                        && pr_numbers == delivered_prs
                }
                _ => false,
            };
            state_matches && state.line_discussion_request_matches_context()
        };
        if !matches_runtime_context {
            return;
        }

        match delivery {
            LineDiscussionDelivery::PullRequests {
                path,
                limit,
                result,
                ..
            } => match result {
                Ok(resolution) => {
                    self.install_line_discussion_pr_resolutions(path, resolution, limit);
                }
                Err(error) => {
                    if let Some(state) = self.browse_state.as_mut() {
                        state.status = Some(format!("Pull request lookup failed: {error}"));
                        state.line_discussion = LineDiscussionState::Failed {
                            failure: LineDiscussionFailure::Api,
                        };
                    }
                }
            },
            LineDiscussionDelivery::Comments {
                path,
                pr_numbers,
                fetched_comments,
                result,
                ..
            } => {
                for (pr_number, comments) in fetched_comments {
                    self.session_cache.put_browser_review_comments(
                        crate::cache::PrCacheKey {
                            repo: self.repo.clone(),
                            pr_number,
                        },
                        comments,
                    );
                }
                match result {
                    Ok(index) => {
                        let cursor_line = self
                            .browse_state
                            .as_ref()
                            .map_or(0, |state| state.cursor_line);
                        let view = if index.thread_indices_at(cursor_line).is_empty() {
                            DiscussionView::Closed
                        } else {
                            DiscussionView::ThreadList {
                                line: cursor_line,
                                selected: 0,
                                scroll: 0,
                            }
                        };
                        let status = if !matches!(view, DiscussionView::Closed) {
                            None
                        } else {
                            Some(line_discussion_closed_status(&path, &pr_numbers, &index))
                        };
                        if let Some(state) = self.browse_state.as_mut() {
                            state.status = status;
                            state.line_discussion = LineDiscussionState::Ready {
                                path,
                                pr_numbers,
                                index,
                                view,
                            };
                        }
                    }
                    Err(LineDiscussionLoadError::Api(message)) => {
                        if let Some(state) = self.browse_state.as_mut() {
                            state.status = Some(format!("Review comment API failed: {message}"));
                            state.line_discussion = LineDiscussionState::Failed {
                                failure: LineDiscussionFailure::Api,
                            };
                        }
                    }
                    Err(LineDiscussionLoadError::Anchor(message)) => {
                        if let Some(state) = self.browse_state.as_mut() {
                            state.status =
                                Some(format!("Review comments could not be anchored: {message}"));
                            state.line_discussion = LineDiscussionState::Failed {
                                failure: LineDiscussionFailure::Anchor,
                            };
                        }
                    }
                }
            }
        }
    }

    fn start_browse_pr_lookup(&mut self, sha: String, subject: String) {
        let repo = self.repo.clone();
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let (request_id, cancel) = state.begin_pr_lookup(&sha);
        let (tx, rx) = mpsc::channel(1);
        state.pr_lookup_receiver = Some(rx);

        tokio::spawn(async move {
            let result = tokio::select! {
                _ = cancel.cancelled() => return,
                result = crate::github::fetch_commit_pull_requests(&repo, &sha, &subject) => result,
            };
            if cancel.is_cancelled() {
                return;
            }
            let _ = tx
                .send(PrLookupLoadResult {
                    request_id,
                    sha,
                    result,
                })
                .await;
        });
    }

    fn install_pr_lookup_resolution(&mut self, sha: String, resolution: CommitPrResolution) {
        self.session_cache
            .put_commit_pr_resolution(sha.clone(), resolution.clone());
        match resolution {
            CommitPrResolution::Confirmed { mut pulls } if pulls.len() == 1 => {
                let pull = pulls.remove(0);
                self.open_pr_from_browse(pull.number, PrOpenSource::ConfirmedCommit { sha });
            }
            CommitPrResolution::Confirmed { pulls } if !pulls.is_empty() => {
                if let Some(state) = self.browse_state.as_mut() {
                    state.overlay = BrowseOverlay::None;
                    state.status = None;
                    state.pr_lookup = PrLookupState::Selecting {
                        sha,
                        pulls,
                        selected: 0,
                    };
                }
            }
            CommitPrResolution::Confirmed { .. } | CommitPrResolution::NotFound => {
                if let Some(state) = self.browse_state.as_mut() {
                    state.status = Some(format!(
                        "No pull request found for commit {}",
                        crate::github::short_sha(&sha)
                    ));
                    state.pr_lookup = PrLookupState::Failed {
                        sha,
                        failure: PrLookupFailure::NotFound,
                    };
                }
            }
            CommitPrResolution::Inferred { pull } => {
                self.open_pr_from_browse(pull.number, PrOpenSource::InferredCommitSubject { sha });
            }
        }
    }

    pub(crate) fn open_pr_from_browse(&mut self, pr_number: u32, source: PrOpenSource) {
        if let Some(state) = self.browse_state.take() {
            state.cancel_token.cancel();
        }
        if self.local_mode {
            self.deactivate_watcher();
            self.local_mode = false;
        }
        self.select_pr(pr_number);
        self.pr_open_source = source;
    }

    fn start_browse_commit_diff(&mut self, annotation: Arc<BlameAnnotation>) {
        let use_local = self.local_mode || self.pr_number.is_none();
        let working_dir = self.working_dir.clone();
        let repo = self.repo.clone();
        let theme = self.config.diff.theme.clone();
        let tab_width = self.config.diff.tab_width;

        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        state.cancel_pr_lookup_request();
        state.pr_lookup = PrLookupState::Idle;
        state.cancel_commit_diff_request();
        state.commit_diff_generation = state.commit_diff_generation.wrapping_add(1);
        let request_id = state.commit_diff_generation;
        let cancel = state.cancel_token.child_token();
        state.commit_diff = BrowseCommitDiffState::Loading {
            request_id,
            annotation: Arc::clone(&annotation),
            cancel: cancel.clone(),
        };
        state.status = None;

        let (tx, rx) = mpsc::channel(1);
        state.commit_diff_receiver = Some(rx);
        let sha = annotation.sha().to_string();
        tokio::spawn(async move {
            let fetch = async {
                if use_local {
                    crate::github::fetch_local_commit_diff(working_dir.as_deref(), &sha).await
                } else {
                    crate::github::fetch_commit_diff(&repo, &sha).await
                }
            };
            let fetched = tokio::select! {
                _ = cancel.cancelled() => return,
                result = fetch => result.map_err(|error| error.to_string()),
            };
            if cancel.is_cancelled() {
                return;
            }

            let result = match fetched.and_then(admit_commit_diff_for_cache) {
                Ok(diff_text) => {
                    let build_cancel = cancel.clone();
                    match tokio::task::spawn_blocking(move || {
                        if build_cancel.is_cancelled() {
                            return None;
                        }
                        let mut parser_pool = ParserPool::new();
                        let cache = crate::ui::diff_view::build_commit_diff_cache(
                            &diff_text,
                            &theme,
                            &mut parser_pool,
                            tab_width,
                        );
                        (!build_cancel.is_cancelled()).then_some(cache)
                    })
                    .await
                    {
                        Ok(Some(cache)) => Ok(cache),
                        Ok(None) => return,
                        Err(error) => Err(format!("commit diff cache task failed: {error}")),
                    }
                }
                Err(message) => Err(message),
            };

            let delivery = CommitDiffLoadResult {
                request_id,
                sha,
                result,
            };
            deliver_commit_diff_load(delivery, &cancel, &tx);
        });
    }

    pub(crate) fn browse_commit_diff_is_active(&self) -> bool {
        self.browse_state
            .as_ref()
            .is_some_and(|state| state.commit_diff.is_active())
    }

    pub(crate) fn return_from_browse_commit_diff(&mut self) -> bool {
        if !self.browse_commit_diff_is_active() {
            return false;
        }
        self.browse_jump_back()
    }

    /// Upgrade the open file's immediate plain cache on a background thread.
    fn start_browse_highlight(&mut self) {
        let tab_width = self.config.diff.tab_width;
        let theme = self.config.diff.theme.clone();
        let markdown_rich = self.is_markdown_rich();
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let Some(open) = state.open.as_ref() else {
            return;
        };
        if !open.viewable {
            return;
        }

        let (tx, rx) = mpsc::channel(1);
        state.highlight_receiver = Some(rx);
        let path = open.path.clone();
        let patch = open.patch.clone();
        let cancel_token = state.cancel_token.clone();
        tokio::task::spawn_blocking(move || {
            deliver_highlighted_cache(&cancel_token, &tx, || {
                let mut pool = ParserPool::new();
                let cache = crate::ui::diff_view::build_diff_cache(
                    &patch,
                    &path,
                    &theme,
                    &mut pool,
                    markdown_rich,
                    tab_width,
                );
                (path, cache)
            });
        });
    }

    /// Toggle markdown rich display and rebuild the open markdown file's
    /// highlight so the change shows without reopening the file.
    pub(crate) fn toggle_browse_markdown_rich(&mut self) {
        self.toggle_markdown_rich();
        let Some(state) = self.browse_state.as_ref() else {
            return;
        };
        // A pending `open` is a placeholder; highlighting it would race the
        // landing file's cache with an empty one. The landing load starts its
        // own highlight and reads the new flag then.
        if state.open_is_pending() {
            return;
        }
        // The flag only changes markdown output, so rebuilding any other
        // language would be wasted work.
        let is_markdown = state
            .open
            .as_ref()
            .is_some_and(|open| crate::language::is_markdown_ext_from_filename(&open.path));
        if is_markdown {
            self.start_browse_highlight();
        }
    }

    /// Jump to the definition of the identifier under the cursor.
    ///
    /// Returns `false` when there is no index yet, no identifier under the
    /// cursor, or no definition for it — the caller surfaces the reason.
    pub(crate) fn browse_go_to_definition(&mut self) -> bool {
        let Some(state) = self.browse_state.as_ref() else {
            return false;
        };
        let IndexState::Ready(ref index) = state.index else {
            return false;
        };
        let Some(open) = state.open.as_ref() else {
            return false;
        };
        let Some(line) = open.source_line(state.cursor_line) else {
            return false;
        };

        let identifiers = crate::symbol::extract_all_identifiers(line);
        if identifiers.is_empty() {
            return false;
        }

        // Prefer a definition for the *first* identifier that resolves, which
        // for `foo.bar()` and `let x = Config::new()` alike is the reading
        // order a human would try.
        let index = Arc::clone(index);
        for (name, _, _) in identifiers {
            let hits = index.definitions(&name);
            let Some(hit) = hits.first() else {
                continue;
            };
            let path = hit.path.to_string();
            let target_line = hit.symbol.line.saturating_sub(1);
            self.browse_push_jump();
            self.browse_open_path(&path, target_line);
            return true;
        }

        false
    }

    /// Record the current position so `jump_back` can return to it.
    pub(crate) fn browse_push_jump(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let Some(open) = state.open.as_ref() else {
            return;
        };
        let entry = BrowseJump {
            path: open.path.clone(),
            line: state.cursor_line,
            scroll: state.scroll_offset,
        };
        state.jump_stack.push(entry);
        // Same bound as the diff view's jump stack.
        if state.jump_stack.len() > 100 {
            state.jump_stack.remove(0);
        }
    }

    /// Return to the position recorded by the most recent jump.
    pub(crate) fn browse_jump_back(&mut self) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        state.cancel_commit_diff_request();
        state.commit_diff = BrowseCommitDiffState::Off;
        let Some(entry) = state.jump_stack.pop() else {
            return false;
        };
        self.browse_open_path_at(&entry.path, entry.line, Some(entry.scroll));
        true
    }
}

fn discussion_pr_label(pr_numbers: &[u32]) -> String {
    match pr_numbers {
        [] => "No pull requests".to_string(),
        [number] => format!("Pull request #{number}"),
        numbers => format!(
            "Pull requests {}",
            numbers
                .iter()
                .map(|number| format!("#{number}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn line_discussion_closed_status(
    current_path: &str,
    pr_numbers: &[u32],
    index: &DiscussionIndex,
) -> String {
    if index.file_thread_count == 0 {
        return format!(
            "{} has no review comments on this file",
            discussion_pr_label(pr_numbers)
        );
    }

    if index.line_threads.iter().all(|threads| threads.is_empty()) {
        let previous_paths = index
            .comment_paths
            .iter()
            .filter(|path| path.as_str() != current_path)
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !previous_paths.is_empty() {
            let path_label = if previous_paths.len() == 1 {
                "previous path"
            } else {
                "previous paths"
            };
            return format!(
                "Review comments exist under {path_label} {}, but none could be anchored to {current_path}",
                previous_paths.join(", ")
            );
        }
    }

    match index.confidence_note() {
        Some(note) => {
            format!("This file has review comments, but none on this line ({note})")
        }
        None => "This file has review comments, but none on this line".to_string(),
    }
}

/// A jump-stack entry inside the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseJump {
    pub path: String,
    pub line: usize,
    pub scroll: usize,
}

/// Lifecycle of the repository symbol index.
#[derive(Debug, Default)]
pub enum IndexState {
    #[default]
    Idle,
    Building,
    Ready(Arc<SymbolIndex>),
    Failed {
        reason: String,
    },
}

impl IndexState {
    pub fn ready(&self) -> Option<&SymbolIndex> {
        match self {
            Self::Ready(index) => Some(index),
            _ => None,
        }
    }

    /// Short status text for the header.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "symbols: -",
            Self::Building => "symbols: indexing",
            Self::Ready(_) => "symbols: ready",
            Self::Failed { .. } => "symbols: unavailable",
        }
    }

    pub fn blocked_message(&self) -> Option<String> {
        match self {
            Self::Idle => Some("Symbol index has not started".to_string()),
            Self::Building => Some("Symbol index is still building".to_string()),
            Self::Failed { reason } => Some(format!("Symbol index is unavailable: {reason}")),
            Self::Ready(_) => None,
        }
    }
}

/// Lifecycle of the repository module graph built in the symbol analysis pass.
#[derive(Debug, Default)]
pub enum ModuleGraphState {
    #[default]
    Idle,
    Building,
    Ready(Arc<ModuleGraph>),
    Failed,
}

impl ModuleGraphState {
    pub fn ready(&self) -> Option<&ModuleGraph> {
        match self {
            Self::Ready(graph) => Some(graph),
            Self::Idle | Self::Building | Self::Failed => None,
        }
    }
}

/// Lifecycle of the blame annotation for the open browser file.
#[derive(Debug, Default)]
pub(crate) enum BlameState {
    #[default]
    Off,
    Waiting {
        path: String,
    },
    Loading {
        path: String,
        cancel: CancellationToken,
    },
    Ready {
        path: String,
        gutter: BlameGutter,
    },
    // The user-facing failure is kept in `BrowseState::status`.
    Failed,
}

#[derive(Default)]
pub(crate) enum BrowseCommitDiffState {
    #[default]
    Off,
    Loading {
        request_id: u64,
        annotation: Arc<BlameAnnotation>,
        cancel: CancellationToken,
    },
    Ready {
        annotation: Arc<BlameAnnotation>,
        cache: DiffCache,
        scroll: DiffScrollState,
    },
    Failed {
        annotation: Arc<BlameAnnotation>,
        message: String,
    },
}

impl BrowseCommitDiffState {
    pub(crate) fn is_active(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

#[derive(Debug)]
pub(crate) struct BlameGutter {
    rows: Vec<BlameGutterRow>,
    coverage: BlameCoverage,
    origins: OnceLock<Arc<[Option<LineOrigin>]>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlameCoverage {
    Exact,
    ShorterThanBuffer {
        blame_lines: usize,
        buffer_lines: usize,
    },
    LongerThanBuffer {
        blame_lines: usize,
        buffer_lines: usize,
    },
}

#[derive(Debug)]
enum BlameGutterRow {
    Annotation(Arc<BlameAnnotation>, u32),
    Blank(Arc<BlameAnnotation>, u32),
    Missing,
}

#[derive(Debug)]
pub(crate) struct BlameAnnotation {
    sha: Arc<str>,
    author_name: Arc<str>,
    summary: Arc<str>,
    full: Arc<str>,
    author: Arc<str>,
    identity: Arc<str>,
    original_path: Arc<str>,
}

impl BlameAnnotation {
    pub(crate) fn sha(&self) -> &str {
        &self.sha
    }

    pub(crate) fn short_sha(&self) -> &str {
        crate::github::short_sha(&self.sha)
    }

    pub(crate) fn author_name(&self) -> &str {
        &self.author_name
    }

    pub(crate) fn summary(&self) -> &str {
        &self.summary
    }

    pub(crate) fn is_uncommitted(&self) -> bool {
        !self.sha.is_empty() && self.sha.bytes().all(|byte| byte == b'0')
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum BlameGutterWidth {
    Full,
    Author,
    Identity,
}

const BLAME_FULL_BLANK: &str = "                                ";
const BLAME_AUTHOR_BLANK: &str = "                       ";
const BLAME_IDENTITY_BLANK: &str = "            ";
const BLAME_FULL_MISSING: &str = "[not blamed]                    ";
const BLAME_AUTHOR_MISSING: &str = "[not blamed]           ";
const BLAME_IDENTITY_MISSING: &str = "[not blamed]";

impl BlameGutter {
    pub(crate) fn from_file(blame: BlameFile, buffer_lines: usize) -> Self {
        debug_assert_eq!(BLAME_FULL_BLANK.width(), BLAME_FULL_WIDTH);
        debug_assert_eq!(BLAME_AUTHOR_BLANK.width(), BLAME_AUTHOR_WIDTH);
        debug_assert_eq!(BLAME_IDENTITY_BLANK.width(), BLAME_IDENTITY_WIDTH);

        let blame_lines = blame.line_count();
        let coverage = if blame_lines < buffer_lines {
            BlameCoverage::ShorterThanBuffer {
                blame_lines,
                buffer_lines,
            }
        } else if blame_lines > buffer_lines {
            BlameCoverage::LongerThanBuffer {
                blame_lines,
                buffer_lines,
            }
        } else {
            BlameCoverage::Exact
        };
        let mut annotations = HashMap::<(&str, &str), Arc<BlameAnnotation>>::new();
        let mut rows = Vec::with_capacity(buffer_lines);
        let mut previous_sha = None;

        for line in 0..buffer_lines {
            let Some(reference) = blame.at(line) else {
                rows.push(BlameGutterRow::Missing);
                previous_sha = None;
                continue;
            };
            let annotation = annotations
                .entry((reference.sha, reference.original_path))
                .or_insert_with(|| Arc::new(prepare_blame_annotation(reference)))
                .clone();
            if previous_sha == Some(reference.sha) {
                rows.push(BlameGutterRow::Blank(annotation, reference.original_line));
            } else {
                rows.push(BlameGutterRow::Annotation(
                    annotation,
                    reference.original_line,
                ));
            }
            previous_sha = Some(reference.sha);
        }

        Self {
            rows,
            coverage,
            origins: OnceLock::new(),
        }
    }

    pub(crate) fn coverage(&self) -> BlameCoverage {
        self.coverage
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn text(&self, line: usize, width: BlameGutterWidth) -> &str {
        match (self.rows.get(line), width) {
            (Some(BlameGutterRow::Annotation(annotation, _)), BlameGutterWidth::Full) => {
                &annotation.full
            }
            (Some(BlameGutterRow::Annotation(annotation, _)), BlameGutterWidth::Author) => {
                &annotation.author
            }
            (Some(BlameGutterRow::Annotation(annotation, _)), BlameGutterWidth::Identity) => {
                &annotation.identity
            }
            (Some(BlameGutterRow::Blank(_, _)), BlameGutterWidth::Full) => BLAME_FULL_BLANK,
            (Some(BlameGutterRow::Blank(_, _)), BlameGutterWidth::Author) => BLAME_AUTHOR_BLANK,
            (Some(BlameGutterRow::Blank(_, _)), BlameGutterWidth::Identity) => BLAME_IDENTITY_BLANK,
            (Some(BlameGutterRow::Missing) | None, BlameGutterWidth::Full) => BLAME_FULL_MISSING,
            (Some(BlameGutterRow::Missing) | None, BlameGutterWidth::Author) => {
                BLAME_AUTHOR_MISSING
            }
            (Some(BlameGutterRow::Missing) | None, BlameGutterWidth::Identity) => {
                BLAME_IDENTITY_MISSING
            }
        }
    }

    pub(crate) fn annotation_at(&self, line: usize) -> Option<&Arc<BlameAnnotation>> {
        match self.rows.get(line)? {
            BlameGutterRow::Annotation(annotation, _) | BlameGutterRow::Blank(annotation, _) => {
                Some(annotation)
            }
            BlameGutterRow::Missing => None,
        }
    }

    pub(crate) fn origin_at(
        &self,
        line: usize,
    ) -> Option<crate::app::browse_discussion::LineOrigin> {
        let (annotation, original_line) = match self.rows.get(line)? {
            BlameGutterRow::Annotation(annotation, original_line)
            | BlameGutterRow::Blank(annotation, original_line) => (annotation, original_line),
            BlameGutterRow::Missing => return None,
        };
        Some(crate::app::browse_discussion::LineOrigin {
            sha: Arc::clone(&annotation.sha),
            path: Arc::clone(&annotation.original_path),
            line: *original_line,
        })
    }

    fn origins(&self) -> Arc<[Option<LineOrigin>]> {
        Arc::clone(self.origins.get_or_init(|| {
            (0..self.rows.len())
                .map(|line| self.origin_at(line))
                .collect::<Arc<[_]>>()
        }))
    }

    fn discussion_commits(&self) -> Vec<(String, String)> {
        let mut seen = HashSet::new();
        self.rows
            .iter()
            .filter_map(|row| match row {
                BlameGutterRow::Annotation(annotation, _)
                | BlameGutterRow::Blank(annotation, _)
                    if !annotation.is_uncommitted()
                        && seen.insert(annotation.sha().to_string()) =>
                {
                    Some((
                        annotation.sha().to_string(),
                        annotation.summary().to_string(),
                    ))
                }
                BlameGutterRow::Annotation(_, _)
                | BlameGutterRow::Blank(_, _)
                | BlameGutterRow::Missing => None,
            })
            .collect()
    }
}

fn prepare_blame_annotation(reference: BlameRef<'_>) -> BlameAnnotation {
    if reference.is_uncommitted() {
        return BlameAnnotation {
            sha: Arc::from(reference.sha),
            author_name: Arc::from(reference.author),
            summary: Arc::from(reference.summary),
            full: Arc::from(pad_blame_text("Uncommitted".to_string(), BLAME_FULL_WIDTH)),
            author: Arc::from(pad_blame_text(
                "Uncommitted".to_string(),
                BLAME_AUTHOR_WIDTH,
            )),
            identity: Arc::from(pad_blame_text(
                "Uncommitted".to_string(),
                BLAME_IDENTITY_WIDTH,
            )),
            original_path: Arc::from(reference.original_path),
        };
    }

    let sha = reference.short_sha();
    let time = crate::github::format_relative_time_from_epoch(reference.author_time);
    let full_author_width = BLAME_FULL_WIDTH.saturating_sub(sha.width() + time.width() + 3);
    let author_width = BLAME_AUTHOR_WIDTH.saturating_sub(sha.width() + 2);
    let full_author = truncate_with_width(reference.author, full_author_width);
    let author = truncate_with_width(reference.author, author_width);

    BlameAnnotation {
        sha: Arc::from(reference.sha),
        author_name: Arc::from(reference.author),
        summary: Arc::from(reference.summary),
        full: Arc::from(pad_blame_text(
            format!("{sha} {full_author} {time}"),
            BLAME_FULL_WIDTH,
        )),
        author: Arc::from(pad_blame_text(
            format!("{sha} {author}"),
            BLAME_AUTHOR_WIDTH,
        )),
        identity: Arc::from(pad_blame_text(sha.to_string(), BLAME_IDENTITY_WIDTH)),
        original_path: Arc::from(reference.original_path),
    }
}

fn pad_blame_text(mut text: String, width: usize) -> String {
    let padding = width.saturating_sub(text.width());
    text.reserve(padding);
    text.extend(std::iter::repeat_n(' ', padding));
    text
}

/// A file opened in the browser.
pub struct OpenFile {
    /// Repository-relative path.
    pub path: String,
    /// The file rendered as an all-context pseudo-patch, so the diff
    /// highlighter can be reused verbatim.
    pub patch: String,
    pub cache: DiffCache,
    /// Source lines, retained for identifier extraction under the cursor.
    pub lines: Vec<String>,
    pub symbols: Vec<Symbol>,
    /// Whether [`App::start_browse_highlight`] may build a syntax cache.
    ///
    /// The renderer chooses between notice and content from [`Self::notice`],
    /// not this flag. The `Debug` implementation also reports this value.
    pub viewable: bool,
    /// Short content-pane message for an unviewable or deliberately empty state.
    pub notice: Option<String>,
}

impl std::fmt::Debug for OpenFile {
    /// `DiffCache` holds a string interner and is not `Debug`; the fields that
    /// matter for diagnosing a failure are the path, size and viewability.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenFile")
            .field("path", &self.path)
            .field("lines", &self.lines.len())
            .field("symbols", &self.symbols.len())
            .field("viewable", &self.viewable)
            .field("notice", &self.notice)
            .finish()
    }
}

impl OpenFile {
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Source text of a 0-based line.
    pub fn source_line(&self, line: usize) -> Option<&str> {
        self.lines.get(line).map(String::as_str)
    }
}

/// Direction selected in the module-graph pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleGraphDirection {
    Dependencies,
    Dependents,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleGraphJump {
    pub path: String,
    pub line: usize,
}

/// Precomputed UML node text for one direct relationship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleGraphRow {
    pub node: String,
    pub edge: String,
    pub jump: Option<ModuleGraphJump>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleGraphRows {
    pub rows: Vec<ModuleGraphRow>,
    pub total: usize,
    pub guarantee: DependencyGuarantee,
}

/// Precomputed dependency nodes; drawing never walks the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleGraphPanel {
    /// Full repository-relative path used to validate request context.
    pub path: String,
    /// Bounded current-module label used by the renderer.
    pub display_path: String,
    pub direction: ModuleGraphDirection,
    pub selected: usize,
    pub dependencies: ModuleGraphRows,
    pub dependents: ModuleGraphRows,
}

impl ModuleGraphPanel {
    pub fn current(&self) -> &ModuleGraphRows {
        match self.direction {
            ModuleGraphDirection::Dependencies => &self.dependencies,
            ModuleGraphDirection::Dependents => &self.dependents,
        }
    }

    pub fn current_rows(&self) -> &[ModuleGraphRow] {
        &self.current().rows
    }

    pub fn set_direction(&mut self, direction: ModuleGraphDirection) {
        self.direction = direction;
        self.selected = self
            .selected
            .min(self.current_rows().len().saturating_sub(1));
    }
}

/// Visibility and request lifecycle of the right-side module-graph pane.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum ModuleGraphPaneState {
    #[default]
    Closed,
    /// A new file is still loading; query only the last successfully opened path.
    Waiting {
        path: String,
    },
    Loading {
        request_id: u64,
        path: String,
    },
    Ready(ModuleGraphPanel),
}

impl ModuleGraphPaneState {
    pub fn is_visible(&self) -> bool {
        !matches!(self, Self::Closed)
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Closed => None,
            Self::Waiting { path } | Self::Loading { path, .. } => Some(path),
            Self::Ready(panel) => Some(&panel.path),
        }
    }
}

/// Which modal overlay is on top of the browser, if any.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum BrowseOverlay {
    #[default]
    None,
    /// Outline of the open file.
    Outline { selected: usize },
    /// Repository-wide fuzzy symbol search.
    SymbolSearch { query: String, selected: usize },
}

/// Lifecycle of the file currently being opened.
#[derive(Debug, Default)]
pub enum OpenLoad {
    #[default]
    Idle,
    Pending {
        path: String,
        line: usize,
        scroll: Option<usize>,
        cancel: CancellationToken,
    },
    Failed {
        path: String,
        message: String,
    },
}

pub(crate) struct FileLoadResult {
    path: String,
    result: Result<OpenFile, String>,
}

pub(crate) struct BlameLoadResult {
    path: String,
    result: Result<BlameFile, BlameError>,
}

pub(crate) struct CommitDiffLoadResult {
    request_id: u64,
    sha: String,
    result: Result<DiffCache, String>,
}

pub(crate) struct ModuleGraphPanelDelivery {
    request_id: u64,
    path: String,
    panel: ModuleGraphPanel,
}

pub(crate) struct PrLookupLoadResult {
    request_id: u64,
    sha: String,
    result: Result<CommitPrResolution, CommitPrLookupError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLookupFailure {
    NotFound,
    Lookup(CommitPrLookupError),
}

#[derive(Debug)]
pub enum PrLookupState {
    Idle,
    Loading {
        request_id: u64,
        sha: String,
        cancel: CancellationToken,
    },
    Selecting {
        sha: String,
        pulls: Vec<CommitPullRequest>,
        selected: usize,
    },
    Failed {
        sha: String,
        failure: PrLookupFailure,
    },
}

/// Outcome of one background file load.
#[derive(Debug)]
pub(crate) enum FileLoad {
    /// Ready to display — including the "unviewable" notices (binary, oversized).
    Ready(Box<OpenFile>),
    /// A newer request superseded this one; nothing is delivered.
    Superseded,
    /// The file could not be read.
    Failed(String),
}

/// Deliver one finished file load, dropping a result whose request was
/// superseded while the load ran.
///
/// `load_file` polls cancellation only between stages, so a request cancelled
/// during the final stage still yields `FileLoad::Ready`. This is the last gate
/// that keeps that stale file out of the channel.
fn deliver_file_load(
    load: FileLoad,
    path: String,
    cancel: &dyn CancelSignal,
    tx: &mpsc::Sender<FileLoadResult>,
) {
    match load {
        FileLoad::Ready(open) => {
            if !cancel.is_cancelled() {
                let _ = tx.blocking_send(FileLoadResult {
                    path,
                    result: Ok(*open),
                });
            }
        }
        FileLoad::Superseded => {}
        FileLoad::Failed(message) => {
            let _ = tx.blocking_send(FileLoadResult {
                path,
                result: Err(message),
            });
        }
    }
}

fn deliver_blame_load(
    result: Result<BlameFile, BlameError>,
    path: String,
    cancel: &dyn CancelSignal,
    tx: &mpsc::Sender<BlameLoadResult>,
) {
    if !cancel.is_cancelled() {
        let _ = tx.blocking_send(BlameLoadResult { path, result });
    }
}

fn deliver_commit_diff_load(
    delivery: CommitDiffLoadResult,
    cancel: &dyn CancelSignal,
    tx: &mpsc::Sender<CommitDiffLoadResult>,
) {
    if !cancel.is_cancelled() {
        let _ = tx.try_send(delivery);
    }
}

/// Walk the repository for a browse session that is still current.
///
/// `spawn_blocking` cannot be interrupted mid-call, so a session cancelled
/// between the spawn and the thread picking the task up would otherwise run a
/// full `git ls-files` and hand its result to a browser the user already left.
/// The entry gate skips the walk; the delivery gate drops a walk that finished
/// after the cancellation.
///
/// The walk is a parameter rather than a fixed call so both gates are separately
/// observable: a test can see whether the closure ran at all, and can cancel
/// from inside it to reach the delivery gate with the walk already done.
fn deliver_repository_files<F>(
    cancel: &dyn CancelSignal,
    tx: &mpsc::Sender<Result<RepositoryFiles, String>>,
    walk: F,
) where
    F: FnOnce() -> Result<RepositoryFiles, String>,
{
    let Some(result) = stage(cancel, walk) else {
        return;
    };
    if !cancel.is_cancelled() {
        let _ = tx.blocking_send(result);
    }
}

/// Build the syntax-highlighted cache for a browse session that is still current.
///
/// Same two gates, same reason, as [`deliver_repository_files`] — highlighting a
/// whole file is the most expensive thing the browser starts in the background,
/// so a queued task for an abandoned session is the one most worth skipping.
fn deliver_highlighted_cache<F>(
    cancel: &dyn CancelSignal,
    tx: &mpsc::Sender<(String, DiffCache)>,
    build: F,
) where
    F: FnOnce() -> (String, DiffCache),
{
    let Some(highlighted) = stage(cancel, build) else {
        return;
    };
    if !cancel.is_cancelled() {
        let _ = tx.blocking_send(highlighted);
    }
}

/// What a finished index build hands back to the UI thread.
///
/// `IndexBuild::Cancelled` is deliberately absent: a cancelled build sends
/// nothing at all, so it cannot be observed here.
pub(crate) enum IndexDelivery {
    Ready(Box<CodeIndex>),
    Failed(String),
}

/// Everything the Repository Browser needs. `None` on [`App`] means inactive.
pub struct BrowseState {
    pub repo_root: PathBuf,
    pub(crate) cancel_token: CancellationToken,
    pub paths: LoadState<Vec<String>>,
    pub tree: crate::app::file_tree::FileTreeState,
    pub filter: Option<ListFilter>,
    pub open: Option<OpenFile>,
    pub open_load: OpenLoad,
    pub(crate) blame: BlameState,
    pub(crate) commit_diff: BrowseCommitDiffState,
    commit_diff_generation: u64,
    pub(crate) pr_lookup: PrLookupState,
    pr_lookup_generation: u64,
    pub(crate) line_discussion: LineDiscussionState,
    line_discussion_generation: u64,
    /// 0-based cursor line within the open file.
    pub cursor_line: usize,
    pub scroll_offset: usize,
    pub index: IndexState,
    pub module_graph: ModuleGraphState,
    pub module_graph_pane: ModuleGraphPaneState,
    module_graph_query_generation: u64,
    module_graph_query_cancel: Option<CancellationToken>,
    pub source_universe: SourceUniverse,
    pub overlay: BrowseOverlay,
    pub jump_stack: Vec<BrowseJump>,
    /// Transient message shown in the footer (unreadable file, no definition, …).
    pub status: Option<String>,
    /// Repository-level message from the listing: truncation, non-UTF-8 skips,
    /// an empty repository.
    ///
    /// Kept apart from `status` because it stays true for the whole session
    /// while `status` is overwritten by every transient message. A filter that
    /// matches nothing used to overwrite it and then clear the footer outright,
    /// losing a truncation warning permanently.
    pub(crate) listing_status: Option<String>,
    pub return_state: AppState,
    pub(crate) paths_receiver: Option<mpsc::Receiver<Result<RepositoryFiles, String>>>,
    pub(crate) index_receiver: Option<mpsc::Receiver<IndexDelivery>>,
    pub(crate) module_graph_query_receiver: Option<mpsc::Receiver<ModuleGraphPanelDelivery>>,
    pub(crate) file_receiver: Option<mpsc::Receiver<FileLoadResult>>,
    pub(crate) blame_receiver: Option<mpsc::Receiver<BlameLoadResult>>,
    pub(crate) commit_diff_receiver: Option<mpsc::Receiver<CommitDiffLoadResult>>,
    pub(crate) pr_lookup_receiver: Option<mpsc::Receiver<PrLookupLoadResult>>,
    pub(crate) line_discussion_receiver: Option<mpsc::Receiver<LineDiscussionDelivery>>,
    pub(crate) highlight_receiver: Option<mpsc::Receiver<(String, DiffCache)>>,
}

impl BrowseState {
    /// True while the open file's content is still being read in the background.
    ///
    /// Anything that answers a question *about the open file* must consult this
    /// first. `open` holds a placeholder for the duration of the load, so a
    /// consumer that inspects only `open` sees an empty file and reports it as
    /// one — which is how `o` came to answer "No symbols in this file" about a
    /// file that had not been read yet.
    pub fn open_is_pending(&self) -> bool {
        matches!(self.open_load, OpenLoad::Pending { .. })
    }

    pub fn new(repo_root: PathBuf, return_state: AppState) -> Self {
        Self {
            repo_root,
            cancel_token: CancellationToken::new(),
            paths: LoadState::Loading,
            tree: crate::app::file_tree::FileTreeState::new(),
            filter: None,
            open: None,
            open_load: OpenLoad::Idle,
            blame: BlameState::Off,
            commit_diff: BrowseCommitDiffState::Off,
            commit_diff_generation: 0,
            pr_lookup: PrLookupState::Idle,
            pr_lookup_generation: 0,
            line_discussion: LineDiscussionState::Idle,
            line_discussion_generation: 0,
            cursor_line: 0,
            scroll_offset: 0,
            index: IndexState::Idle,
            module_graph: ModuleGraphState::Idle,
            module_graph_pane: ModuleGraphPaneState::Closed,
            module_graph_query_generation: 0,
            module_graph_query_cancel: None,
            source_universe: SourceUniverse::Partial,
            overlay: BrowseOverlay::None,
            jump_stack: Vec::new(),
            status: None,
            listing_status: None,
            return_state,
            paths_receiver: None,
            index_receiver: None,
            module_graph_query_receiver: None,
            file_receiver: None,
            blame_receiver: None,
            commit_diff_receiver: None,
            pr_lookup_receiver: None,
            line_discussion_receiver: None,
            highlight_receiver: None,
        }
    }

    pub(crate) fn start_module_graph_query(&mut self, path: String, graph: Arc<ModuleGraph>) {
        self.cancel_module_graph_query();
        self.module_graph_query_generation = self.module_graph_query_generation.wrapping_add(1);
        let request_id = self.module_graph_query_generation;
        let cancel = self.cancel_token.child_token();
        let task_cancel = cancel.clone();
        let task_path = path.clone();
        let (tx, rx) = mpsc::channel(1);
        self.module_graph_query_cancel = Some(cancel);
        self.module_graph_query_receiver = Some(rx);
        self.module_graph_pane = ModuleGraphPaneState::Loading { request_id, path };

        tokio::task::spawn_blocking(move || {
            let Some(panel) = build_module_graph_panel(&graph, &task_path, &task_cancel) else {
                return;
            };
            if !task_cancel.is_cancelled() {
                let _ = tx.blocking_send(ModuleGraphPanelDelivery {
                    request_id,
                    path: task_path,
                    panel,
                });
            }
        });
    }

    pub(crate) fn start_module_graph_query_for_path(
        &mut self,
        path: String,
    ) -> Result<(), &'static str> {
        let graph = match &self.module_graph {
            ModuleGraphState::Idle | ModuleGraphState::Building => {
                return Err("Module graph is still building");
            }
            ModuleGraphState::Failed => return Err("Module graph is unavailable"),
            ModuleGraphState::Ready(graph) => Arc::clone(graph),
        };
        if !graph.is_analyzed(&path) {
            return Err(if crate::module_graph::supports_imports(&path) {
                "Import analysis is unavailable for this file"
            } else {
                "Import analysis is not supported for this file"
            });
        }
        self.start_module_graph_query(path, graph);
        Ok(())
    }

    pub(crate) fn cancel_module_graph_query(&mut self) {
        if let Some(cancel) = self.module_graph_query_cancel.take() {
            cancel.cancel();
        }
        self.module_graph_query_receiver = None;
    }

    pub(crate) fn close_module_graph_pane(&mut self) {
        self.cancel_module_graph_query();
        self.module_graph_pane = ModuleGraphPaneState::Closed;
    }

    #[cfg(test)]
    pub(crate) fn module_graph_query_token(&self) -> Option<CancellationToken> {
        self.module_graph_query_cancel.clone()
    }

    fn cancel_blame_request(&mut self) {
        if let BlameState::Loading { ref cancel, .. } = self.blame {
            cancel.cancel();
        }
        self.blame_receiver = None;
    }

    fn cancel_commit_diff_request(&mut self) {
        if let BrowseCommitDiffState::Loading { ref cancel, .. } = self.commit_diff {
            cancel.cancel();
        }
        self.commit_diff_receiver = None;
    }

    fn cancel_pr_lookup_request(&mut self) {
        if let PrLookupState::Loading { ref cancel, .. } = self.pr_lookup {
            cancel.cancel();
        }
        self.pr_lookup_receiver = None;
    }

    fn begin_pr_lookup(&mut self, sha: &str) -> (u64, CancellationToken) {
        self.cancel_pr_lookup_request();
        self.pr_lookup_generation = self.pr_lookup_generation.wrapping_add(1);
        let request_id = self.pr_lookup_generation;
        let cancel = self.cancel_token.child_token();
        self.pr_lookup = PrLookupState::Loading {
            request_id,
            sha: sha.to_string(),
            cancel: cancel.clone(),
        };
        self.status = None;
        (request_id, cancel)
    }

    fn cancel_line_discussion_request(&mut self) {
        match &self.line_discussion {
            LineDiscussionState::ResolvingPullRequests { cancel, .. }
            | LineDiscussionState::LoadingComments { cancel, .. } => cancel.cancel(),
            LineDiscussionState::Idle
            | LineDiscussionState::Ready { .. }
            | LineDiscussionState::Failed { .. } => {}
        }
        self.line_discussion_receiver = None;
    }

    fn begin_line_discussion_resolution(&mut self, path: String) -> (u64, CancellationToken) {
        self.cancel_line_discussion_request();
        self.line_discussion_generation = self.line_discussion_generation.wrapping_add(1);
        let request_id = self.line_discussion_generation;
        let cancel = self.cancel_token.child_token();
        self.line_discussion = LineDiscussionState::ResolvingPullRequests {
            request_id,
            path,
            cancel: cancel.clone(),
        };
        self.status = None;
        (request_id, cancel)
    }

    fn begin_line_discussion_load(
        &mut self,
        path: String,
        pr_numbers: Vec<u32>,
    ) -> (u64, CancellationToken) {
        self.cancel_line_discussion_request();
        self.line_discussion_generation = self.line_discussion_generation.wrapping_add(1);
        let request_id = self.line_discussion_generation;
        let cancel = self.cancel_token.child_token();
        self.line_discussion = LineDiscussionState::LoadingComments {
            request_id,
            path,
            pr_numbers,
            cancel: cancel.clone(),
        };
        self.status = None;
        (request_id, cancel)
    }

    fn line_discussion_request_matches_context(&self) -> bool {
        let requested_path = match &self.line_discussion {
            LineDiscussionState::ResolvingPullRequests { path, .. }
            | LineDiscussionState::LoadingComments { path, .. } => path,
            _ => return true,
        };
        self.open
            .as_ref()
            .is_some_and(|open| &open.path == requested_path)
    }

    fn pr_lookup_matches_current_context(&self) -> bool {
        let PrLookupState::Loading { sha, .. } = &self.pr_lookup else {
            return false;
        };
        if !matches!(self.commit_diff, BrowseCommitDiffState::Off) {
            return false;
        }
        let Some(open) = self.open.as_ref() else {
            return false;
        };
        let BlameState::Ready { path, gutter } = &self.blame else {
            return false;
        };
        path == &open.path
            && gutter
                .annotation_at(self.cursor_line)
                .is_some_and(|annotation| annotation.sha() == sha)
    }

    /// Install a freshly listed set of tracked paths and rebuild the tree.
    pub fn set_paths(&mut self, paths: Vec<String>) {
        self.paths = LoadState::Loaded(paths);
        self.rebuild_tree();
    }

    /// All paths currently listed, before filtering.
    pub fn all_paths(&self) -> &[String] {
        match self.paths {
            LoadState::Loaded(ref paths) => paths,
            _ => &[],
        }
    }

    /// Rebuild the tree from the paths that survive the active filter.
    pub fn rebuild_tree(&mut self) {
        let visible: Vec<(usize, String)> = match self.filter {
            Some(ref filter) => filter
                .matched_indices
                .iter()
                .filter_map(|index| {
                    self.all_paths()
                        .get(*index)
                        .map(|path| (*index, path.clone()))
                })
                .collect(),
            None => self
                .all_paths()
                .iter()
                .enumerate()
                .map(|(index, path)| (index, path.clone()))
                .collect(),
        };
        self.tree.rebuild_owned(visible);
    }

    /// Re-apply the filter query to the path list.
    pub fn apply_filter(&mut self) {
        let paths: Vec<String> = self.all_paths().to_vec();
        if let Some(filter) = self.filter.as_mut() {
            filter.apply(&paths, |path: &String, query| {
                path.to_lowercase().contains(query)
            });
            filter.sync_selection();
        }
        self.rebuild_tree();

        // An empty repository matches nothing whatever the query is, so naming
        // the query would send the user to narrow a filter that is not the
        // cause.
        let has_paths = !self.all_paths().is_empty();
        let empty_filter = self.filter.as_ref().and_then(|filter| {
            (has_paths && !filter.query.is_empty() && filter.matched_indices.is_empty())
                .then(|| format!("No files match filter \"{}\".", filter.query))
        });
        if let Some(message) = empty_filter {
            self.status = Some(message);
        } else if self
            .status
            .as_deref()
            .is_some_and(|message| message.starts_with("No files match filter \""))
        {
            // Restore rather than clear: the listing-level message the filter
            // message displaced is still true.
            self.status = self.listing_status.clone();
        }
    }

    /// Repository-relative path under the tree cursor, if it is a file row.
    pub fn selected_path(&self) -> Option<&str> {
        let index = self.tree.selected_file_index()?;
        self.all_paths().get(index).map(String::as_str)
    }

    /// Move the tree cursor onto the open file so the panes agree.
    pub fn sync_tree_to_open_file(&mut self) {
        let Some(ref open) = self.open else {
            return;
        };
        let Some(index) = self.all_paths().iter().position(|path| *path == open.path) else {
            return;
        };
        if let Some(row) = self.tree.find_row_for_file(index) {
            self.tree.selected_row = row;
        }
    }

    /// Place the cursor on a 0-based line, clamped to the file. The next
    /// render's `clamp_scroll` centres the viewport on it.
    pub fn focus_line(&mut self, line: usize) {
        let last = self
            .open
            .as_ref()
            .map(|open| open.line_count().saturating_sub(1))
            .unwrap_or(0);
        self.cursor_line = line.min(last);
    }

    /// Scroll so the cursor rides the centre of a viewport `height` rows
    /// tall — the diff view's margin behaviour. The diff view clamps the
    /// overscroll at render time; here the offset is consumed directly by
    /// `content_window`, so the end-of-file clamp lives in the state.
    pub fn clamp_scroll(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        let line_count = self
            .open
            .as_ref()
            .map(|open| open.line_count())
            .unwrap_or(0);
        self.scroll_offset = crate::diff_store::margin_scroll_offset(
            self.cursor_line,
            self.scroll_offset,
            line_count,
            height,
        )
        .min(line_count.saturating_sub(height));
    }

    /// Move the cursor by `delta` lines, clamped to the file.
    pub fn move_cursor(&mut self, delta: isize) {
        let last = self
            .open
            .as_ref()
            .map(|open| open.line_count().saturating_sub(1))
            .unwrap_or(0);
        let next = self.cursor_line as isize + delta;
        self.cursor_line = next.clamp(0, last as isize) as usize;
    }

    /// Attach symbols for the open file, preferring the index when it is ready.
    pub fn refresh_open_file_symbols(&mut self) {
        let indexed = self.index.ready().and_then(|index| {
            self.open
                .as_ref()
                .and_then(|open| index.file_symbols(&open.path))
                .map(<[Symbol]>::to_vec)
        });
        if let (Some(open), Some(symbols)) = (self.open.as_mut(), indexed) {
            open.symbols = symbols;
        }
    }

    /// Swap in the fully highlighted cache once the background task delivers it.
    pub fn apply_highlighted_cache(&mut self, path: &str, cache: DiffCache) {
        if let Some(open) = self.open.as_mut() {
            if open.path == path {
                open.cache = cache;
            }
        }
    }

    /// Mirrors `apply_highlighted_cache`: the path check is a plain runtime
    /// `if`, not a `debug_assert!`, so release builds cannot install blame for
    /// a different open file.
    pub(crate) fn apply_blame_result(&mut self, path: &str, blame: BlameFile) -> bool {
        if let Some(open) = self.open.as_ref() {
            if open.path == path {
                let buffer_lines = open.line_count();
                self.blame = BlameState::Ready {
                    path: path.to_string(),
                    gutter: BlameGutter::from_file(blame, buffer_lines),
                };
                self.cancel_line_discussion_request();
                self.line_discussion = LineDiscussionState::Idle;
                return true;
            }
        }
        false
    }

    /// Symbol rows for the outline overlay.
    pub fn outline_symbols(&self) -> &[Symbol] {
        self.open
            .as_ref()
            .map(|open| open.symbols.as_slice())
            .unwrap_or(&[])
    }

    /// The one symbol-search result set: what the overlay draws, what the
    /// selection is clamped against, and what Enter indexes into.
    ///
    /// Every caller goes through here so the cap cannot drift between them.
    /// Two independent `search` calls agreeing only because they happened to
    /// pass the same limit meant the highlighted row and the row Enter opened
    /// were the same symbol by coincidence, not by construction.
    pub fn symbol_search_hits(&self, query: &str) -> Vec<SymbolRef<'_>> {
        self.index
            .ready()
            .map(|index| index.search(query, MAX_SYMBOL_SEARCH_RESULTS))
            .unwrap_or_default()
    }

    /// Current symbol-search results as `(path, line, label)` rows.
    pub fn symbol_search_results(&self, query: &str) -> Vec<(String, usize, String)> {
        self.symbol_search_hits(query)
            .into_iter()
            .map(|hit| (hit.path.to_string(), hit.symbol.line, hit.search_label()))
            .collect()
    }
}

fn build_module_graph_panel(
    graph: &ModuleGraph,
    path: &str,
    cancel: &dyn CancelSignal,
) -> Option<ModuleGraphPanel> {
    let (dependencies, dependencies_total) =
        graph.dependencies_bounded(path, MAX_MODULE_GRAPH_RESULTS)?;
    if cancel.is_cancelled() {
        return None;
    }
    let dependencies = module_graph_rows(
        graph,
        dependencies,
        dependencies_total,
        ModuleGraphDirection::Dependencies,
        cancel,
    )?;
    let (dependents, dependents_total) =
        graph.dependents_bounded(path, MAX_MODULE_GRAPH_RESULTS)?;
    if cancel.is_cancelled() {
        return None;
    }
    let dependents = module_graph_rows(
        graph,
        dependents,
        dependents_total,
        ModuleGraphDirection::Dependents,
        cancel,
    )?;
    Some(ModuleGraphPanel {
        path: path.to_string(),
        display_path: bounded_module_graph_text(path),
        direction: ModuleGraphDirection::Dependencies,
        selected: 0,
        dependencies,
        dependents,
    })
}

fn module_graph_rows(
    graph: &ModuleGraph,
    result: DependencyResult,
    total: usize,
    direction: ModuleGraphDirection,
    cancel: &dyn CancelSignal,
) -> Option<ModuleGraphRows> {
    let guarantee = result.guarantee;
    let mut rows = Vec::with_capacity(result.edges.len());
    for (index, edge) in result.edges.into_iter().enumerate() {
        if index.is_multiple_of(64) && cancel.is_cancelled() {
            return None;
        }
        let row = match direction {
            ModuleGraphDirection::Dependencies => {
                let specifier = bounded_module_graph_text(&edge.specifier);
                let (target, jump) = match edge.target {
                    DependencyTarget::Path(path) => {
                        let jump = graph.is_listed(&path).then(|| ModuleGraphJump {
                            path: path.clone(),
                            line: 0,
                        });
                        (bounded_module_graph_text(&path), jump)
                    }
                    DependencyTarget::External(package) => (
                        format!("package {}", bounded_module_graph_text(&package)),
                        None,
                    ),
                    DependencyTarget::Unresolved(reason) => (
                        format!("unresolved ({})", bounded_module_graph_text(&reason)),
                        None,
                    ),
                };
                ModuleGraphRow {
                    node: bounded_module_graph_text(&target),
                    edge: bounded_module_graph_text(&format!(
                        "[{}] {}  :{}",
                        edge.kind.label(),
                        specifier,
                        edge.line
                    )),
                    jump,
                }
            }
            ModuleGraphDirection::Dependents => {
                let from = bounded_module_graph_text(&edge.from);
                let specifier = bounded_module_graph_text(&edge.specifier);
                ModuleGraphRow {
                    node: bounded_module_graph_text(&from),
                    edge: bounded_module_graph_text(&format!(
                        "[{}] {}  :{}",
                        edge.kind.label(),
                        specifier,
                        edge.line
                    )),
                    jump: graph.is_listed(&edge.from).then(|| ModuleGraphJump {
                        path: edge.from,
                        line: edge.line.saturating_sub(1),
                    }),
                }
            }
        };
        rows.push(row);
    }
    Some(ModuleGraphRows {
        rows,
        total,
        guarantee,
    })
}

pub(crate) fn bounded_module_graph_text(text: &str) -> String {
    let end = text
        .char_indices()
        .nth(MAX_MODULE_GRAPH_COMPONENT_CHARS)
        .map_or(text.len(), |(index, _)| index);
    let character_truncated = end < text.len();
    if character_truncated {
        let mut marked = String::with_capacity(end + '…'.len_utf8());
        marked.push_str(&text[..end]);
        if !marked.ends_with('…') {
            marked.push('…');
        }
        truncate_with_width(&marked, MAX_MODULE_GRAPH_LABEL_WIDTH).into_owned()
    } else {
        truncate_with_width(&text[..end], MAX_MODULE_GRAPH_LABEL_WIDTH).into_owned()
    }
}

/// A bounded repository file listing plus its full de-duplicated size.
#[derive(Debug, PartialEq, Eq)]
pub struct RepositoryFiles {
    pub paths: Vec<String>,
    /// Number of representable paths before [`MAX_BROWSE_FILES`] truncation.
    pub total: usize,
    /// Invalid UTF-8 paths omitted because the browser stores openable paths as `String`.
    pub skipped_non_utf8: usize,
}

impl RepositoryFiles {
    pub fn source_universe(&self) -> SourceUniverse {
        if self.total == self.paths.len() && self.skipped_non_utf8 == 0 {
            SourceUniverse::Complete
        } else {
            SourceUniverse::Partial
        }
    }
}

/// List the repository's files: tracked plus untracked-but-not-ignored.
///
/// `git ls-files` is used rather than a filesystem walk so `.gitignore`,
/// submodules and sparse checkouts are all honoured for free — the same
/// contract every other octorus screen already relies on.
///
/// `--others` matters more than it looks: a file an agent wrote thirty seconds
/// ago is not committed yet, and a repository viewer that cannot show it is
/// blind exactly when the user most needs to read.
pub fn list_repository_files(repo_root: &std::path::Path) -> Result<RepositoryFiles, String> {
    let output = std::process::Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("failed to run git ls-files: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            "not a git repository".to_string()
        } else {
            stderr.to_string()
        });
    }

    Ok(parse_ls_files(&output.stdout))
}

/// Parse NUL-delimited `git ls-files -z` bytes into openable UTF-8 paths.
///
/// Lossy decoding is forbidden here: replacement characters would create a
/// tree entry that no longer names the file emitted by Git. Invalid paths are
/// excluded and counted so [`repository_listing_status`] can tell the user.
pub fn parse_ls_files(stdout: &[u8]) -> RepositoryFiles {
    let mut raw_paths: Vec<&[u8]> = stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    raw_paths.sort_unstable();
    raw_paths.dedup();

    let mut skipped_non_utf8 = 0;
    let mut paths: Vec<String> = raw_paths
        .into_iter()
        .filter_map(|path| match std::str::from_utf8(path) {
            Ok(path) => Some(path.to_owned()),
            Err(_) => {
                skipped_non_utf8 += 1;
                None
            }
        })
        .collect();
    let total = paths.len();
    paths.truncate(MAX_BROWSE_FILES);
    RepositoryFiles {
        paths,
        total,
        skipped_non_utf8,
    }
}

/// Read a file and prepare it for display, stopping between expensive stages
/// when a newer request supersedes this one.
fn load_file(
    absolute: &std::path::Path,
    path: &str,
    tab_width: u8,
    cancel: &dyn CancelSignal,
) -> FileLoad {
    let Some(metadata) = stage(cancel, || file_metadata(absolute, path)) else {
        return FileLoad::Superseded;
    };
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(message) => return FileLoad::Failed(message),
    };
    load_file_contents(absolute, path, tab_width, metadata.len(), cancel)
}

fn file_metadata(absolute: &std::path::Path, path: &str) -> Result<std::fs::Metadata, String> {
    let metadata = std::fs::metadata(absolute).map_err(|e| format!("{path}: {e}"))?;
    if !metadata.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    Ok(metadata)
}

fn load_file_contents(
    absolute: &std::path::Path,
    path: &str,
    tab_width: u8,
    file_size: u64,
    cancel: &dyn CancelSignal,
) -> FileLoad {
    if file_size > MAX_VIEWABLE_FILE_BYTES {
        return FileLoad::Ready(Box::new(unviewable(
            path,
            format!(
                "File is too large to display: {file_size} bytes exceeds the {} limit.",
                human_bytes(MAX_VIEWABLE_FILE_BYTES)
            ),
            tab_width,
        )));
    }

    let Some(bytes) = stage(cancel, || std::fs::read(absolute)) else {
        return FileLoad::Superseded;
    };
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => return FileLoad::Failed(format!("{path}: {error}")),
    };
    let Some(source) = stage(cancel, || String::from_utf8(bytes)) else {
        return FileLoad::Superseded;
    };
    let Ok(source) = source else {
        return FileLoad::Ready(Box::new(unviewable(
            path,
            "Binary file — no text preview.".to_string(),
            tab_width,
        )));
    };

    let Some((line_count, longest_line)) = stage(cancel, || {
        source
            .lines()
            .fold((0_usize, 0_usize), |(count, longest), line| {
                (count + 1, longest.max(line.len()))
            })
    }) else {
        return FileLoad::Superseded;
    };
    if line_count > MAX_VIEWABLE_FILE_LINES {
        return FileLoad::Ready(Box::new(unviewable(
            path,
            format!(
                "File has {line_count} lines — too many to display ({} line limit).",
                MAX_VIEWABLE_FILE_LINES
            ),
            tab_width,
        )));
    }
    if longest_line > MAX_VIEWABLE_LINE_BYTES {
        return FileLoad::Ready(Box::new(unviewable(
            path,
            format!(
                "File contains a {longest_line}-byte line — too long to display ({}-byte line limit).",
                MAX_VIEWABLE_LINE_BYTES
            ),
            tab_width,
        )));
    }

    let Some((lines, patch)) = stage(cancel, || {
        (
            source.lines().map(str::to_string).collect::<Vec<_>>(),
            build_file_patch(&source),
        )
    }) else {
        return FileLoad::Superseded;
    };
    let Some(cache) = stage(cancel, || {
        crate::ui::diff_view::build_plain_diff_cache(&patch, tab_width)
    }) else {
        return FileLoad::Superseded;
    };

    FileLoad::Ready(Box::new(OpenFile {
        path: path.to_string(),
        patch,
        cache,
        lines,
        symbols: Vec::new(),
        viewable: true,
        notice: source.is_empty().then(|| "Empty file.".to_string()),
    }))
}

/// Run one load stage only while its request is current.
///
/// Centralising the cancellation branch makes the useful contract directly
/// testable: a cancelled stage must not invoke its closure at all. Callers put
/// each filesystem or O(file-size) operation behind this gate.
fn stage<T>(cancel: &dyn CancelSignal, work: impl FnOnce() -> T) -> Option<T> {
    if cancel.is_cancelled() {
        None
    } else {
        Some(work())
    }
}

fn install_open_file(state: &mut BrowseState, open: OpenFile, line: usize, scroll: Option<usize>) {
    state.status = None;
    state.open = Some(open);
    state.focus_line(line);
    if let Some(scroll) = scroll {
        state.scroll_offset = scroll;
    }
    state.sync_tree_to_open_file();
    state.refresh_open_file_symbols();
}

fn install_file_load_failure(
    state: &mut BrowseState,
    path: String,
    message: String,
    tab_width: u8,
) {
    if matches!(
        state.blame,
        BlameState::Waiting { path: ref blame_path }
            | BlameState::Loading {
                path: ref blame_path,
                ..
            } if blame_path == &path
    ) {
        state.cancel_blame_request();
        state.blame = BlameState::Failed;
    }
    state.open = Some(unviewable(&path, message.clone(), tab_width));
    state.status = Some(message.clone());
    state.open_load = OpenLoad::Failed { path, message };
}

/// Put a repository-listing failure somewhere it can actually be read.
///
/// The tree renderer has one unwrapped line in the narrow pane, and the content
/// renderer's `OpenFile::notice` branch is clipped the same way. A pseudo-file
/// with `notice: None` takes the cached-content branch, whose viewport scrolls
/// vertically; fixed-width wrapping also keeps long path/hint lines reachable
/// without horizontal scrolling.
fn install_repository_listing_failure(state: &mut BrowseState, message: String, tab_width: u8) {
    const WRAP_WIDTH: usize = 60;

    let wrapped = wrap_display_message(&message, WRAP_WIDTH);
    let source = format!("Git could not list repository files.\n\n{wrapped}");
    let patch = build_file_patch(&source);
    state.open = Some(OpenFile {
        path: "Repository listing error".to_string(),
        cache: crate::ui::diff_view::build_plain_diff_cache(&patch, tab_width),
        patch,
        lines: source.lines().map(str::to_string).collect(),
        symbols: Vec::new(),
        viewable: true,
        notice: None,
    });
    state.focus_line(0);
    state.scroll_offset = 0;
    state.status = Some("Repository listing failed; details are in the preview pane.".to_string());
    state.paths = LoadState::Error(message);
}

fn wrap_display_message(message: &str, max_width: usize) -> String {
    let mut wrapped = String::with_capacity(message.len());
    for (line_index, line) in message.split('\n').enumerate() {
        if line_index > 0 {
            wrapped.push('\n');
        }
        let mut width = 0;
        for character in line.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if width > 0 && width + character_width > max_width {
                wrapped.push('\n');
                width = 0;
            }
            wrapped.push(character);
            width += character_width;
        }
    }
    wrapped
}

fn repository_listing_status(listing: &RepositoryFiles) -> Option<String> {
    let mut messages = Vec::new();
    if listing.paths.is_empty() && listing.skipped_non_utf8 == 0 {
        messages.push("Repository contains no files.".to_string());
    }
    if listing.total > MAX_BROWSE_FILES {
        messages.push(format!(
            "Repository has {} files; showing the first {}. Use a narrower working directory or exclude generated files.",
            listing.total, MAX_BROWSE_FILES
        ));
    }
    if listing.skipped_non_utf8 > 0 {
        let (noun, verb) = if listing.skipped_non_utf8 == 1 {
            ("path", "is")
        } else {
            ("paths", "are")
        };
        messages.push(format!(
            "Skipped {} repository {noun} that {verb} not valid UTF-8 and cannot be represented safely in the browser.",
            listing.skipped_non_utf8,
        ));
    }
    (!messages.is_empty()).then(|| messages.join(" "))
}

fn unviewable(path: &str, notice: String, tab_width: u8) -> OpenFile {
    let patch = build_file_patch("");
    OpenFile {
        path: path.to_string(),
        cache: crate::ui::diff_view::build_plain_diff_cache(&patch, tab_width),
        patch,
        lines: Vec::new(),
        symbols: Vec::new(),
        viewable: false,
        notice: Some(notice),
    }
}

/// Render file content as an all-context pseudo-patch.
///
/// This is what lets the browser reuse the diff highlighter — and therefore
/// every language, theme and injection the diff view already supports —
/// without a second rendering path to keep in sync.
pub fn build_file_patch(source: &str) -> String {
    let source = source.replace("\r\n", "\n");
    let line_count = source.lines().count().max(1);
    let mut patch = String::with_capacity(source.len() + 32);
    patch.push_str(&format!("@@ -1,{line_count} +1,{line_count} @@\n"));
    for line in source.lines() {
        patch.push(' ');
        patch.push_str(line);
        patch.push('\n');
    }
    patch
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::browse_discussion::{DiscussionIndex, DiscussionOutcome};

    #[test]
    fn original_line_fits_the_existing_blame_gutter_row_padding() {
        assert_eq!(std::mem::size_of::<BlameGutterRow>(), 16);
    }

    #[test]
    fn discussion_origins_slice_is_built_once_for_repeated_keypresses() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 2\n\
             author Alice\n\
             summary first\n\
             filename src/a.rs\n\
             \tone\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2\n\
             \ttwo\n";
        let gutter = BlameGutter::from_file(parse_porcelain(PORCELAIN), 2);
        let first: Arc<[Option<LineOrigin>]> = gutter.origins();

        for _ in 0..8 {
            let next: Arc<[Option<LineOrigin>]> = gutter.origins();
            assert!(
                Arc::ptr_eq(&first, &next),
                "repeated discussion keypress rebuilt the origins slice"
            );
        }
    }

    use crate::github::{
        parse_porcelain, BlameError, CommitPrLookupError, CommitPrResolution, CommitPullRequest,
        CommitPullRequestState,
    };
    use crate::symbols::{FileSymbols, SymbolKind};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn render_buffer(app: &mut App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, app))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_at(app: &mut App, width: u16, height: u16) -> String {
        let buffer = render_buffer(app, width, height);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Run one Git fixture command, making the only legitimate skip observable.
    ///
    /// A missing binary may be skipped locally, but GitHub Actions is expected
    /// to provide Git. Any command that starts and fails is a broken fixture,
    /// never a skip.
    fn run_git_fixture(test_name: &str, directory: &std::path::Path, args: &[&str]) -> bool {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(directory)
            .output();

        match output {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                panic!(
                    "{test_name}: git {} failed with {}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && std::env::var_os("CI").is_none() =>
            {
                use std::io::Write;

                let notice = format!("SKIPPED {test_name}: git binary is unavailable ({error})\n");
                std::io::stderr()
                    .lock()
                    .write_all(notice.as_bytes())
                    .expect("write visible test skip");
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                panic!("{test_name}: git is required when CI is set ({error})");
            }
            Err(error) => {
                panic!("{test_name}: failed to run git {}: {error}", args.join(" "));
            }
        }
    }

    fn state_with_paths(paths: &[&str]) -> BrowseState {
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        state.set_paths(paths.iter().map(|p| p.to_string()).collect());
        state
    }

    fn load_file_for_test(
        absolute: &std::path::Path,
        path: &str,
        tab_width: u8,
    ) -> Result<OpenFile, String> {
        match load_file(absolute, path, tab_width, &CancellationToken::new()) {
            FileLoad::Ready(open) => Ok(*open),
            FileLoad::Failed(message) => Err(message),
            FileLoad::Superseded => panic!("a fresh test token cannot be cancelled"),
        }
    }

    /// Drive the browse background channels until the pending load lands.
    async fn settle_browse(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state.as_ref().map(|state| &state.open_load),
                None | Some(OpenLoad::Idle | OpenLoad::Failed { .. })
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("browse load never settled");
    }

    /// Drive the index channel to one of its observable terminal states.
    async fn settle_index(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state.as_ref().map(|state| &state.index),
                Some(IndexState::Ready(_) | IndexState::Failed { .. })
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("symbol index never settled");
    }

    async fn settle_blame(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state.as_ref().map(|state| &state.blame),
                Some(BlameState::Ready { .. } | BlameState::Failed)
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("blame load never settled");
    }

    async fn settle_line_discussion(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state
                    .as_ref()
                    .map(|state| &state.line_discussion),
                Some(LineDiscussionState::Ready { .. } | LineDiscussionState::Failed { .. })
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("line discussion load never settled");
    }

    fn state_with_pending_load(path: &str) -> BrowseState {
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        state.open = Some(unviewable(path, "Loading…".to_string(), 4));
        state.open_load = OpenLoad::Pending {
            path: path.to_string(),
            line: 0,
            scroll: None,
            cancel: state.cancel_token.child_token(),
        };
        state
    }

    // ===== git ls-files parsing =====

    #[test]
    fn test_parse_ls_files_sorts_and_dedups() {
        let out = b"src/b.rs\0src/a.rs\0src/a.rs\0README.md\0";
        assert_eq!(
            parse_ls_files(out).paths,
            vec!["README.md", "src/a.rs", "src/b.rs"]
        );
    }

    #[test]
    fn test_parse_ls_files_empty_repository() {
        assert!(parse_ls_files(b"").paths.is_empty());
        assert!(parse_ls_files(b"\0\0").paths.is_empty());
    }

    #[test]
    fn test_parse_ls_files_handles_crlf_output() {
        // `-z` makes CRLF translation moot: a trailing `\r` is path data.
        assert_eq!(parse_ls_files(b"src/a.rs\r\0").paths, vec!["src/a.rs\r"]);
    }

    #[test]
    fn test_parse_ls_files_keeps_paths_with_spaces() {
        assert_eq!(
            parse_ls_files(b"docs/my notes.md\0").paths,
            vec!["docs/my notes.md"]
        );
    }

    #[test]
    fn test_parse_ls_files_keeps_embedded_newline() {
        assert_eq!(
            parse_ls_files(b"docs/line\nbreak.md\0src/lib.rs\0").paths,
            vec!["docs/line\nbreak.md", "src/lib.rs"]
        );
    }

    #[test]
    fn test_parse_ls_files_excludes_non_utf8_paths_and_counts_them() {
        // APFS rejects non-UTF-8 filenames, so the platform-independent parser
        // boundary is the only reliable place to reproduce Git's byte output.
        let listing =
            parse_ls_files(b"src/valid.rs\0src/\xff.rs\0src/also-valid.rs\0src/\xff.rs\0");

        assert_eq!(
            listing.paths,
            vec!["src/also-valid.rs", "src/valid.rs"],
            "a lossy replacement path must never become an unopenable tree row"
        );
        assert_eq!(listing.total, 2);
        assert_eq!(
            listing.skipped_non_utf8, 1,
            "duplicate invalid paths are counted once, like valid paths"
        );
    }

    // ===== pseudo-patch conversion =====

    #[test]
    fn test_build_file_patch() {
        assert_eq!(build_file_patch("a\nb"), "@@ -1,2 +1,2 @@\n a\n b\n");
    }

    #[test]
    fn test_build_file_patch_empty_file() {
        assert_eq!(build_file_patch(""), "@@ -1,1 +1,1 @@\n");
    }

    #[test]
    fn test_build_file_patch_preserves_diff_looking_lines() {
        // A source line starting with '-' must not be read back as a deletion.
        assert_eq!(
            build_file_patch("-not a deletion\n+not an addition"),
            "@@ -1,2 +1,2 @@\n -not a deletion\n +not an addition\n"
        );
    }

    #[test]
    fn test_build_file_patch_normalises_crlf() {
        assert_eq!(build_file_patch("a\r\nb\r\n"), "@@ -1,2 +1,2 @@\n a\n b\n");
    }

    // ===== tree + filter =====

    #[test]
    fn test_tree_built_from_paths() {
        let state = state_with_paths(&["src/app.rs", "src/ui/mod.rs", "README.md"]);
        insta::assert_snapshot!(state.tree.dump_tree(), @r"
        ▼ src/
          ▼ ui/
            mod.rs
          app.rs
        README.md
        ");
    }

    #[test]
    fn test_empty_repository_yields_empty_tree() {
        let state = state_with_paths(&[]);
        assert_eq!(state.tree.row_count(), 0);
        assert!(state.selected_path().is_none());
    }

    #[test]
    fn test_filter_narrows_tree_and_keeps_source_indices() {
        let mut state = state_with_paths(&["src/app.rs", "src/ui.rs", "README.md"]);
        let mut filter = ListFilter::new();
        filter.query = "ui".to_string();
        state.filter = Some(filter);
        state.apply_filter();

        assert_eq!(state.tree.dump_tree(), "▼ src/\n  ui.rs");
        // Selecting the only match must resolve to the original path, not to
        // index 0 of the filtered list.
        state.tree.selected_row = 1;
        assert_eq!(state.selected_path(), Some("src/ui.rs"));
    }

    #[test]
    fn test_filter_with_no_matches_leaves_nothing_selectable() {
        let mut state = state_with_paths(&["src/app.rs"]);
        let mut filter = ListFilter::new();
        filter.query = "zzz".to_string();
        state.filter = Some(filter);
        state.apply_filter();

        assert_eq!(state.tree.row_count(), 0);
        assert!(state.selected_path().is_none());
    }

    #[test]
    fn test_module_graph_labels_are_bounded_by_unicode_display_width() {
        let bounded = bounded_module_graph_text(&"依存先".repeat(1_000));

        assert!(
            unicode_width::UnicodeWidthStr::width(bounded.as_str()) <= MAX_MODULE_GRAPH_LABEL_WIDTH
        );
        assert!(bounded.ends_with('…'));
        assert!(bounded.chars().count() <= MAX_MODULE_GRAPH_COMPONENT_CHARS + 1);
    }

    #[test]
    fn test_module_graph_label_reserves_width_for_a_character_cap_ellipsis() {
        let text = format!("[import] {}{}tail", "a".repeat(231), "\u{301}".repeat(272));
        let bounded = bounded_module_graph_text(&text);
        let width = unicode_width::UnicodeWidthStr::width(bounded.as_str());

        assert!(width <= MAX_MODULE_GRAPH_LABEL_WIDTH);
        assert!(bounded.ends_with('…'));
        insta::assert_snapshot!(format!("width={width} suffix={}", bounded.ends_with('…')), @"width=240 suffix=true");
    }

    struct CancelDuringPanelRows {
        polls: std::sync::atomic::AtomicUsize,
    }

    impl CancelSignal for CancelDuringPanelRows {
        fn is_cancelled(&self) -> bool {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2
        }
    }

    #[test]
    fn test_high_fan_in_panel_retains_the_limit_and_preserves_the_total() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/center.ts"),
            "export const center = 1;\n",
        )
        .unwrap();
        let mut paths = vec!["src/center.ts".to_string()];
        for index in 0..250 {
            let path = format!("src/importer_{index:03}.ts");
            std::fs::write(
                dir.path().join(&path),
                format!("import './center';\nexport const value{index} = {index};\n"),
            )
            .unwrap();
            paths.push(path);
        }
        let CodeIndexBuild::Completed(index) = CodeIndex::build_cancellable(
            dir.path(),
            &paths,
            SourceUniverse::Complete,
            &CancellationToken::new(),
        ) else {
            panic!("high fan-in fixture must build");
        };

        let cancel_during_rows = CancelDuringPanelRows {
            polls: std::sync::atomic::AtomicUsize::new(0),
        };
        assert!(
            build_module_graph_panel(&index.modules, "src/center.ts", &cancel_during_rows)
                .is_none(),
            "row projection ignored cancellation"
        );
        assert_eq!(
            cancel_during_rows
                .polls
                .load(std::sync::atomic::Ordering::SeqCst),
            3,
            "cancellation must occur at the first dependent-row poll"
        );

        let mut panel =
            build_module_graph_panel(&index.modules, "src/center.ts", &CancellationToken::new())
                .expect("panel");

        assert_eq!(panel.dependents.total, 250);
        assert_eq!(panel.dependents.rows.len(), MAX_MODULE_GRAPH_RESULTS);
        assert!(panel.dependents.rows.iter().all(|row| {
            unicode_width::UnicodeWidthStr::width(row.node.as_str()) <= MAX_MODULE_GRAPH_LABEL_WIDTH
                && unicode_width::UnicodeWidthStr::width(row.edge.as_str())
                    <= MAX_MODULE_GRAPH_LABEL_WIDTH
        }));

        panel.set_direction(ModuleGraphDirection::Dependents);
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(paths);
        state.module_graph_pane = ModuleGraphPaneState::Ready(panel);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseGraph;
        let rendered = render_at(&mut app, 80, 12);
        assert!(rendered.contains("Importers 200/250 exact"), "{rendered}");
    }

    // ===== cursor and scrolling =====

    fn state_with_open_file(line_count: usize) -> BrowseState {
        let mut state = state_with_paths(&["src/a.rs"]);
        let source: String = (0..line_count)
            .map(|i| format!("line {i}\n"))
            .collect::<String>();
        let patch = build_file_patch(&source);
        state.open = Some(OpenFile {
            path: "src/a.rs".to_string(),
            cache: crate::ui::diff_view::build_plain_diff_cache(&patch, 4),
            patch,
            lines: source.lines().map(str::to_string).collect(),
            symbols: Vec::new(),
            viewable: true,
            notice: None,
        });
        state
    }

    fn state_with_ready_blame(porcelain: &str, line_count: usize) -> BrowseState {
        let mut state = state_with_open_file(line_count);
        state.blame = BlameState::Ready {
            path: "src/a.rs".to_string(),
            gutter: BlameGutter::from_file(parse_porcelain(porcelain), line_count),
        };
        state
    }

    #[test]
    fn test_move_cursor_clamps_to_file_bounds() {
        let mut state = state_with_open_file(5);
        state.move_cursor(100);
        assert_eq!(state.cursor_line, 4);
        state.move_cursor(-100);
        assert_eq!(state.cursor_line, 0);
    }

    #[test]
    fn test_move_cursor_without_open_file_stays_at_zero() {
        let mut state = state_with_paths(&["src/a.rs"]);
        state.move_cursor(10);
        assert_eq!(state.cursor_line, 0);
    }

    #[test]
    fn test_clamp_scroll_keeps_the_cursor_centred_like_the_diff_view() {
        let mut state = state_with_open_file(100);
        state.cursor_line = 40;
        state.scroll_offset = 0;
        state.clamp_scroll(10);
        // margin = 10 / 2: the cursor rides the centre of the viewport, not
        // its bottom edge (edge behaviour would leave the offset at 31).
        assert_eq!(state.scroll_offset, 36);

        state.cursor_line = 5;
        state.clamp_scroll(10);
        assert_eq!(state.scroll_offset, 1);
    }

    #[test]
    fn test_clamp_scroll_stops_at_the_end_so_the_cursor_walks_to_the_bottom() {
        let mut state = state_with_open_file(100);
        state.cursor_line = 99;
        state.scroll_offset = 80;
        state.clamp_scroll(10);
        // The margin maths alone would put the offset at 95; the end-of-file
        // clamp pins it to line_count - height so no blank rows render and the
        // cursor descends to the bottom row, matching the diff view's
        // render-side clamp.
        assert_eq!(state.scroll_offset, 90);
    }

    #[test]
    fn test_clamp_scroll_short_file_stays_pinned_to_the_top() {
        let mut state = state_with_open_file(5);
        state.cursor_line = 4;
        state.scroll_offset = 3;
        state.clamp_scroll(10);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_clamp_scroll_with_zero_height_is_a_noop() {
        let mut state = state_with_open_file(10);
        state.scroll_offset = 3;
        state.clamp_scroll(0);
        assert_eq!(state.scroll_offset, 3);
    }

    #[test]
    fn test_focus_line_clamps_the_cursor_and_the_render_clamp_centres_it() {
        let mut state = state_with_open_file(50);
        state.focus_line(30);
        assert_eq!(state.cursor_line, 30);
        state.clamp_scroll(10);
        assert_eq!(state.scroll_offset, 26);

        state.focus_line(999);
        assert_eq!(state.cursor_line, 49);
        state.clamp_scroll(10);
        // Jumping to the last line lands the cursor on the bottom row.
        assert_eq!(state.scroll_offset, 40);
    }

    #[test]
    fn test_focus_line_near_top_does_not_underflow() {
        let mut state = state_with_open_file(50);
        state.scroll_offset = 20;
        state.focus_line(2);
        state.clamp_scroll(10);
        assert_eq!(state.scroll_offset, 0);
    }

    // ===== symbols =====

    fn ready_index() -> IndexState {
        IndexState::Ready(Arc::new(SymbolIndex::from_files(vec![FileSymbols {
            path: "src/a.rs".to_string(),
            symbols: vec![Symbol {
                name: "alpha".to_string(),
                kind: SymbolKind::Function,
                line: 3,
                column: 3,
                depth: 0,
            }],
        }])))
    }

    #[test]
    fn test_refresh_open_file_symbols_from_index() {
        let mut state = state_with_open_file(10);
        assert!(state.outline_symbols().is_empty());

        state.index = ready_index();
        state.refresh_open_file_symbols();

        assert_eq!(state.outline_symbols().len(), 1);
        assert_eq!(state.outline_symbols()[0].name, "alpha");
    }

    #[test]
    fn test_symbol_search_results_before_index_is_ready() {
        let state = state_with_open_file(10);
        assert!(state.symbol_search_results("alpha").is_empty());
    }

    #[test]
    fn test_symbol_search_results_render_location() {
        let mut state = state_with_open_file(10);
        state.index = ready_index();
        assert_eq!(
            state.symbol_search_results("alpha"),
            vec![("src/a.rs".to_string(), 3, "ƒ alpha  src/a.rs:3".to_string())]
        );
    }

    #[test]
    fn test_index_state_labels() {
        assert_eq!(IndexState::Idle.label(), "symbols: -");
        assert_eq!(IndexState::Building.label(), "symbols: indexing");
        assert_eq!(ready_index().label(), "symbols: ready");
        assert_eq!(
            IndexState::Failed {
                reason: "parser crashed".to_string(),
            }
            .label(),
            "symbols: unavailable"
        );
        assert!(IndexState::Building.ready().is_none());
    }

    #[test]
    fn test_apply_blame_result_requires_the_current_open_path() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tline 0\n";
        let mut state = state_with_open_file(1);
        state.blame = BlameState::Loading {
            path: "src/a.rs".to_string(),
            cancel: state.cancel_token.child_token(),
        };

        assert!(!state.apply_blame_result("src/other.rs", parse_porcelain(PORCELAIN)));
        assert!(matches!(
            state.blame,
            BlameState::Loading { ref path, .. } if path == "src/a.rs"
        ));

        assert!(state.apply_blame_result("src/a.rs", parse_porcelain(PORCELAIN)));
        assert!(matches!(
            state.blame,
            BlameState::Ready {
                ref path,
                ref gutter
            } if path == "src/a.rs" && gutter.len() == 1
        ));
    }

    #[test]
    fn test_blame_gutter_rows_and_coverage_follow_the_open_buffer() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary first\n\
             \tone\n\
             bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2 2 1\n\
             author Bob\n\
             author-time 1700000001\n\
             summary second\n\
             \ttwo\n";

        let shorter = BlameGutter::from_file(parse_porcelain(PORCELAIN), 4);
        assert_eq!(shorter.len(), 4);
        assert!(matches!(
            shorter.coverage(),
            BlameCoverage::ShorterThanBuffer {
                blame_lines: 2,
                buffer_lines: 4
            }
        ));

        let longer = BlameGutter::from_file(parse_porcelain(PORCELAIN), 1);
        assert_eq!(longer.len(), 1);
        assert!(matches!(
            longer.coverage(),
            BlameCoverage::LongerThanBuffer {
                blame_lines: 2,
                buffer_lines: 1
            }
        ));
    }

    #[test]
    fn test_blame_gutter_keeps_full_identity_for_continuation_rows_without_repeating_text() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 2\n\
             author Alice\n\
             author-time 1700000000\n\
             summary shared commit\n\
             filename src/before rename.rs\n\
             \tone\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2\n\
             \ttwo\n";

        let gutter = BlameGutter::from_file(parse_porcelain(PORCELAIN), 2);
        let first = gutter.annotation_at(0).expect("first line identity");
        let continuation = gutter.annotation_at(1).expect("continuation identity");

        assert!(Arc::ptr_eq(first, continuation));
        assert_eq!(first.sha(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(continuation.author_name(), "Alice");
        assert_eq!(continuation.summary(), "shared commit");
        assert_eq!(
            gutter.origin_at(0),
            Some(crate::app::browse_discussion::LineOrigin {
                sha: Arc::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                path: Arc::from("src/before rename.rs"),
                line: 1,
            })
        );
        assert_eq!(gutter.origin_at(1).map(|origin| origin.line), Some(2));
        assert_eq!(
            gutter.text(1, BlameGutterWidth::Identity),
            BLAME_IDENTITY_BLANK
        );
    }

    #[test]
    fn stale_line_discussion_delivery_is_generation_safe_but_survives_cursor_changes() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             summary first\n\
             filename src/a.rs\n\
             \tone\n\
             bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2 2 1\n\
             author Bob\n\
             summary second\n\
             filename src/a.rs\n\
             \ttwo\n";
        let mut app = App::new_for_test();
        let mut state = state_with_ready_blame(PORCELAIN, 2);
        let path = "src/a.rs".to_string();
        let pr_numbers = vec![42];
        let (first_request, _) = state.begin_line_discussion_load(path.clone(), pr_numbers.clone());
        let (second_request, _) =
            state.begin_line_discussion_load(path.clone(), pr_numbers.clone());
        app.state = AppState::RepoBrowseFile;
        app.browse_state = Some(state);

        app.install_line_discussion_delivery(LineDiscussionDelivery::Comments {
            request_id: first_request,
            path: path.clone(),
            pr_numbers: pr_numbers.clone(),
            fetched_comments: Vec::new(),
            result: Err(LineDiscussionLoadError::Api(
                "stale success slot".to_string(),
            )),
        });
        assert!(matches!(
            app.browse_state.as_ref().unwrap().line_discussion,
            LineDiscussionState::LoadingComments {
                request_id,
                ..
            } if request_id == second_request
        ));
        assert!(app.browse_state.as_ref().unwrap().status.is_none());

        app.browse_state.as_mut().unwrap().cursor_line = 1;
        app.install_line_discussion_delivery(LineDiscussionDelivery::Comments {
            request_id: second_request,
            path,
            pr_numbers,
            fetched_comments: Vec::new(),
            result: Err(LineDiscussionLoadError::Api("moved cursor".to_string())),
        });
        assert!(matches!(
            app.browse_state.as_ref().unwrap().line_discussion,
            LineDiscussionState::Failed {
                failure: LineDiscussionFailure::Api,
                ..
            }
        ));
        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Review comment API failed: moved cursor")
        );
    }

    #[test]
    fn line_discussion_delivery_is_rejected_after_the_open_file_changes() {
        let mut app = App::new_for_test();
        let mut state = state_with_open_file(1);
        let path = "src/a.rs".to_string();
        let pr_numbers = vec![42];
        let (request_id, _) = state.begin_line_discussion_load(path.clone(), pr_numbers.clone());
        state.open.as_mut().unwrap().path = "src/other.rs".to_string();
        app.state = AppState::RepoBrowseFile;
        app.browse_state = Some(state);

        app.install_line_discussion_delivery(LineDiscussionDelivery::Comments {
            request_id,
            path,
            pr_numbers,
            fetched_comments: Vec::new(),
            result: Err(LineDiscussionLoadError::Api("stale file".to_string())),
        });

        assert!(matches!(
            app.browse_state.as_ref().unwrap().line_discussion,
            LineDiscussionState::LoadingComments { .. }
        ));
        assert!(app.browse_state.as_ref().unwrap().status.is_none());
    }

    #[test]
    fn current_line_discussion_api_failure_gets_its_own_footer_message() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             summary first\n\
             filename src/a.rs\n\
             \tone\n";
        let mut app = App::new_for_test();
        let mut state = state_with_ready_blame(PORCELAIN, 1);
        let path = "src/a.rs".to_string();
        let pr_numbers = vec![42];
        let (request_id, _) = state.begin_line_discussion_load(path.clone(), pr_numbers.clone());
        app.state = AppState::RepoBrowseFile;
        app.browse_state = Some(state);

        app.install_line_discussion_delivery(LineDiscussionDelivery::Comments {
            request_id,
            path,
            pr_numbers,
            fetched_comments: Vec::new(),
            result: Err(LineDiscussionLoadError::Api("rate limited".to_string())),
        });

        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Review comment API failed: rate limited")
        );
        assert!(matches!(
            app.browse_state.as_ref().unwrap().line_discussion,
            LineDiscussionState::Failed {
                failure: LineDiscussionFailure::Api,
                ..
            }
        ));
    }

    #[test]
    fn current_line_discussion_distinguishes_no_file_comments_from_no_line_comments() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             summary first\n\
             filename src/a.rs\n\
             \tone\n";

        for (file_thread_count, outcome, expected) in [
            (
                0,
                DiscussionOutcome::Complete,
                "Pull request #42 has no review comments on this file",
            ),
            (
                1,
                DiscussionOutcome::UnplacedThreads { count: 1 },
                "This file has review comments, but none on this line (1 thread(s) could not be placed confidently)",
            ),
        ] {
            let mut app = App::new_for_test();
            let mut state = state_with_ready_blame(PORCELAIN, 1);
            let path = "src/a.rs".to_string();
            let pr_numbers = vec![42];
            let (request_id, _) =
                state.begin_line_discussion_load(path.clone(), pr_numbers.clone());
            app.state = AppState::RepoBrowseFile;
            app.browse_state = Some(state);

            app.install_line_discussion_delivery(LineDiscussionDelivery::Comments {
                request_id,
                path,
                pr_numbers,
                fetched_comments: Vec::new(),
                result: Ok(DiscussionIndex {
                    comments: Vec::new(),
                    threads: Vec::new(),
                    line_threads: vec![smallvec::SmallVec::new()],
                    file_thread_count,
                    comment_paths: Vec::new(),
                    outcome,
                }),
            });

            let state = app.browse_state.as_ref().unwrap();
            assert_eq!(state.status.as_deref(), Some(expected));
            assert!(matches!(
                state.line_discussion,
                LineDiscussionState::Ready {
                    view: DiscussionView::Closed,
                    ..
                }
            ));
            assert!(state.line_discussion_receiver.is_none());
        }
    }

    #[test]
    fn renamed_file_reports_unanchored_comments_under_previous_path() {
        let comment = serde_json::from_value(serde_json::json!({
            "id": 1,
            "path": "src/old.rs",
            "line": null,
            "original_line": 1,
            "side": "RIGHT",
            "original_commit_id": "cccccccccccccccccccccccccccccccccccccccc",
            "body": "old-name discussion",
            "user": { "login": "reviewer" },
            "created_at": "2026-07-28T00:00:00Z"
        }))
        .unwrap();
        let current_origins = vec![Some(LineOrigin {
            sha: Arc::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            path: Arc::from("src/old.rs"),
            line: 1,
        })];
        let index = crate::app::browse_discussion::build_discussion_index(
            vec![comment],
            "src/a.rs",
            &current_origins,
            |_, path, _, _| {
                assert_eq!(path, "src/old.rs");
                Ok(parse_porcelain(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 1 1 1\n\
                     author Alice\n\
                     summary historical\n\
                     filename src/old.rs\n\
                     \told\n",
                ))
            },
            || false,
        )
        .unwrap();
        assert_eq!(index.file_thread_count, 1);
        assert!(index.line_threads.iter().all(smallvec::SmallVec::is_empty));

        let mut app = App::new_for_test();
        let mut state = state_with_open_file(1);
        let path = "src/a.rs".to_string();
        let pr_numbers = vec![42];
        let (request_id, _) = state.begin_line_discussion_load(path.clone(), pr_numbers.clone());
        app.state = AppState::RepoBrowseFile;
        app.browse_state = Some(state);
        app.install_line_discussion_delivery(LineDiscussionDelivery::Comments {
            request_id,
            path,
            pr_numbers,
            fetched_comments: Vec::new(),
            result: Ok(index),
        });

        let message = app
            .browse_state
            .as_ref()
            .and_then(|state| state.status.as_deref())
            .unwrap();
        insta::assert_snapshot!(
            message,
            @"Review comments exist under previous path src/old.rs, but none could be anchored to src/a.rs"
        );
        assert!(!message.contains("no review comments on this file"));
    }

    #[tokio::test]
    async fn file_discussion_marks_and_threads_are_cursor_independent_across_pull_requests() {
        let temp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(temp.path())
                .args([
                    "-c",
                    "user.name=Discussion Fixture",
                    "-c",
                    "user.email=discussion@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .output()
                .expect("git fixture command failed to start");
            assert!(
                output.status.success(),
                "git {} failed with {}\nstdout:\n{}\nstderr:\n{}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };

        git(&["init"]);
        std::fs::write(temp.path().join("main.rs"), "first\n").unwrap();
        git(&["add", "main.rs"]);
        git(&["commit", "-m", "first line"]);
        let first_sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();

        std::fs::write(temp.path().join("main.rs"), "first\nsecond\n").unwrap();
        git(&["add", "main.rs"]);
        git(&["commit", "-m", "second line"]);
        let second_sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();

        let comment = |id: u64, line: u32, revision: &str, body: &str| {
            serde_json::from_value::<crate::github::comment::ReviewComment>(serde_json::json!({
                "id": id,
                "path": "main.rs",
                "line": null,
                "original_line": line,
                "side": "RIGHT",
                "original_commit_id": revision,
                "body": body,
                "user": { "login": "reviewer" },
                "created_at": "2026-07-28T00:00:00Z"
            }))
            .unwrap()
        };

        let mut renders = Vec::new();
        for start_line in [0, 1] {
            let mut app = App::new_for_test();
            app.set_repository_availability(RepositoryAvailability::Available);
            let mut state = BrowseState::new(temp.path().to_path_buf(), AppState::FileList);
            state.set_paths(vec!["main.rs".to_string()]);
            state.open =
                Some(load_file_for_test(&temp.path().join("main.rs"), "main.rs", 4).unwrap());
            let blame = crate::github::blame_file(temp.path(), "main.rs").unwrap();
            state.blame = BlameState::Ready {
                path: "main.rs".to_string(),
                gutter: BlameGutter::from_file(blame, 2),
            };
            state.cursor_line = start_line;
            app.state = AppState::RepoBrowseFile;
            app.browse_state = Some(state);

            for (sha, pr_number) in [(&first_sha, 101), (&second_sha, 202)] {
                app.session_cache.put_commit_pr_resolution(
                    sha.clone(),
                    CommitPrResolution::Confirmed {
                        pulls: vec![CommitPullRequest {
                            number: pr_number,
                            title: format!("PR {pr_number}"),
                            state: CommitPullRequestState::Merged,
                        }],
                    },
                );
            }
            let repo = app.repo.clone();
            app.session_cache.put_browser_review_comments(
                crate::cache::PrCacheKey {
                    repo: repo.clone(),
                    pr_number: 101,
                },
                vec![comment(1, 1, &first_sha, "thread from PR 101")],
            );
            app.session_cache.put_browser_review_comments(
                crate::cache::PrCacheKey {
                    repo,
                    pr_number: 202,
                },
                vec![comment(2, 2, &second_sha, "thread from PR 202")],
            );

            app.open_browse_line_discussion();
            settle_line_discussion(&mut app).await;

            let state = app.browse_state.as_mut().unwrap();
            let LineDiscussionState::Ready { view, .. } = &mut state.line_discussion else {
                panic!("cached discussion lookup did not become ready");
            };
            *view = DiscussionView::Closed;
            state.cursor_line = 0;
            let gutter = render_at(&mut app, 100, 10);
            assert_eq!(
                gutter.matches('●').count(),
                2,
                "both pull requests must mark their line regardless of the trigger cursor:\n{gutter}"
            );

            let mut overlays = Vec::new();
            for line in [0, 1] {
                let state = app.browse_state.as_mut().unwrap();
                state.cursor_line = line;
                let LineDiscussionState::Ready { view, .. } = &mut state.line_discussion else {
                    unreachable!("discussion stayed ready");
                };
                *view = DiscussionView::ThreadList {
                    line,
                    selected: 0,
                    scroll: 0,
                };
                overlays.push(render_at(&mut app, 100, 18));
            }
            assert!(
                overlays[0].contains("thread from PR 101"),
                "{}",
                overlays[0]
            );
            assert!(
                overlays[1].contains("thread from PR 202"),
                "{}",
                overlays[1]
            );
            renders.push((gutter, overlays));
        }

        assert_eq!(
            renders[0], renders[1],
            "the file-wide gutter and per-line overlays changed with the trigger cursor"
        );
    }

    #[test]
    fn test_blame_annotation_short_sha_truncates_on_a_character_boundary() {
        let annotation = prepare_blame_annotation(BlameRef {
            sha: "日本語のテキストです",
            author: "Alice",
            summary: "multibyte identifier",
            author_time: 1_700_000_000,
            original_line: 1,
            original_path: "src/lib.rs",
        });

        assert_eq!(annotation.short_sha(), "日本語のテキス");
    }

    // ===== file loading =====

    #[test]
    fn test_load_file_reads_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        let open = load_file_for_test(&dir.path().join("a.rs"), "a.rs", 4).unwrap();
        assert!(open.viewable);
        assert_eq!(open.lines, vec!["fn main() {}"]);
        assert_eq!(open.source_line(0), Some("fn main() {}"));
        assert_eq!(open.source_line(9), None);
    }

    #[test]
    fn test_load_file_empty_file_is_viewable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.rs"), "").unwrap();
        let open = load_file_for_test(&dir.path().join("empty.rs"), "empty.rs", 4).unwrap();
        assert!(open.viewable);
        assert_eq!(open.line_count(), 0);
        assert_eq!(open.notice.as_deref(), Some("Empty file."));
    }

    #[test]
    fn test_empty_file_renders_an_explicit_content_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.rs");
        std::fs::write(&path, "").unwrap();
        let open = load_file_for_test(&path, "empty.rs", 4).unwrap();
        let mut app = App::new_for_test();
        let mut state = state_with_paths(&["empty.rs"]);
        state.open = Some(open);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseFile;

        let rendered = render_at(&mut app, 100, 10);

        assert!(rendered.contains("Empty file."), "{rendered}");
    }

    #[test]
    fn test_load_file_binary_is_reported_not_rendered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let open = load_file_for_test(&dir.path().join("blob.bin"), "blob.bin", 4).unwrap();
        assert!(!open.viewable);
        assert_eq!(
            open.notice.as_deref(),
            Some("Binary file — no text preview.")
        );
    }

    #[test]
    fn test_load_file_missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_file_for_test(&dir.path().join("nope.rs"), "nope.rs", 4).unwrap_err();
        assert!(err.starts_with("nope.rs:"), "{err}");
    }

    #[test]
    fn test_load_file_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let err = load_file_for_test(&dir.path().join("sub"), "sub", 4).unwrap_err();
        assert_eq!(err, "sub: not a regular file");
    }

    #[test]
    fn test_load_file_oversized_sparse_file_is_reported_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_VIEWABLE_FILE_BYTES + 1024 * 1024).unwrap();

        let open = load_file_for_test(&path, "oversized.txt", 4).unwrap();

        assert!(!open.viewable);
        assert_eq!(
            open.notice.as_deref(),
            Some("File is too large to display: 9437184 bytes exceeds the 8.0 MiB limit.")
        );
    }

    #[test]
    fn test_load_file_one_byte_over_limit_never_reports_equal_rounded_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("just-over-limit.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_VIEWABLE_FILE_BYTES + 1).unwrap();

        let open = load_file_for_test(&path, "just-over-limit.txt", 4).unwrap();

        assert!(!open.viewable);
        assert_eq!(
            open.notice.as_deref(),
            Some("File is too large to display: 8388609 bytes exceeds the 8.0 MiB limit.")
        );
    }

    #[test]
    fn test_load_file_contents_reports_a_read_failure_after_metadata_succeeded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vanished.txt");
        std::fs::write(&path, "gone").unwrap();
        let file_size = std::fs::metadata(&path).unwrap().len();
        std::fs::remove_file(&path).unwrap();

        let FileLoad::Failed(error) = load_file_contents(
            &path,
            "vanished.txt",
            4,
            file_size,
            &CancellationToken::new(),
        ) else {
            panic!("the post-metadata read must fail after the file vanishes");
        };
        assert!(error.starts_with("vanished.txt:"), "{error}");
    }

    #[test]
    fn test_load_file_rejects_too_many_lines() {
        let dir = tempfile::tempdir().unwrap();
        let source = "x\n".repeat(100_001);
        std::fs::write(dir.path().join("many-lines.txt"), source).unwrap();

        let open =
            load_file_for_test(&dir.path().join("many-lines.txt"), "many-lines.txt", 4).unwrap();

        assert!(!open.viewable);
        assert!(
            open.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("100000 line limit")),
            "{:?}",
            open.notice
        );
    }

    #[test]
    fn test_load_file_rejects_too_long_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("minified.js"), "x".repeat(10_001)).unwrap();

        let open = load_file_for_test(&dir.path().join("minified.js"), "minified.js", 4).unwrap();

        assert!(!open.viewable);
        assert!(
            open.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("10000-byte line limit")),
            "{:?}",
            open.notice
        );
    }

    #[test]
    fn test_load_file_just_under_line_count_limit_is_viewable() {
        let dir = tempfile::tempdir().unwrap();
        let source = "x\n".repeat(99_999);
        std::fs::write(dir.path().join("many-lines.txt"), source).unwrap();

        let open =
            load_file_for_test(&dir.path().join("many-lines.txt"), "many-lines.txt", 4).unwrap();

        assert!(open.viewable);
    }

    #[test]
    fn test_cancelled_stage_does_not_run_its_work() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ran = std::cell::Cell::new(false);

        let result = stage(&cancel, || {
            ran.set(true);
            42
        });

        assert_eq!(result, None);
        assert!(!ran.get(), "cancelled work was still invoked");
    }

    /// Fires once it has been polled `limit` times, making "how far into the
    /// load did cancellation reach" an exact assertion with no timing in it.
    struct PollCountCancel {
        limit: usize,
        polls: std::sync::atomic::AtomicUsize,
    }

    impl CancelSignal for PollCountCancel {
        fn is_cancelled(&self) -> bool {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= self.limit
        }
    }

    /// Every step of a file load sits behind its own cancellation gate.
    ///
    /// A pre-cancelled token can only ever prove the *first* gate: it stops the
    /// load before any later one is reached, so deleting a gate deeper in the
    /// chain changes nothing a pre-cancelled test can see. Counting the polls a
    /// complete load needs pins all of them at once — remove any gate and the
    /// count drops, add one and it rises.
    #[test]
    fn test_every_load_step_sits_behind_its_own_cancellation_gate() {
        let dir = tempfile::tempdir().unwrap();
        let absolute = dir.path().join("gated.rs");
        std::fs::write(&absolute, "pub fn gated() {}\n").unwrap();

        let completes = |limit: usize| {
            let cancel = PollCountCancel {
                limit,
                polls: std::sync::atomic::AtomicUsize::new(0),
            };
            matches!(
                load_file(&absolute, "gated.rs", 4, &cancel),
                FileLoad::Ready(_)
            )
        };

        let gates = (0..32)
            .find(|limit| completes(*limit))
            .expect("a load that is never cancelled must complete");

        // metadata, read, UTF-8 conversion, the line scan, lines+patch, cache.
        assert_eq!(
            gates, 6,
            "a load step lost or gained a cancellation gate; a superseded load \
             now does more (or less) work before noticing"
        );
        for limit in 0..gates {
            assert!(
                !completes(limit),
                "cancelling before gate {} still ran the load to completion",
                limit + 1
            );
        }
    }

    #[test]
    fn test_pre_cancelled_file_load_skips_metadata_work() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.rs");
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(matches!(
            load_file(&missing, "missing.rs", 4, &cancel),
            FileLoad::Superseded
        ));
        assert!(matches!(
            load_file(&missing, "missing.rs", 4, &CancellationToken::new()),
            FileLoad::Failed(_)
        ));
    }

    #[test]
    fn test_pre_cancelled_file_contents_skip_read_work() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("vanished.rs");
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(matches!(
            load_file_contents(&missing, "vanished.rs", 4, 0, &cancel),
            FileLoad::Superseded
        ));
        assert!(matches!(
            load_file_contents(&missing, "vanished.rs", 4, 0, &CancellationToken::new()),
            FileLoad::Failed(_)
        ));
    }

    #[test]
    fn test_cancelled_ready_load_is_not_delivered() {
        let (tx, mut rx) = mpsc::channel(1);

        let superseded = CancellationToken::new();
        superseded.cancel();
        deliver_file_load(
            FileLoad::Ready(Box::new(unviewable("a.rs", "notice".to_string(), 4))),
            "a.rs".to_string(),
            &superseded,
            &tx,
        );
        assert!(
            rx.try_recv().is_err(),
            "a load cancelled while it ran must not be delivered"
        );

        let live = CancellationToken::new();
        deliver_file_load(
            FileLoad::Ready(Box::new(unviewable("a.rs", "notice".to_string(), 4))),
            "a.rs".to_string(),
            &live,
            &tx,
        );
        assert!(
            rx.try_recv().is_ok(),
            "a live request must still be delivered"
        );
    }

    #[test]
    fn test_cancelled_blame_result_is_not_delivered() {
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        cancel.cancel();

        deliver_blame_load(Ok(parse_porcelain("")), "a.rs".to_string(), &cancel, &tx);

        assert!(
            rx.try_recv().is_err(),
            "a superseded blame fetch must not reach the UI channel"
        );
    }

    #[test]
    fn test_cancelled_commit_diff_result_is_not_delivered() {
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        cancel.cancel();

        deliver_commit_diff_load(
            CommitDiffLoadResult {
                request_id: 1,
                sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                result: Ok(crate::ui::diff_view::build_plain_diff_cache("", 4)),
            },
            &cancel,
            &tx,
        );

        assert!(
            rx.try_recv().is_err(),
            "a superseded commit diff must not reach the UI channel"
        );
    }

    #[test]
    fn test_commit_diff_limit_is_cache_highlight_admission_after_fetch() {
        let fetched = "x".repeat(MAX_VIEWABLE_COMMIT_DIFF_BYTES + 1);
        assert_eq!(fetched.len(), MAX_VIEWABLE_COMMIT_DIFF_BYTES + 1);

        let error = admit_commit_diff_for_cache(fetched).unwrap_err();

        insta::assert_snapshot!(error, @"commit diff is 32.0 MiB, over the 32.0 MiB browser cache/highlight limit");
    }

    #[tokio::test]
    async fn test_commit_diff_poll_rejects_an_old_generation_even_for_the_same_sha() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let gutter = BlameGutter::from_file(parse_porcelain(PORCELAIN), 1);
        let annotation = Arc::clone(gutter.annotation_at(0).unwrap());
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        state.commit_diff = BrowseCommitDiffState::Loading {
            request_id: 2,
            annotation,
            cancel: state.cancel_token.child_token(),
        };
        let (tx, rx) = mpsc::channel(2);
        state.commit_diff_receiver = Some(rx);
        app.browse_state = Some(state);

        for request_id in [1, 2] {
            tx.send(CommitDiffLoadResult {
                request_id,
                sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                result: Ok(crate::ui::diff_view::build_plain_diff_cache(
                    &format!("+request {request_id}\n"),
                    4,
                )),
            })
            .await
            .unwrap();
        }

        app.poll_browse_updates();
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.commit_diff,
            BrowseCommitDiffState::Loading { request_id: 2, .. }
        ));
        assert!(state.commit_diff_receiver.is_some());

        app.poll_browse_updates();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().commit_diff,
            BrowseCommitDiffState::Ready { ref cache, .. }
                if cache.lines.iter().any(|line| {
                    line.spans.iter().any(|span| cache.resolve(span.content).contains("request 2"))
                })
        ));
    }

    #[tokio::test]
    async fn test_commit_diff_scrolls_with_the_diff_view_margin_behaviour() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let gutter = BlameGutter::from_file(parse_porcelain(PORCELAIN), 1);
        let annotation = Arc::clone(gutter.annotation_at(0).unwrap());
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        state.commit_diff = BrowseCommitDiffState::Loading {
            request_id: 1,
            annotation,
            cancel: state.cancel_token.child_token(),
        };
        let (tx, rx) = mpsc::channel(1);
        state.commit_diff_receiver = Some(rx);
        app.browse_state = Some(state);

        let patch: String = (0..100).map(|i| format!("+line {i}\n")).collect();
        tx.send(CommitDiffLoadResult {
            request_id: 1,
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            result: Ok(crate::ui::diff_view::build_plain_diff_cache(&patch, 4)),
        })
        .await
        .unwrap();
        app.poll_browse_updates();

        let BrowseCommitDiffState::Ready { ref mut scroll, .. } =
            app.browse_state.as_mut().unwrap().commit_diff
        else {
            panic!("commit diff must be ready");
        };
        scroll.set_visible_lines(10);
        for _ in 0..40 {
            scroll.move_down();
        }
        assert_eq!(scroll.selected_line, 40);
        // Margin mode rides the centre: the offset tracks cursor - 4 for a
        // 10-row viewport. Edge mode would sit at 31 with the cursor glued to
        // the bottom row.
        assert_eq!(scroll.scroll_offset, 36);
    }

    #[test]
    fn test_open_blame_pr_reports_no_repo_blame_off_loading_and_uncommitted_without_a_request() {
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        const UNCOMMITTED: &str = "0000000000000000000000000000000000000000 1 1 1\n\
             author Not Committed Yet\n\
             author-time 0\n\
             summary working tree\n\
             \tone\n";

        let mut no_repo = App::new_for_test();
        no_repo.set_repository_availability(RepositoryAvailability::Unavailable);
        no_repo.browse_state = Some(state_with_ready_blame(COMMITTED, 1));
        no_repo.state = AppState::RepoBrowseFile;
        no_repo.open_browse_blame_pr();
        assert_eq!(
            no_repo.browse_state.as_ref().unwrap().status.as_deref(),
            Some("No GitHub repository is associated with this browser session")
        );

        let mut blame_off = App::new_for_test();
        blame_off.set_repository_availability(RepositoryAvailability::Available);
        blame_off.browse_state = Some(state_with_open_file(1));
        blame_off.state = AppState::RepoBrowseFile;
        blame_off.open_browse_blame_pr();
        assert_eq!(
            blame_off.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Blame is off — press gb to enable")
        );

        let mut blame_loading = App::new_for_test();
        blame_loading.set_repository_availability(RepositoryAvailability::Available);
        let mut loading_state = state_with_open_file(1);
        loading_state.blame = BlameState::Loading {
            path: "src/a.rs".to_string(),
            cancel: loading_state.cancel_token.child_token(),
        };
        blame_loading.browse_state = Some(loading_state);
        blame_loading.state = AppState::RepoBrowseFile;
        blame_loading.open_browse_blame_pr();
        assert_eq!(
            blame_loading
                .browse_state
                .as_ref()
                .unwrap()
                .status
                .as_deref(),
            Some("Blame is still loading")
        );

        let mut uncommitted = App::new_for_test();
        uncommitted.set_repository_availability(RepositoryAvailability::Available);
        uncommitted.browse_state = Some(state_with_ready_blame(UNCOMMITTED, 1));
        uncommitted.state = AppState::RepoBrowseFile;
        uncommitted.open_browse_blame_pr();
        assert_eq!(
            uncommitted.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Uncommitted line has no pull request")
        );

        for app in [&no_repo, &blame_off, &blame_loading, &uncommitted] {
            let state = app.browse_state.as_ref().unwrap();
            assert!(matches!(state.pr_lookup, PrLookupState::Idle));
            assert!(state.pr_lookup_receiver.is_none());
        }
    }

    #[test]
    fn test_open_line_discussion_reports_all_pre_fetch_dead_ends_without_a_request() {
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             summary baseline\n\
             filename src/a.rs\n\
             \tone\n";
        const UNCOMMITTED: &str = "0000000000000000000000000000000000000000 1 1 1\n\
             author Not Committed Yet\n\
             summary working tree\n\
             filename src/a.rs\n\
             \tone\n";

        let mut cases = Vec::new();

        let mut no_repo = App::new_for_test();
        no_repo.set_repository_availability(RepositoryAvailability::Unavailable);
        no_repo.browse_state = Some(state_with_ready_blame(COMMITTED, 1));
        no_repo.state = AppState::RepoBrowseFile;
        no_repo.open_browse_line_discussion();
        cases.push((
            no_repo,
            "No GitHub repository is associated with this browser session",
        ));

        let mut blame_off = App::new_for_test();
        blame_off.set_repository_availability(RepositoryAvailability::Available);
        blame_off.browse_state = Some(state_with_open_file(1));
        blame_off.state = AppState::RepoBrowseFile;
        blame_off.open_browse_line_discussion();
        cases.push((blame_off, "Blame is off — press gb to enable"));

        let mut blame_loading = App::new_for_test();
        blame_loading.set_repository_availability(RepositoryAvailability::Available);
        let mut loading_state = state_with_open_file(1);
        loading_state.blame = BlameState::Loading {
            path: "src/a.rs".to_string(),
            cancel: loading_state.cancel_token.child_token(),
        };
        blame_loading.browse_state = Some(loading_state);
        blame_loading.state = AppState::RepoBrowseFile;
        blame_loading.open_browse_line_discussion();
        cases.push((blame_loading, "Blame is still loading"));

        let mut uncommitted = App::new_for_test();
        uncommitted.set_repository_availability(RepositoryAvailability::Available);
        uncommitted.browse_state = Some(state_with_ready_blame(UNCOMMITTED, 1));
        uncommitted.state = AppState::RepoBrowseFile;
        uncommitted.open_browse_line_discussion();
        cases.push((
            uncommitted,
            "This file has no committed lines to look up on GitHub",
        ));

        for (app, expected) in cases {
            let state = app.browse_state.as_ref().unwrap();
            assert_eq!(state.status.as_deref(), Some(expected));
            assert!(matches!(state.line_discussion, LineDiscussionState::Idle));
            assert!(state.line_discussion_receiver.is_none());
        }
    }

    #[test]
    fn test_cached_not_found_answer_reports_the_commit_without_starting_a_request() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary pushed directly\n\
             \tone\n";
        let mut app = App::new_for_test();
        app.set_repository_availability(RepositoryAvailability::Available);
        app.browse_state = Some(state_with_ready_blame(COMMITTED, 1));
        app.state = AppState::RepoBrowseFile;
        app.session_cache
            .put_commit_pr_resolution(SHA.to_string(), CommitPrResolution::NotFound);

        app.open_browse_blame_pr();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.pr_lookup,
            PrLookupState::Failed {
                ref sha,
                failure: PrLookupFailure::NotFound,
            } if sha == SHA
        ));
        assert!(state.pr_lookup_receiver.is_none());
        assert_eq!(
            state.status.as_deref(),
            Some("No pull request found for commit aaaaaaa")
        );
    }

    #[tokio::test]
    async fn test_cached_inferred_answer_opens_pr_with_persistent_unconfirmed_provenance() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary squash merge subject (#123)\n\
             \tone\n";
        let mut app = App::new_for_test();
        app.set_repository_availability(RepositoryAvailability::Available);
        app.browse_state = Some(state_with_ready_blame(COMMITTED, 1));
        app.state = AppState::RepoBrowseFile;
        app.session_cache.put_commit_pr_resolution(
            SHA.to_string(),
            CommitPrResolution::Inferred {
                pull: CommitPullRequest {
                    number: 123,
                    title: "squash merge subject".to_string(),
                    state: CommitPullRequestState::Unknown,
                },
            },
        );

        app.open_browse_blame_pr();

        assert_eq!(app.state, AppState::FileList);
        assert_eq!(app.pr_number, Some(123));
        assert_eq!(
            app.pr_open_source,
            PrOpenSource::InferredCommitSubject {
                sha: SHA.to_string()
            }
        );
        assert_eq!(
            app.pr_open_notice().as_deref(),
            Some("PR #123 inferred from commit subject; GitHub did not confirm it")
        );
    }

    #[tokio::test]
    async fn test_pr_lookup_second_request_cancels_first_and_old_ok_or_err_never_installs() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let mut app = App::new_for_test();
        let mut state = state_with_ready_blame(COMMITTED, 1);
        let (first_id, first_cancel) = state.begin_pr_lookup(SHA);
        let (second_id, second_cancel) = state.begin_pr_lookup(SHA);
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert!(second_id > first_id);

        let (tx, rx) = mpsc::channel(3);
        state.pr_lookup_receiver = Some(rx);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseFile;

        tx.send(PrLookupLoadResult {
            request_id: first_id,
            sha: SHA.to_string(),
            result: Err(CommitPrLookupError::ApiFailure),
        })
        .await
        .unwrap();
        tx.send(PrLookupLoadResult {
            request_id: first_id,
            sha: SHA.to_string(),
            result: Ok(CommitPrResolution::Confirmed {
                pulls: vec![CommitPullRequest {
                    number: 9,
                    title: "stale".to_string(),
                    state: CommitPullRequestState::Open,
                }],
            }),
        })
        .await
        .unwrap();
        let current = CommitPrResolution::Confirmed {
            pulls: vec![CommitPullRequest {
                number: 42,
                title: "current".to_string(),
                state: CommitPullRequestState::Merged,
            }],
        };
        tx.send(PrLookupLoadResult {
            request_id: second_id,
            sha: SHA.to_string(),
            result: Ok(current.clone()),
        })
        .await
        .unwrap();

        app.poll_browse_updates();
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().pr_lookup,
            PrLookupState::Loading {
                request_id,
                ref sha,
                ..
            } if request_id == second_id && sha == SHA
        ));
        assert!(app.session_cache.get_commit_pr_resolution(SHA).is_none());

        app.poll_browse_updates();
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(app.pr_number, Some(1), "stale PR #9 was opened");

        app.poll_browse_updates();
        assert_eq!(app.state, AppState::FileList);
        assert_eq!(app.pr_number, Some(42));
        assert!(app.browse_state.is_none());
        assert_eq!(
            app.session_cache.get_commit_pr_resolution(SHA),
            Some(&current)
        );
    }

    #[tokio::test]
    async fn test_pr_lookup_result_is_cancelled_when_cursor_moves_to_another_commit() {
        const FIRST_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary first\n\
             \tone\n\
             bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2 2 1\n\
             author Bob\n\
             author-time 1700000001\n\
             summary second\n\
             \ttwo\n";
        let mut app = App::new_for_test();
        let mut state = state_with_ready_blame(PORCELAIN, 2);
        let (request_id, cancel) = state.begin_pr_lookup(FIRST_SHA);
        let (tx, rx) = mpsc::channel(1);
        state.pr_lookup_receiver = Some(rx);
        state.cursor_line = 1;
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseFile;

        tx.send(PrLookupLoadResult {
            request_id,
            sha: FIRST_SHA.to_string(),
            result: Ok(CommitPrResolution::Confirmed {
                pulls: vec![CommitPullRequest {
                    number: 9,
                    title: "wrong cursor".to_string(),
                    state: CommitPullRequestState::Closed,
                }],
            }),
        })
        .await
        .unwrap();

        app.poll_browse_updates();

        assert!(cancel.is_cancelled());
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(app.pr_number, Some(1));
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.pr_lookup, PrLookupState::Idle));
        assert!(state.pr_lookup_receiver.is_none());
        assert_eq!(
            state.status.as_deref(),
            Some("Pull request lookup abandoned because the context moved off that commit")
        );
        assert!(app
            .session_cache
            .get_commit_pr_resolution(FIRST_SHA)
            .is_none());
    }

    #[tokio::test]
    async fn test_pr_lookup_failures_map_to_distinct_exact_footer_messages() {
        const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const COMMITTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let cases = [
            (
                CommitPrLookupError::CliMissing,
                "GitHub CLI is not installed or not on PATH",
            ),
            (
                CommitPrLookupError::Unauthenticated,
                "GitHub CLI is not authenticated — run gh auth login",
            ),
            (
                CommitPrLookupError::RateLimited,
                "GitHub API rate limit exceeded — try again later",
            ),
            (
                CommitPrLookupError::CommitNotOnGitHub,
                "GitHub does not know this commit — it may not be pushed yet",
            ),
            (
                CommitPrLookupError::ApiFailure,
                "GitHub API failed while looking up this commit",
            ),
            (
                CommitPrLookupError::EmptyResponse,
                "GitHub returned an empty response for this commit",
            ),
            (
                CommitPrLookupError::MalformedResponse,
                "GitHub returned malformed pull request data for this commit",
            ),
        ];

        for (error, expected) in cases {
            let mut app = App::new_for_test();
            let mut state = state_with_ready_blame(COMMITTED, 1);
            let (request_id, _) = state.begin_pr_lookup(SHA);
            let (tx, rx) = mpsc::channel(1);
            state.pr_lookup_receiver = Some(rx);
            app.browse_state = Some(state);
            app.state = AppState::RepoBrowseFile;
            tx.send(PrLookupLoadResult {
                request_id,
                sha: SHA.to_string(),
                result: Err(error),
            })
            .await
            .unwrap();

            app.poll_browse_updates();

            let state = app.browse_state.as_ref().unwrap();
            assert_eq!(state.status.as_deref(), Some(expected));
            assert!(matches!(
                state.pr_lookup,
                PrLookupState::Failed {
                    failure: PrLookupFailure::Lookup(actual),
                    ..
                } if actual == error
            ));
            assert!(
                app.session_cache.get_commit_pr_resolution(SHA).is_none(),
                "operational failures must remain retryable"
            );
        }
    }

    #[tokio::test]
    async fn test_blame_poll_discards_a_path_mismatch_without_shifting_the_current_file() {
        let mut app = App::new_for_test();
        let mut state = state_with_pending_load("current.rs");
        state.open_load = OpenLoad::Idle;
        state.blame = BlameState::Loading {
            path: "current.rs".to_string(),
            cancel: state.cancel_token.child_token(),
        };
        let (tx, rx) = mpsc::channel(2);
        state.blame_receiver = Some(rx);
        app.browse_state = Some(state);

        tx.send(BlameLoadResult {
            path: "stale.rs".to_string(),
            result: Ok(parse_porcelain("")),
        })
        .await
        .unwrap();
        tx.send(BlameLoadResult {
            path: "current.rs".to_string(),
            result: Ok(parse_porcelain("")),
        })
        .await
        .unwrap();

        app.poll_browse_updates();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Loading { ref path, .. } if path == "current.rs"
        ));
        assert!(app.browse_state.as_ref().unwrap().blame_receiver.is_some());

        app.poll_browse_updates();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Ready { ref path, .. } if path == "current.rs"
        ));
    }

    #[tokio::test]
    async fn test_opening_another_file_cancels_and_replaces_active_blame() {
        let dir = tempfile::tempdir().unwrap();
        if !run_git_fixture("replace-blame", dir.path(), &["init"]) {
            return;
        }
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        assert!(run_git_fixture(
            "replace-blame",
            dir.path(),
            &["add", "a.rs", "b.rs"]
        ));
        assert!(run_git_fixture(
            "replace-blame",
            dir.path(),
            &["commit", "-m", "baseline"]
        ));

        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.open = Some(load_file_for_test(&dir.path().join("a.rs"), "a.rs", 4).unwrap());
        state.blame = BlameState::Loading {
            path: "a.rs".to_string(),
            cancel: state.cancel_token.child_token(),
        };
        let old_cancel = match &state.blame {
            BlameState::Loading { cancel, .. } => cancel.clone(),
            _ => unreachable!(),
        };
        let (_tx, rx) = mpsc::channel(1);
        state.blame_receiver = Some(rx);
        app.browse_state = Some(state);

        app.browse_open_path("b.rs", 0);

        let state = app.browse_state.as_ref().unwrap();
        assert!(old_cancel.is_cancelled());
        assert!(matches!(
            state.blame,
            BlameState::Waiting { ref path } if path == "b.rs"
        ));
        assert!(state.blame_receiver.is_none());

        settle_browse(&mut app).await;
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Loading { ref path, .. } if path == "b.rs"
        ));
        assert!(app.browse_state.as_ref().unwrap().blame_receiver.is_some());

        settle_blame(&mut app).await;
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Ready { ref path, .. } if path == "b.rs"
        ));
    }

    #[tokio::test]
    async fn test_retoggle_cancels_the_old_blame_request_before_starting_another() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();

        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.open = Some(load_file_for_test(&dir.path().join("a.rs"), "a.rs", 4).unwrap());
        app.browse_state = Some(state);

        app.toggle_browse_blame();
        let first = match &app.browse_state.as_ref().unwrap().blame {
            BlameState::Loading { cancel, .. } => cancel.clone(),
            _ => panic!("first toggle did not start blame"),
        };

        app.toggle_browse_blame();
        let state = app.browse_state.as_ref().unwrap();
        assert!(first.is_cancelled());
        assert!(matches!(state.blame, BlameState::Off));
        assert!(state.blame_receiver.is_none());

        app.toggle_browse_blame();
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.blame,
            BlameState::Loading { ref cancel, .. } if !cancel.is_cancelled()
        ));
        assert!(state.blame_receiver.is_some());
    }

    #[tokio::test]
    async fn test_binary_and_oversized_files_never_start_background_blame() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binary.dat"), [0xff, 0xfe, 0x00]).unwrap();
        let oversized = std::fs::File::create(dir.path().join("oversized.dat")).unwrap();
        oversized.set_len(MAX_VIEWABLE_FILE_BYTES + 1).unwrap();

        for path in ["binary.dat", "oversized.dat"] {
            let mut app = App::new_for_test();
            let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
            state.blame = BlameState::Waiting {
                path: path.to_string(),
            };
            app.browse_state = Some(state);
            app.browse_open_path(path, 0);
            settle_browse(&mut app).await;

            let state = app.browse_state.as_ref().unwrap();
            assert!(matches!(state.blame, BlameState::Off), "{path}");
            assert!(state.blame_receiver.is_none(), "{path}");
            assert_eq!(
                state.status.as_deref(),
                Some("Blame is unavailable for this file"),
                "{path}"
            );

            app.browse_state.as_mut().unwrap().status = None;
            app.toggle_browse_blame();
            let state = app.browse_state.as_ref().unwrap();
            assert!(matches!(state.blame, BlameState::Off), "{path}");
            assert!(state.blame_receiver.is_none(), "{path}");
            assert_eq!(
                state.status.as_deref(),
                Some("Blame is unavailable for this file"),
                "{path}"
            );
        }
    }

    #[test]
    fn test_blame_toggle_explains_missing_and_still_loading_files() {
        let mut no_open_app = App::new_for_test();
        no_open_app.browse_state =
            Some(BrowseState::new(PathBuf::from("/repo"), AppState::FileList));

        no_open_app.toggle_browse_blame();

        let state = no_open_app.browse_state.as_ref().unwrap();
        assert!(matches!(state.blame, BlameState::Off));
        assert_eq!(
            state.status.as_deref(),
            Some("Blame is unavailable for this file")
        );

        let mut app = App::new_for_test();
        app.browse_state = Some(state_with_pending_load("slow.rs"));

        app.toggle_browse_blame();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.blame, BlameState::Off));
        assert_eq!(state.status.as_deref(), Some("Still opening this file"));
    }

    #[tokio::test]
    async fn test_untracked_and_no_commit_blame_errors_enter_failed_with_user_message() {
        for (name, initialize_with_commit, expected) in [
            (
                "untracked",
                true,
                BlameError::NotTracked {
                    path: "new.rs".to_string(),
                },
            ),
            ("no-commits", false, BlameError::NoCommitsYet),
        ] {
            let dir = tempfile::tempdir().unwrap();
            if !run_git_fixture(name, dir.path(), &["init"]) {
                return;
            }
            if initialize_with_commit {
                std::fs::write(dir.path().join("baseline.txt"), "baseline\n").unwrap();
                assert!(run_git_fixture(name, dir.path(), &["add", "baseline.txt"]));
                assert!(run_git_fixture(
                    name,
                    dir.path(),
                    &["commit", "-m", "baseline"]
                ));
            }
            std::fs::write(dir.path().join("new.rs"), "fn new_work() {}\n").unwrap();

            let mut app = App::new_for_test();
            app.browse_state = Some(BrowseState::new(
                dir.path().to_path_buf(),
                AppState::FileList,
            ));
            app.browse_open_path("new.rs", 0);
            settle_browse(&mut app).await;
            app.toggle_browse_blame();
            settle_blame(&mut app).await;

            let state = app.browse_state.as_ref().unwrap();
            assert!(matches!(state.blame, BlameState::Failed), "{name}");
            assert_eq!(state.status.as_deref(), Some(expected.to_string().as_str()));
        }
    }

    #[tokio::test]
    async fn test_empty_tracked_file_produces_an_empty_ready_gutter() {
        let dir = tempfile::tempdir().unwrap();
        if !run_git_fixture("empty-blame", dir.path(), &["init"]) {
            return;
        }
        std::fs::write(dir.path().join("empty.rs"), "").unwrap();
        assert!(run_git_fixture(
            "empty-blame",
            dir.path(),
            &["add", "empty.rs"]
        ));
        assert!(run_git_fixture(
            "empty-blame",
            dir.path(),
            &["commit", "-m", "empty"]
        ));

        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));
        app.browse_open_path("empty.rs", 0);
        settle_browse(&mut app).await;
        app.toggle_browse_blame();
        settle_blame(&mut app).await;

        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Ready { ref gutter, .. } if gutter.is_empty()
        ));
    }

    fn plain_cache(source: &str) -> DiffCache {
        crate::ui::diff_view::build_plain_diff_cache(&build_file_patch(source), 4)
    }

    fn empty_repository_files() -> RepositoryFiles {
        RepositoryFiles {
            paths: Vec::new(),
            total: 0,
            skipped_non_utf8: 0,
        }
    }

    #[test]
    fn test_cancelled_session_skips_the_repository_walk_entirely() {
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let walked = std::cell::Cell::new(false);

        deliver_repository_files(&cancel, &tx, || {
            walked.set(true);
            Ok(empty_repository_files())
        });

        assert!(
            !walked.get(),
            "a cancelled session still ran a full git ls-files"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_repository_walk_cancelled_while_it_ran_is_not_delivered() {
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        // Live on entry, cancelled by the time the walk returns — the exact
        // window the entry gate cannot cover.
        deliver_repository_files(&cancel, &tx, || {
            cancel.cancel();
            Ok(empty_repository_files())
        });
        assert!(
            rx.try_recv().is_err(),
            "a walk cancelled while it ran must not be delivered"
        );

        let live = CancellationToken::new();
        deliver_repository_files(&live, &tx, || Ok(empty_repository_files()));
        assert!(
            rx.try_recv().is_ok(),
            "a live session must still receive its file list"
        );
    }

    #[test]
    fn test_cancelled_session_skips_highlighting_entirely() {
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let highlighted = std::cell::Cell::new(false);

        deliver_highlighted_cache(&cancel, &tx, || {
            highlighted.set(true);
            ("a.rs".to_string(), plain_cache("fn main() {}\n"))
        });

        assert!(
            !highlighted.get(),
            "a cancelled session still highlighted a whole file"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_highlight_cancelled_while_it_ran_is_not_delivered() {
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        deliver_highlighted_cache(&cancel, &tx, || {
            cancel.cancel();
            ("a.rs".to_string(), plain_cache("fn main() {}\n"))
        });
        assert!(
            rx.try_recv().is_err(),
            "a highlight cancelled while it ran must not be delivered"
        );

        let live = CancellationToken::new();
        deliver_highlighted_cache(&live, &tx, || {
            ("a.rs".to_string(), plain_cache("fn main() {}\n"))
        });
        assert!(
            rx.try_recv().is_ok(),
            "a live session must still receive its highlighted cache"
        );
    }

    #[test]
    fn test_oversized_file_reports_ready_even_when_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        // The size branch returns before any `stage()` poll, so a request
        // cancelled during `file_metadata` still produces a deliverable file —
        // which is why the delivery guard is not dead code.
        assert!(matches!(
            load_file_contents(
                std::path::Path::new("/nonexistent"),
                "huge.bin",
                4,
                MAX_VIEWABLE_FILE_BYTES + 1,
                &cancel,
            ),
            FileLoad::Ready(_)
        ));
    }

    #[test]
    fn test_load_file_just_under_line_length_limit_is_viewable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bundle.js"), "x".repeat(9_999)).unwrap();

        let open = load_file_for_test(&dir.path().join("bundle.js"), "bundle.js", 4).unwrap();

        assert!(open.viewable);
    }

    /// Both caps are inclusive, so each has two neighbouring cases that must
    /// disagree. A test that only checks the accepted side leaves the rejecting
    /// branch — the one that exists to stop the renderer choking — unguarded.
    #[test]
    fn test_line_length_cap_admits_its_own_value_and_rejects_one_more() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("exact.js"),
            "x".repeat(MAX_VIEWABLE_LINE_BYTES),
        )
        .unwrap();
        let exact = load_file_for_test(&dir.path().join("exact.js"), "exact.js", 4).unwrap();
        assert!(exact.viewable, "the cap is inclusive");
        assert!(exact.notice.is_none());

        std::fs::write(
            dir.path().join("over.js"),
            "x".repeat(MAX_VIEWABLE_LINE_BYTES + 1),
        )
        .unwrap();
        let over = load_file_for_test(&dir.path().join("over.js"), "over.js", 4).unwrap();
        assert!(!over.viewable);
        assert!(
            over.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("too long to display")),
            "{:?}",
            over.notice
        );
    }

    /// The cap is denominated in bytes, so the measurement must be too.
    ///
    /// An ASCII fixture cannot tell `len()` from `chars().count()`. A CJK line
    /// can: three bytes per character means a character-counted measurement
    /// lets roughly three times the intended byte budget through, and the
    /// renderer's cost is driven by bytes, not characters.
    #[test]
    fn test_the_line_length_cap_counts_bytes_not_characters() {
        let dir = tempfile::tempdir().unwrap();
        // Over the byte cap, comfortably under it when counted as characters.
        let line = "あ".repeat(MAX_VIEWABLE_LINE_BYTES / 2);
        assert!(line.len() > MAX_VIEWABLE_LINE_BYTES);
        assert!(line.chars().count() < MAX_VIEWABLE_LINE_BYTES);
        std::fs::write(dir.path().join("cjk.txt"), &line).unwrap();

        let open = load_file_for_test(&dir.path().join("cjk.txt"), "cjk.txt", 4).unwrap();

        assert!(!open.viewable);
        assert!(
            open.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("too long to display")),
            "{:?}",
            open.notice
        );
    }

    #[test]
    fn test_line_count_cap_admits_its_own_value_and_rejects_one_more() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("exact.txt"),
            "x\n".repeat(MAX_VIEWABLE_FILE_LINES),
        )
        .unwrap();
        let exact = load_file_for_test(&dir.path().join("exact.txt"), "exact.txt", 4).unwrap();
        assert!(exact.viewable, "the cap is inclusive");
        assert_eq!(exact.lines.len(), MAX_VIEWABLE_FILE_LINES);
        // The first six-digit line number: the gutter must have widened for it.
        assert_eq!(
            exact.lines.len().to_string().len(),
            6,
            "this test only pins the gutter boundary while the cap has six digits"
        );

        std::fs::write(
            dir.path().join("over.txt"),
            "x\n".repeat(MAX_VIEWABLE_FILE_LINES + 1),
        )
        .unwrap();
        let over = load_file_for_test(&dir.path().join("over.txt"), "over.txt", 4).unwrap();
        assert!(!over.viewable);
        assert!(
            over.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("too many to display")),
            "{:?}",
            over.notice
        );
    }

    #[tokio::test]
    async fn test_even_a_tiny_file_is_read_off_the_ui_thread() {
        let dir = tempfile::tempdir().unwrap();
        let source = "pub fn tiny() {}\n";
        assert!(source.len() <= 20);
        std::fs::write(dir.path().join("tiny.rs"), source).unwrap();

        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));

        app.browse_open_path("tiny.rs", 0);

        let state = app.browse_state.as_ref().unwrap();
        let open = state.open.as_ref().unwrap();
        assert!(matches!(state.open_load, OpenLoad::Pending { .. }));
        assert!(!open.viewable);
        assert!(open
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Loading")));
    }

    #[tokio::test]
    async fn test_browse_open_path_loads_file_in_background() {
        let dir = tempfile::tempdir().unwrap();
        let source = "x\n".repeat(65_537);
        std::fs::write(dir.path().join("background.txt"), &source).unwrap();

        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));

        app.browse_open_path("background.txt", 50_000);

        let placeholder = app.browse_state.as_ref().unwrap().open.as_ref().unwrap();
        assert!(!placeholder.viewable);
        assert!(placeholder
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Loading")));

        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        let open = state.open.as_ref().unwrap();
        assert!(open.viewable);
        assert_eq!(open.lines.len(), 65_537);
        assert_eq!(state.cursor_line, 50_000);
    }

    /// A landed load attaches the file's symbols and moves the tree cursor.
    ///
    /// Both are easy to lose because nothing else in the load path reads them
    /// back: the file still opens, still renders, and every other assertion in
    /// the suite still holds. What breaks is the next thing the user does —
    /// `o` claims the file has no symbols, and the tree resumes from whatever
    /// row it was on before the jump.
    #[tokio::test]
    async fn test_a_landed_load_attaches_symbols_and_moves_the_tree_cursor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("first.rs"), "pub fn first() {}\n").unwrap();
        std::fs::write(dir.path().join("target.rs"), "pub fn target() {}\n").unwrap();

        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["first.rs".to_string(), "target.rs".to_string()]);
        state.index = IndexState::Ready(Arc::new(crate::symbols::SymbolIndex::from_files(vec![
            FileSymbols {
                path: "target.rs".to_string(),
                symbols: vec![Symbol {
                    name: "target".to_string(),
                    kind: SymbolKind::Function,
                    line: 1,
                    column: 7,
                    depth: 0,
                }],
            },
        ])));
        let first_row = state.tree.selected_row;
        app.browse_state = Some(state);

        app.browse_open_path("target.rs", 0);

        // Move the cursor away mid-load, the way a user browsing the tree while
        // a large file reads would. Only the delivery-time re-sync can undo
        // this — the request-time one has already run.
        app.browse_state.as_mut().unwrap().tree.selected_row = first_row;

        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        let open = state.open.as_ref().unwrap();
        assert_eq!(open.path, "target.rs");
        assert_eq!(
            open.symbols.len(),
            1,
            "the index was ready, so the landed file must carry its symbols"
        );
        let target_row = state
            .tree
            .find_row_for_file(1)
            .expect("the opened file has a tree row");
        assert_ne!(target_row, first_row, "the fixture must require a move");
        assert_eq!(
            state.tree.selected_row, target_row,
            "the tree cursor must follow the file the preview pane is showing"
        );
    }

    /// The tree cursor follows the request, not just the completed load.
    ///
    /// Between the request and the delivery the preview pane already shows the
    /// new file's placeholder. A tree that only catches up on completion leaves
    /// the two panes naming different files for the whole load, and forever if
    /// the load fails.
    #[tokio::test]
    async fn test_the_tree_cursor_follows_a_file_while_it_is_still_loading() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("first.rs"), "pub fn first() {}\n").unwrap();
        std::fs::write(dir.path().join("target.rs"), "pub fn target() {}\n").unwrap();

        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["first.rs".to_string(), "target.rs".to_string()]);
        app.browse_state = Some(state);

        app.browse_open_path("target.rs", 0);

        let state = app.browse_state.as_ref().unwrap();
        assert!(
            matches!(state.open_load, OpenLoad::Pending { .. }),
            "the fixture must still be mid-load"
        );
        let target_row = state.tree.find_row_for_file(1).unwrap();
        assert_eq!(state.tree.selected_row, target_row);
    }

    /// Re-requesting the file already in flight adjusts it instead of restarting.
    ///
    /// Pressing the same jump twice, or a second `gd` onto the same target, must
    /// not cancel the read that is already running — restarting it makes the
    /// "Loading…" state outlive every repeat and re-reads the file from disk.
    #[tokio::test]
    async fn test_re_requesting_the_in_flight_file_does_not_restart_its_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), "x\n".repeat(20_000)).unwrap();

        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));

        app.browse_open_path("big.txt", 10);
        let first_request = match app.browse_state.as_ref().unwrap().open_load {
            OpenLoad::Pending { ref cancel, .. } => cancel.clone(),
            _ => panic!("the first request must be pending"),
        };

        app.browse_open_path("big.txt", 900);

        assert!(
            !first_request.is_cancelled(),
            "the in-flight read was cancelled and restarted for the same file"
        );
        match app.browse_state.as_ref().unwrap().open_load {
            OpenLoad::Pending { ref path, line, .. } => {
                assert_eq!(path, "big.txt");
                assert_eq!(line, 900, "the repeat must retarget the pending load");
            }
            _ => panic!("the load must still be the same pending one"),
        }

        settle_browse(&mut app).await;
    }

    /// A dropped file channel only speaks for a load that was actually pending.
    ///
    /// Reporting unconditionally would replace the file the user is reading
    /// with an error about a load that is not happening — and name an empty
    /// path while doing it.
    #[tokio::test]
    async fn test_a_dropped_file_channel_is_silent_when_no_load_is_pending() {
        let mut app = App::new_for_test();
        let mut state = state_with_paths(&["a.rs"]);
        let (tx, rx) = mpsc::channel(1);
        state.file_receiver = Some(rx);
        state.status = Some("something the user is reading".to_string());
        app.browse_state = Some(state);
        drop(tx);

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.open_load, OpenLoad::Idle));
        assert_eq!(
            state.status.as_deref(),
            Some("something the user is reading")
        );
        assert!(state.open.is_none());
    }

    /// A failed load is not a pending one.
    ///
    /// `o` and `gd` check `open_is_pending()` before anything else, so widening
    /// it to include `Failed` would make both answer "Still opening this file"
    /// forever and hide the real error.
    #[test]
    fn test_a_failed_load_is_not_reported_as_still_pending() {
        let mut state = state_with_paths(&["a.rs"]);
        install_file_load_failure(&mut state, "a.rs".to_string(), "boom".to_string(), 4);

        assert!(matches!(state.open_load, OpenLoad::Failed { .. }));
        assert!(
            !state.open_is_pending(),
            "a terminated load must not keep claiming the file is still opening"
        );
    }

    /// The listing-error pane starts at the top.
    ///
    /// It reuses the ordinary content pane, which scrolls, so a scroll position
    /// left over from the file the user had open would put the explanation off
    /// screen and show them a blank pane instead.
    #[test]
    fn test_the_listing_error_pane_is_scrolled_to_its_message() {
        let mut state = state_with_paths(&["a.rs"]);
        state.scroll_offset = 400;
        state.cursor_line = 420;

        install_repository_listing_failure(&mut state, "git exploded".to_string(), 4);

        assert_eq!(state.scroll_offset, 0);
        assert_eq!(state.cursor_line, 0);
        assert!(state
            .open
            .as_ref()
            .is_some_and(|open| open.lines.iter().any(|line| line.contains("git exploded"))));
    }

    /// The repository walk belongs to the session, not to itself.
    ///
    /// Handing the spawned walk a token of its own looks identical from here —
    /// the listing still arrives — but nothing would then stop a full
    /// `git ls-files` over a large repository after the user has closed the
    /// browser or moved to another root. Awaiting the receiver is exact rather
    /// than timed: the task drops its sender when it returns, so `recv`
    /// resolves to `None` only once the walk has finished without delivering.
    #[tokio::test]
    async fn test_a_cancelled_session_delivers_no_repository_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
        let mut app = App::new_for_test();
        app.working_dir = Some(dir.path().to_string_lossy().into_owned());

        app.open_repo_browse();
        let state = app.browse_state.as_mut().unwrap();
        state.cancel_token.cancel();
        let mut receiver = state
            .paths_receiver
            .take()
            .expect("opening the browser installs a listing receiver");

        assert!(
            receiver.recv().await.is_none(),
            "a cancelled session still delivered a repository listing, so the \
             walk is not tied to the session"
        );
    }

    /// Background highlighting belongs to the session too.
    ///
    /// It is the most expensive thing the browser starts, so a highlight that
    /// outlives the screen it was for is the worst one to leak.
    #[tokio::test]
    async fn test_a_cancelled_session_delivers_no_highlighted_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn a() {}\n").unwrap();
        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));

        app.browse_open_path("a.rs", 0);
        settle_browse(&mut app).await;
        assert!(app
            .browse_state
            .as_ref()
            .unwrap()
            .open
            .as_ref()
            .is_some_and(|open| open.viewable));

        app.browse_state.as_mut().unwrap().cancel_token.cancel();
        app.start_browse_highlight();

        let mut receiver = app
            .browse_state
            .as_mut()
            .unwrap()
            .highlight_receiver
            .take()
            .expect("highlighting a viewable file installs a receiver");
        assert!(
            receiver.recv().await.is_none(),
            "a cancelled session still delivered a highlighted cache, so the \
             highlight is not tied to the session"
        );
    }

    #[tokio::test]
    async fn test_unviewable_files_never_start_background_highlighting() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binary.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let oversized = std::fs::File::create(dir.path().join("oversized.txt")).unwrap();
        oversized.set_len(MAX_VIEWABLE_FILE_BYTES + 1).unwrap();
        let mut highlighted = Vec::new();

        for (kind, path) in [("binary", "binary.bin"), ("oversized", "oversized.txt")] {
            let mut app = App::new_for_test();
            app.browse_state = Some(BrowseState::new(
                dir.path().to_path_buf(),
                AppState::FileList,
            ));

            app.browse_open_path(path, 0);
            settle_browse(&mut app).await;

            let state = app.browse_state.as_ref().unwrap();
            assert!(
                !state.open.as_ref().unwrap().viewable,
                "{kind} fixture must settle as an unviewable file"
            );
            if state.highlight_receiver.is_some() {
                highlighted.push(kind);
            }
        }

        assert!(
            highlighted.is_empty(),
            "binary and oversized files must never start background highlighting; receivers were installed for {highlighted:?}"
        );
    }

    #[tokio::test]
    async fn test_opening_a_newer_file_supersedes_an_in_flight_background_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("slow.txt"), "x\n".repeat(65_537)).unwrap();
        std::fs::write(dir.path().join("newer.txt"), "newer\n").unwrap();

        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));

        app.browse_open_path("slow.txt", 10_000);
        let superseded_request = match &app.browse_state.as_ref().unwrap().open_load {
            OpenLoad::Pending { cancel, .. } => cancel.clone(),
            OpenLoad::Idle | OpenLoad::Failed { .. } => panic!("first load must be pending"),
        };
        app.browse_open_path("newer.txt", 0);
        assert!(
            superseded_request.is_cancelled(),
            "receiver replacement alone must not be mistaken for work cancellation"
        );
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        let open = state.open.as_ref().unwrap();
        assert_eq!(open.path, "newer.txt");
        assert_eq!(open.lines, vec!["newer"]);
    }

    #[tokio::test]
    async fn test_opening_a_second_file_cancels_the_first_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));

        app.browse_open_path("a.txt", 0);
        let first_request = match &app.browse_state.as_ref().unwrap().open_load {
            OpenLoad::Pending { cancel, .. } => cancel.clone(),
            OpenLoad::Idle | OpenLoad::Failed { .. } => panic!("first load must be pending"),
        };
        let session = app.browse_state.as_ref().unwrap().cancel_token.clone();

        app.browse_open_path("b.txt", 0);

        assert!(first_request.is_cancelled());
        assert!(!session.is_cancelled());
    }

    #[tokio::test]
    async fn test_close_repo_browse_cancels_an_in_flight_request_token() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));
        app.browse_open_path("a.txt", 0);
        let request = match &app.browse_state.as_ref().unwrap().open_load {
            OpenLoad::Pending { cancel, .. } => cancel.clone(),
            OpenLoad::Idle | OpenLoad::Failed { .. } => panic!("load must be pending"),
        };

        app.close_repo_browse();

        assert!(request.is_cancelled());
    }

    #[tokio::test]
    async fn test_reopening_a_loading_file_at_a_newer_line_uses_the_newer_line() {
        let dir = tempfile::tempdir().unwrap();
        let source: String = (0..2_000).map(|line| format!("line {line}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), source).unwrap();
        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));

        app.browse_open_path("big.txt", 100);
        app.browse_open_path("big.txt", 900);
        settle_browse(&mut app).await;

        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 900);
    }

    #[tokio::test]
    async fn test_reopening_a_failed_file_retries_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            dir.path().to_path_buf(),
            AppState::FileList,
        ));

        app.browse_open_path("retry.txt", 0);
        settle_browse(&mut app).await;
        assert!(matches!(
            app.browse_state.as_ref().unwrap().open_load,
            OpenLoad::Failed { .. }
        ));

        std::fs::write(dir.path().join("retry.txt"), "now present\n").unwrap();
        app.browse_open_path("retry.txt", 0);
        assert!(matches!(
            app.browse_state.as_ref().unwrap().open_load,
            OpenLoad::Pending { .. }
        ));
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.open_load, OpenLoad::Idle));
        assert_eq!(
            state.open.as_ref().unwrap().lines,
            vec!["now present".to_string()]
        );
    }

    #[tokio::test]
    async fn test_jump_back_restores_the_recorded_scroll_after_the_load_lands() {
        let dir = tempfile::tempdir().unwrap();
        let source: String = (0..200).map(|line| format!("line {line}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), &source).unwrap();
        std::fs::write(dir.path().join("b.txt"), &source).unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["a.txt".to_string(), "b.txt".to_string()]);
        app.browse_state = Some(state);

        app.browse_open_path("a.txt", 120);
        settle_browse(&mut app).await;
        {
            let state = app.browse_state.as_mut().unwrap();
            state.cursor_line = 120;
            state.scroll_offset = 100;
        }
        app.browse_push_jump();
        app.browse_open_path("b.txt", 0);
        settle_browse(&mut app).await;

        assert!(app.browse_jump_back());
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.cursor_line, 120);
        assert_eq!(state.scroll_offset, 100);
    }

    #[test]
    fn test_jump_back_restores_scroll_when_the_file_is_already_open() {
        let mut app = App::new_for_test();
        let mut state = state_with_open_file(200);
        state.cursor_line = 120;
        state.scroll_offset = 100;
        app.browse_state = Some(state);
        app.browse_push_jump();
        {
            let state = app.browse_state.as_mut().unwrap();
            state.cursor_line = 150;
            state.scroll_offset = 140;
        }

        assert!(app.browse_jump_back());

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.cursor_line, 120);
        assert_eq!(state.scroll_offset, 100);
    }

    #[tokio::test]
    async fn test_jump_back_updates_a_pending_same_file_target_and_scroll() {
        let dir = tempfile::tempdir().unwrap();
        let source: String = (0..200).map(|line| format!("line {line}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), source).unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["a.txt".to_string()]);
        app.browse_state = Some(state);

        app.browse_open_path("a.txt", 10);
        app.browse_state
            .as_mut()
            .unwrap()
            .jump_stack
            .push(BrowseJump {
                path: "a.txt".to_string(),
                line: 120,
                scroll: 100,
            });
        assert!(app.browse_jump_back());
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.cursor_line, 120);
        assert_eq!(state.scroll_offset, 100);
    }

    /// A delivery for a path we are no longer waiting on must be dropped — and
    /// must not take the in-flight request down with it.
    ///
    /// Consuming the receiver here would strand `current.txt` in `Pending`
    /// forever; forcing `Idle` instead would leave the "Loading…" placeholder
    /// installed while the already-open fast path short-circuits every retry.
    /// Either way the pane is stuck on Loading with no way out.
    #[tokio::test]
    async fn test_a_delivery_for_another_path_is_dropped_without_stranding_the_request() {
        let mut app = App::new_for_test();
        let mut state = state_with_pending_load("current.txt");

        let stale_source = "stale\nreplacement\n";
        let stale_patch = build_file_patch(stale_source);
        let stale_open = OpenFile {
            path: "slow.txt".to_string(),
            cache: crate::ui::diff_view::build_plain_diff_cache(&stale_patch, 4),
            patch: stale_patch,
            lines: stale_source.lines().map(str::to_string).collect(),
            symbols: Vec::new(),
            viewable: true,
            notice: None,
        };
        let (tx, rx) = mpsc::channel(1);
        state.file_receiver = Some(rx);
        app.browse_state = Some(state);
        tx.send(FileLoadResult {
            path: "slow.txt".to_string(),
            result: Ok(stale_open),
        })
        .await
        .unwrap();

        app.poll_browse_updates();

        {
            let state = app.browse_state.as_ref().unwrap();
            assert_eq!(
                state.open.as_ref().unwrap().path,
                "current.txt",
                "a stale delivery must not replace the open file"
            );
            assert!(
                matches!(state.open_load, OpenLoad::Pending { ref path, .. } if path == "current.txt"),
                "the in-flight request must survive a stale delivery"
            );
            assert!(
                state.file_receiver.is_some(),
                "dropping the receiver would strand the in-flight request forever"
            );
        }

        // The request it *was* waiting for still lands on the next poll.
        let real_source = "real\n";
        let real_patch = build_file_patch(real_source);
        let real_open = OpenFile {
            path: "current.txt".to_string(),
            cache: crate::ui::diff_view::build_plain_diff_cache(&real_patch, 4),
            patch: real_patch,
            lines: real_source.lines().map(str::to_string).collect(),
            symbols: Vec::new(),
            viewable: true,
            notice: None,
        };
        tx.send(FileLoadResult {
            path: "current.txt".to_string(),
            result: Ok(real_open),
        })
        .await
        .unwrap();

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.open_load, OpenLoad::Idle));
        let open = state.open.as_ref().unwrap();
        assert_eq!(open.path, "current.txt");
        assert_eq!(open.lines, vec!["real"]);
    }

    #[tokio::test]
    async fn test_close_repo_browse_cancels_the_session_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new_for_test();
        app.working_dir = Some(dir.path().to_string_lossy().into_owned());
        app.open_repo_browse();
        let token = app.browse_state.as_ref().unwrap().cancel_token.clone();

        app.close_repo_browse();

        assert!(token.is_cancelled());
        assert!(app.browse_state.is_none());
    }

    #[tokio::test]
    async fn test_reopening_browser_creates_fresh_session_token_and_index_builds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        let mut app = App::new_for_test();
        app.working_dir = Some(dir.path().to_string_lossy().into_owned());

        app.open_repo_browse();
        let old_session = app.browse_state.as_ref().unwrap().cancel_token.clone();
        app.close_repo_browse();
        assert!(old_session.is_cancelled());

        app.open_repo_browse();
        {
            let state = app.browse_state.as_mut().unwrap();
            assert!(!state.cancel_token.is_cancelled());
            state.paths_receiver = None;
            state.set_paths(vec!["lib.rs".to_string()]);
        }
        app.start_symbol_index_build();
        settle_index(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert!(!state.cancel_token.is_cancelled());
        let index = state.index.ready().expect("fresh session index");
        assert!(!index.definitions("alpha").is_empty());
    }

    #[test]
    fn test_open_repo_browse_reuses_same_root_and_refreshes_return_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new_for_test();
        app.working_dir = Some(dir.path().to_string_lossy().into_owned());
        let state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        let token = state.cancel_token.clone();
        app.browse_state = Some(state);
        app.state = AppState::Help;

        app.open_repo_browse();

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.return_state, AppState::Help);
        assert!(!token.is_cancelled());
        assert_eq!(app.state, AppState::RepoBrowseTree);
    }

    #[tokio::test]
    async fn test_open_repo_browse_replaces_different_root_and_cancels_old_session() {
        let old_dir = tempfile::tempdir().unwrap();
        let new_dir = tempfile::tempdir().unwrap();
        let mut app = App::new_for_test();
        let state = BrowseState::new(old_dir.path().to_path_buf(), AppState::FileList);
        let old_token = state.cancel_token.clone();
        app.browse_state = Some(state);
        app.working_dir = Some(new_dir.path().to_string_lossy().into_owned());
        app.state = AppState::Help;

        app.open_repo_browse();

        let state = app.browse_state.as_ref().unwrap();
        assert!(old_token.is_cancelled());
        assert_eq!(state.repo_root, new_dir.path());
        assert_eq!(state.return_state, AppState::Help);
    }

    #[tokio::test]
    async fn test_poll_browse_updates_records_file_list_error() {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        tx.send(Err("git listing failed".to_string()))
            .await
            .unwrap();

        app.poll_browse_updates();

        assert!(matches!(
            app.browse_state.as_ref().unwrap().paths,
            LoadState::Error(ref message) if message == "git listing failed"
        ));
        assert_eq!(
            app.browse_state
                .as_ref()
                .unwrap()
                .open
                .as_ref()
                .map(|open| open.path.as_str()),
            Some("Repository listing error")
        );
    }

    #[tokio::test]
    async fn test_repository_listing_error_is_complete_in_the_wide_content_pane() {
        let first_line = "fatal: dubious ownership at '/segment-one/segment-two/segment-three/segment-four/segment-five/segment-six'";
        let second_line = "hint: add it to safe.directory before trying again";
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        tx.send(Err(format!("{first_line}\n{second_line}")))
            .await
            .unwrap();

        app.poll_browse_updates();
        let rendered = render_at(&mut app, 120, 16);
        let wrapped_first_line = wrap_display_message(first_line, 60);

        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(
            wrapped_first_line.lines().collect::<String>(),
            first_line,
            "wrapping must preserve every character"
        );
        for chunk in wrapped_first_line.lines() {
            assert!(
                rendered.contains(chunk),
                "wrapped error chunk is unreachable: {chunk:?}\n{rendered}"
            );
        }
        assert!(
            rendered.contains(second_line),
            "the later stderr line was lost:\n{rendered}"
        );
    }

    #[tokio::test]
    async fn test_repository_listing_error_does_not_steal_focus_from_help() {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        app.state = AppState::Help;
        tx.send(Err("fatal: listing failed".to_string()))
            .await
            .unwrap();

        app.poll_browse_updates();

        assert_eq!(app.state, AppState::Help);
        assert_eq!(
            app.browse_state
                .as_ref()
                .and_then(|state| state.open.as_ref())
                .map(|open| open.path.as_str()),
            Some("Repository listing error")
        );
    }

    #[tokio::test]
    async fn test_poll_browse_updates_marks_disconnected_paths_as_error() {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        drop(tx);

        app.poll_browse_updates();

        assert!(matches!(
            app.browse_state.as_ref().unwrap().paths,
            LoadState::Error(ref message) if message == "file listing task ended"
        ));
    }

    #[tokio::test]
    async fn test_poll_browse_updates_renders_file_load_error() {
        let mut app = App::new_for_test();
        let mut state = state_with_pending_load("slow.txt");
        let (tx, rx) = mpsc::channel(1);
        state.file_receiver = Some(rx);
        app.browse_state = Some(state);
        tx.send(FileLoadResult {
            path: "slow.txt".to_string(),
            result: Err("slow.txt: permission denied".to_string()),
        })
        .await
        .unwrap();

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.status.as_deref(), Some("slow.txt: permission denied"));
        assert!(matches!(
            state.open_load,
            OpenLoad::Failed {
                ref path,
                ref message
            } if path == "slow.txt" && message == "slow.txt: permission denied"
        ));
        let open = state.open.as_ref().unwrap();
        assert_eq!(open.path, "slow.txt");
        assert!(!open.viewable);
        assert_eq!(open.notice.as_deref(), Some("slow.txt: permission denied"));
    }

    #[tokio::test]
    async fn test_poll_browse_updates_renders_disconnected_file_load_error() {
        let mut app = App::new_for_test();
        let mut state = state_with_pending_load("slow.txt");
        let (tx, rx) = mpsc::channel(1);
        state.file_receiver = Some(rx);
        app.browse_state = Some(state);
        drop(tx);

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        let message = "slow.txt: file loading task ended";
        assert_eq!(state.status.as_deref(), Some(message));
        assert!(matches!(
            state.open_load,
            OpenLoad::Failed {
                ref path,
                message: ref failure
            } if path == "slow.txt" && failure == message
        ));
        let open = state.open.as_ref().unwrap();
        assert_eq!(open.path, "slow.txt");
        assert!(!open.viewable);
        assert_eq!(open.notice.as_deref(), Some(message));
    }

    #[tokio::test]
    async fn test_poll_browse_updates_reports_truncated_file_listing() {
        let stdout: String = (0..=MAX_BROWSE_FILES)
            .map(|index| format!("file-{index:06}.rs\0"))
            .collect();
        let listing = parse_ls_files(stdout.as_bytes());
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        tx.send(Ok(listing)).await.unwrap();

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.all_paths().len(), MAX_BROWSE_FILES);
        assert_eq!(
            state.status.as_deref(),
            Some(
                "Repository has 200001 files; showing the first 200000. Use a narrower working directory or exclude generated files."
            )
        );
    }

    #[tokio::test]
    async fn test_poll_browse_updates_leaves_status_empty_for_complete_listing() {
        let listing = parse_ls_files(b"README.md\0src/lib.rs\0");
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        tx.send(Ok(listing)).await.unwrap();

        app.poll_browse_updates();

        assert!(app.browse_state.as_ref().unwrap().status.is_none());
    }

    #[tokio::test]
    async fn test_empty_repository_renders_an_explicit_status() {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        tx.send(Ok(parse_ls_files(b""))).await.unwrap();

        app.poll_browse_updates();
        let rendered = render_at(&mut app, 100, 10);

        assert!(
            rendered.contains("Repository contains no files."),
            "{rendered}"
        );
    }

    #[test]
    fn test_filter_with_no_paths_renders_an_explicit_status() {
        let mut app = App::new_for_test();
        let mut state = state_with_paths(&["src/app.rs"]);
        let mut filter = ListFilter::new();
        filter.query = "missing".to_string();
        state.filter = Some(filter);
        state.apply_filter();
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;

        let rendered = render_at(&mut app, 100, 10);

        assert!(
            rendered.contains("No files match filter \"missing\"."),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn test_empty_repository_does_not_blame_the_filter() {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        tx.send(Ok(parse_ls_files(b""))).await.unwrap();
        app.poll_browse_updates();

        let state = app.browse_state.as_mut().unwrap();
        let mut filter = ListFilter::new();
        filter.query = "anything".to_string();
        state.filter = Some(filter);
        state.apply_filter();

        // Nothing matches because the repository is empty, not because the
        // query is too narrow. Blaming the query sends the user to fix the
        // wrong thing.
        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Repository contains no files.")
        );
    }

    #[tokio::test]
    async fn test_clearing_a_filter_restores_the_repository_status_it_replaced() {
        let listing = RepositoryFiles {
            paths: vec!["src/app.rs".to_string()],
            total: MAX_BROWSE_FILES + 1,
            skipped_non_utf8: 0,
        };
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        tx.send(Ok(listing)).await.unwrap();
        app.poll_browse_updates();

        let truncation = app
            .browse_state
            .as_ref()
            .unwrap()
            .status
            .clone()
            .expect("a truncated listing must say so");
        assert!(truncation.contains("showing the first"), "{truncation}");

        let state = app.browse_state.as_mut().unwrap();
        let mut filter = ListFilter::new();
        filter.query = "missing".to_string();
        state.filter = Some(filter);
        state.apply_filter();
        assert!(state
            .status
            .as_deref()
            .is_some_and(|message| message.starts_with("No files match filter")));

        // Clearing the filter must not take the repository-level warning with
        // it — the listing is still truncated.
        state.filter.as_mut().unwrap().query.clear();
        state.apply_filter();
        assert_eq!(state.status.as_deref(), Some(truncation.as_str()));
    }

    #[tokio::test]
    async fn test_non_utf8_path_skip_count_is_visible() {
        let listing = parse_ls_files(b"src/valid.rs\0src/\xff.rs\0");
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        tx.send(Ok(listing)).await.unwrap();

        app.poll_browse_updates();
        let rendered = render_at(&mut app, 120, 10);

        assert!(
            rendered.contains(
                "Skipped 1 repository path that is not valid UTF-8 and cannot be represented"
            ),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn test_poll_browse_updates_marks_disconnected_index_failed() {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.index = IndexState::Building;
        state.module_graph = ModuleGraphState::Building;
        state.index_receiver = Some(rx);
        app.browse_state = Some(state);
        drop(tx);

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.index,
            IndexState::Failed { ref reason } if reason == "code indexing task ended"
        ));
        assert!(matches!(state.module_graph, ModuleGraphState::Failed));
    }

    #[test]
    fn test_module_graph_delivery_preserves_a_newer_status() {
        let mut app = App::new_for_test();
        let mut state = state_with_open_file(1);
        let (tx, rx) = mpsc::channel(1);
        state.module_graph_query_receiver = Some(rx);
        state.module_graph_query_cancel = Some(CancellationToken::new());
        state.module_graph_pane = ModuleGraphPaneState::Loading {
            request_id: 9,
            path: "src/a.rs".to_string(),
        };
        state.status = Some("No position to jump back to".to_string());
        app.browse_state = Some(state);
        tx.try_send(ModuleGraphPanelDelivery {
            request_id: 9,
            path: "src/a.rs".to_string(),
            panel: ModuleGraphPanel {
                path: "src/a.rs".to_string(),
                display_path: "src/a.rs".to_string(),
                direction: ModuleGraphDirection::Dependencies,
                selected: 0,
                dependencies: ModuleGraphRows {
                    rows: Vec::new(),
                    total: 0,
                    guarantee: DependencyGuarantee::Exact,
                },
                dependents: ModuleGraphRows {
                    rows: Vec::new(),
                    total: 0,
                    guarantee: DependencyGuarantee::Exact,
                },
            },
        })
        .unwrap();

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Ready(_)
        ));
        assert_eq!(state.status.as_deref(), Some("No position to jump back to"));
    }

    #[test]
    fn test_poll_browse_updates_reports_a_disconnected_module_graph_query() {
        let mut app = App::new_for_test();
        let mut state = state_with_open_file(1);
        let (tx, rx) = mpsc::channel::<ModuleGraphPanelDelivery>(1);
        state.module_graph_query_receiver = Some(rx);
        state.module_graph_query_cancel = Some(CancellationToken::new());
        state.module_graph_pane = ModuleGraphPaneState::Loading {
            request_id: 9,
            path: "src/a.rs".to_string(),
        };
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseGraph;
        drop(tx);

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.module_graph_pane,
            ModuleGraphPaneState::Closed
        ));
        assert_eq!(state.status.as_deref(), Some("Dependency query task ended"));
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert!(state.module_graph_query_receiver.is_none());
    }

    #[tokio::test]
    async fn test_symbol_index_failure_reports_instead_of_a_silent_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let missing_root = dir.path().join("removed-worktree");
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(missing_root, AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        tx.send(Ok(RepositoryFiles {
            paths: vec!["src/lib.rs".to_string()],
            total: 1,
            skipped_non_utf8: 0,
        }))
        .await
        .unwrap();

        settle_index(&mut app).await;

        let reason = {
            let state = app.browse_state.as_ref().unwrap();
            let IndexState::Failed { reason } = &state.index else {
                panic!("a failed index build must keep its error");
            };
            assert!(reason.contains("cannot build symbol index"), "{reason}");
            assert!(reason.contains("removed-worktree"), "{reason}");
            assert_eq!(state.status.as_deref(), Some(reason.as_str()));
            assert!(matches!(state.module_graph, ModuleGraphState::Failed));
            reason.clone()
        };
        let expected = format!("Symbol index is unavailable: {reason}");
        app.handle_repo_browse_tree_input(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Char('s'),
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
        .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(state.status.as_deref(), Some(expected.as_str()));
    }

    #[tokio::test]
    async fn test_symbol_index_build_completes_and_becomes_ready() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn searchable_alpha() {}\n",
        )
        .unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        tx.send(Ok(RepositoryFiles {
            paths: vec!["src/lib.rs".to_string()],
            total: 1,
            skipped_non_utf8: 0,
        }))
        .await
        .unwrap();

        settle_index(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        let IndexState::Ready(index) = &state.index else {
            panic!("completed build must become ready");
        };
        assert!(!index.search("searchable_alpha", 10).is_empty());
    }

    #[test]
    fn test_repository_listing_completeness_controls_graph_universe() {
        assert_eq!(
            RepositoryFiles {
                paths: vec!["src/app.ts".to_string()],
                total: 1,
                skipped_non_utf8: 0,
            }
            .source_universe(),
            crate::module_graph::SourceUniverse::Complete
        );
        assert_eq!(
            RepositoryFiles {
                paths: vec!["src/app.ts".to_string()],
                total: 2,
                skipped_non_utf8: 0,
            }
            .source_universe(),
            crate::module_graph::SourceUniverse::Partial
        );
        assert_eq!(
            RepositoryFiles {
                paths: vec!["src/app.ts".to_string()],
                total: 1,
                skipped_non_utf8: 1,
            }
            .source_universe(),
            crate::module_graph::SourceUniverse::Partial
        );
    }

    #[tokio::test]
    async fn test_combined_index_build_makes_symbols_and_module_graph_ready_together() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/app.ts"),
            "import { helper } from './helper';\nexport function app() { return helper(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/helper.ts"),
            "export function helper() { return 1; }\n",
        )
        .unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        app.browse_state = Some(state);
        tx.send(Ok(RepositoryFiles {
            paths: vec!["src/app.ts".to_string(), "src/helper.ts".to_string()],
            total: 2,
            skipped_non_utf8: 0,
        }))
        .await
        .unwrap();

        settle_index(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        let IndexState::Ready(symbols) = &state.index else {
            panic!("symbols must become ready");
        };
        assert!(!symbols.search("helper", 10).is_empty());
        let ModuleGraphState::Ready(modules) = &state.module_graph else {
            panic!("module graph must become ready with symbols");
        };
        assert_eq!(
            modules.dependencies("src/app.ts").unwrap().edges[0].target,
            crate::module_graph::DependencyTarget::Path("src/helper.ts".to_string())
        );
    }

    #[tokio::test]
    async fn test_pre_cancelled_real_symbol_index_build_delivers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn cancelled_before_build() {}\n",
        )
        .unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["src/lib.rs".to_string()]);
        state.cancel_token.cancel();
        app.browse_state = Some(state);

        app.start_symbol_index_build();

        let state = app.browse_state.as_mut().unwrap();
        assert!(
            matches!(state.index, IndexState::Building),
            "a pre-cancelled real build must still enter the Building state"
        );
        let mut receiver = state
            .index_receiver
            .take()
            .expect("a pre-cancelled real build must install its result receiver");
        assert!(
            receiver.recv().await.is_none(),
            "a cancelled symbol index build must deliver nothing"
        );
    }

    #[tokio::test]
    async fn test_start_symbol_index_build_preserves_an_in_flight_build() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn disk_symbol() {}\n").unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["src/lib.rs".to_string()]);
        state.index = IndexState::Building;
        let (tx, rx) = mpsc::channel(1);
        state.index_receiver = Some(rx);
        app.browse_state = Some(state);

        app.start_symbol_index_build();

        let injected_name = "injected_in_flight_sentinel";
        let injected_index = SymbolIndex::from_files(vec![FileSymbols {
            path: "injected.rs".to_string(),
            symbols: vec![Symbol {
                name: injected_name.to_string(),
                kind: SymbolKind::Function,
                line: 7,
                column: 0,
                depth: 0,
            }],
        }]);
        let injected_code = CodeIndex {
            symbols: injected_index,
            modules: ModuleGraph::default(),
        };
        assert!(
            tx.send(IndexDelivery::Ready(Box::new(injected_code)))
                .await
                .is_ok(),
            "starting a second build must not replace and disconnect the in-flight receiver"
        );
        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        let index = state
            .index
            .ready()
            .expect("the delivery from the preserved in-flight receiver must become ready");
        assert!(
            !index.search(injected_name, 10).is_empty(),
            "the preserved in-flight receiver must install the injected index, not a replacement build"
        );
    }

    #[tokio::test]
    async fn test_disconnected_index_channel_from_cancelled_session_does_not_report_failure() {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        let (tx, rx) = mpsc::channel(1);
        state.index = IndexState::Building;
        state.index_receiver = Some(rx);
        state.cancel_token.cancel();
        app.browse_state = Some(state);
        drop(tx);

        app.poll_browse_updates();

        let state = app.browse_state.as_ref().unwrap();
        assert!(
            matches!(state.index, IndexState::Building),
            "a disconnected index channel from a cancelled session must preserve the Building state"
        );
        assert!(
            state.status.is_none(),
            "a disconnected index channel from a cancelled session must not report a failure"
        );
    }

    #[tokio::test]
    async fn test_poll_browse_updates_applies_highlight_only_to_matching_path() {
        let mut app = App::new_for_test();
        let mut state = state_with_open_file(3);
        let old_hash = state.open.as_ref().unwrap().cache.patch_hash;
        let replacement_patch = build_file_patch("replacement\n");
        let replacement_hash =
            crate::ui::diff_view::build_plain_diff_cache(&replacement_patch, 4).patch_hash;
        assert_ne!(old_hash, replacement_hash);

        let (stale_tx, stale_rx) = mpsc::channel(1);
        state.highlight_receiver = Some(stale_rx);
        app.browse_state = Some(state);
        stale_tx
            .send((
                "src/other.rs".to_string(),
                crate::ui::diff_view::build_plain_diff_cache(&replacement_patch, 4),
            ))
            .await
            .unwrap();

        app.poll_browse_updates();

        assert_eq!(
            app.browse_state
                .as_ref()
                .unwrap()
                .open
                .as_ref()
                .unwrap()
                .cache
                .patch_hash,
            old_hash,
            "a stale path must leave the current cache unchanged"
        );

        let (matching_tx, matching_rx) = mpsc::channel(1);
        app.browse_state.as_mut().unwrap().highlight_receiver = Some(matching_rx);
        matching_tx
            .send((
                "src/a.rs".to_string(),
                crate::ui::diff_view::build_plain_diff_cache(&replacement_patch, 4),
            ))
            .await
            .unwrap();

        app.poll_browse_updates();

        assert_eq!(
            app.browse_state
                .as_ref()
                .unwrap()
                .open
                .as_ref()
                .unwrap()
                .cache
                .patch_hash,
            replacement_hash,
            "a matching path must install the delivered cache"
        );
    }

    #[tokio::test]
    async fn test_browse_highlight_uses_runtime_markdown_rich_setting() {
        for markdown_rich in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("README.md"), "# Heading\n").unwrap();
            let mut app = App::new_for_test();
            app.markdown_rich = markdown_rich;
            assert_eq!(
                app.is_markdown_rich(),
                markdown_rich,
                "test setup must exercise the requested markdown mode"
            );
            app.browse_state = Some(BrowseState::new(
                dir.path().to_path_buf(),
                AppState::FileList,
            ));

            app.browse_open_path("README.md", 0);
            for _ in 0..1_000 {
                app.poll_browse_updates();
                if app
                    .browse_state
                    .as_ref()
                    .and_then(|state| state.open.as_ref())
                    .is_some_and(|open| open.cache.highlighted)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }

            let cache = &app
                .browse_state
                .as_ref()
                .unwrap()
                .open
                .as_ref()
                .unwrap()
                .cache;
            assert!(cache.highlighted);
            assert_eq!(
                cache.markdown_rich, markdown_rich,
                "highlight cache must use the runtime markdown mode"
            );
        }
    }

    #[test]
    fn test_human_bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(8 * 1024 * 1024), "8.0 MiB");
    }

    // ===== listing a real repository =====

    #[test]
    fn test_list_repository_files_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        // A bare temp dir is not a git repo — the browser must report that
        // rather than showing an empty tree as if the repo had no files.
        assert!(list_repository_files(dir.path()).is_err());
    }

    #[test]
    fn test_list_repository_files_includes_untracked_but_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let test_name = "test_list_repository_files_includes_untracked_but_not_ignored";
        if !run_git_fixture(test_name, dir.path(), &["init"]) {
            return;
        }
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("committed.rs"), "pub fn a() {}\n").unwrap();
        if !run_git_fixture(
            test_name,
            dir.path(),
            &["add", ".gitignore", "committed.rs"],
        ) {
            return;
        }
        if !run_git_fixture(test_name, dir.path(), &["commit", "-m", "fixture"]) {
            return;
        }
        std::fs::write(dir.path().join("brand_new.rs"), "pub fn b() {}\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "pub fn c() {}\n").unwrap();

        let paths = list_repository_files(dir.path()).unwrap().paths;
        assert!(paths.contains(&"committed.rs".to_string()), "{paths:?}");
        assert!(
            paths.contains(&"brand_new.rs".to_string()),
            "a file written seconds ago must be browsable: {paths:?}"
        );
        assert!(!paths.contains(&"ignored.rs".to_string()), "{paths:?}");
    }

    #[test]
    fn test_list_repository_files_preserves_cjk_path() {
        let dir = tempfile::tempdir().unwrap();
        let test_name = "test_list_repository_files_preserves_cjk_path";
        if !run_git_fixture(test_name, dir.path(), &["init"]) {
            return;
        }

        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/日本語.rs"), "pub fn alpha() {}\n").unwrap();
        if !run_git_fixture(test_name, dir.path(), &["add", "src/日本語.rs"]) {
            return;
        }
        if !run_git_fixture(test_name, dir.path(), &["commit", "-m", "fixture"]) {
            return;
        }

        let paths = list_repository_files(dir.path()).unwrap().paths;
        assert!(
            paths.contains(&"src/日本語.rs".to_string()),
            "CJK path must not be C-quoted: {paths:?}"
        );
    }
}
