//! Unified highlighter supporting both tree-sitter and syntect.
//!
//! Tree-sitter is used for supported languages:
//! - Rust, TypeScript/TSX, JavaScript/JSX, Go, Python (original)
//! - Ruby, Zig, C, C++, Java, C# (added)
//! - Lua, Bash/Shell, PHP, Swift, Haskell (Phase 1)
//! - MoonBit (Phase 2)
//!
//! Syntect is used as a fallback for other languages (Vue, Svelte, YAML, Markdown, etc.).

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Seek, SeekFrom, Write};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use lasso::Rodeo;
use ratatui::style::Style;
use rustc_hash::{FxHashMap, FxHasher};
use smallvec::smallvec;
use syntect::easy::HighlightLines;
use tokio_util::sync::CancellationToken;
use tree_sitter::{
    Node, Query, QueryCapture, QueryCursor, QueryCursorOptions, QueryCursorState,
    StreamingIterator, Tree,
};

use crate::app::{InternedSpan, SpanVec};
use crate::language::SupportedLanguage;

use super::parser_pool::ParserPool;
use super::themes::ThemeStyleCache;
use super::{convert_syntect_style, get_theme, syntax_for_file, syntax_set};

/// Wall-clock budget for one *prefetch* file.
///
/// The file the user is looking at does not use this. A large linear file is
/// allowed to finish and replace the plain diff. Prefetch uses the budget so
/// one slow file cannot hold the queue.
///
/// Measured on `3b1ec19` (bench profile, parser pool reused,
/// `generate_haskell_diff_patch`, `has_error == true`): 1000 lines 159 ms,
/// 2000 lines 1.38 s, 4000 lines 8.05 s.
pub const PREFETCH_QUERY_BUDGET: Duration = Duration::from_secs(1);

/// Documented maximum for [`QueryCursor::set_match_limit`] (`> 0` and `<= 65536`).
///
/// This counts in-progress capture lists, not total captures. The default
/// cursor limit is `u32::MAX` (grow as needed). Setting this value makes
/// tree-sitter drop the earliest in-progress match once the pool is full
/// (`tree-sitter` `api.h`). On the 2000-line generated Haskell input
/// (`has_error == true`, 22,297 captures) the flag stayed false.
///
/// The cap cannot be raised. The viewed file therefore does not set it: a
/// large file's captures would otherwise be stolen and the highlight thrown
/// away. Prefetch still sets it and publishes the plain diff if it trips.
pub const HIGHLIGHT_MATCH_LIMIT: u32 = 65_536;

/// Visible-node stride for a deep-tree highlight query.
///
/// Patterns such as Haskell `(apply (_) . [variable])` keep a partial match
/// alive at every level of a left-nested chain. Query cost then grows with
/// nodes × depth. Starting a fresh cursor every this many levels, with
/// [`QueryCursor::set_max_start_depth`] set to the same value, caps how many
/// partial matches one cursor carries. Adjacent slabs overlap by one level;
/// matches are deduped on `(pattern, captures)`.
const QUERY_DEPTH_STRIDE: u32 = 8;

/// Trees deeper than this may use [`QUERY_DEPTH_STRIDE`] slabs.
///
/// Slab queries are slower on ordinary files, and on bushy trees the replay
/// buffer costs more than the depth cap saves. Measured whole-cursor times
/// stay ahead of slabs through a few hundred levels (a real module at depth
/// ~350, a million-line JavaScript file at depth ~300). The generated Haskell
/// chain is already faster at depth ~700. At or below this depth the cursor
/// is the single whole-tree query.
const DEEP_TREE_MIN_DEPTH: u32 = 512;

/// Maximum average nodes per visible level for the slab path.
///
/// `nodes / depth` above this is a bushy tree: one whole-tree cursor is
/// cheaper in both time and memory. The generated Haskell chain sits near
/// 10–20. A wide file of real functions is hundreds or thousands.
const DEEP_TREE_MAX_NODES_PER_LEVEL: usize = 48;

/// A level wider than this is not a chain. Scanning it costs more than the
/// slab path can give back, and a left-nested `apply` chain has a handful of
/// children per node. Wider levels stay on the whole-tree cursor.
const DEEP_TREE_MAX_FANOUT: usize = 4096;

/// Why a highlight was thrown away. The caller keeps the plain diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightAbort {
    /// The file switch (or a dropped prefetch) cancelled the job.
    Cancelled,
    /// [`PREFETCH_QUERY_BUDGET`] elapsed during the query.
    TimedOut,
    /// [`HIGHLIGHT_MATCH_LIMIT`] in-progress matches was exceeded.
    MatchLimit,
}

/// A slab walk stopped because the query was aborted, or because the spill
/// file could not be written. A spill failure falls back to the whole-tree
/// cursor so the highlight still completes when the temp directory is unusable.
#[derive(Debug)]
enum SlabQueryError {
    Abort(HighlightAbort),
    Spill,
}

impl From<HighlightAbort> for SlabQueryError {
    fn from(reason: HighlightAbort) -> Self {
        Self::Abort(reason)
    }
}

thread_local! {
    static SPILL_UNUSABLE: Cell<bool> = const { Cell::new(false) };
}

fn spill_unusable() -> bool {
    SPILL_UNUSABLE.with(Cell::get)
}

fn note_spill_unusable() {
    SPILL_UNUSABLE.with(|flag| flag.set(true));
}

#[cfg(test)]
thread_local! {
    static FORCE_SPILL_FAILURE: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn force_spill_failure(fail: bool) {
    FORCE_SPILL_FAILURE.with(|flag| flag.set(fail));
}

#[cfg(test)]
fn spill_forced_to_fail() -> bool {
    FORCE_SPILL_FAILURE.with(Cell::get)
}

/// Cooperative stop signal checked before a query and from tree-sitter's
/// progress callback (every 100 query operations).
pub trait HighlightControl {
    fn poll_abort(&self) -> Option<HighlightAbort>;

    /// `Some` installs [`QueryCursor::set_match_limit`] and discards the
    /// highlight if tree-sitter reports that in-progress matches were stolen.
    ///
    /// `None` leaves the cursor at its default (`u32::MAX`). The viewed file
    /// uses `None` because the documented maximum is [`HIGHLIGHT_MATCH_LIMIT`]
    /// and a stolen-match result must not come back as the plain diff.
    fn match_limit(&self) -> Option<u32> {
        None
    }
}

/// Cancellation only. The viewed file uses this: no deadline and no match cap,
/// so a large highlight can finish and replace the plain diff. Switching files
/// cancels `cancel`.
pub struct CancelHighlight {
    cancel: CancellationToken,
}

impl CancelHighlight {
    pub fn new(cancel: CancellationToken) -> Self {
        Self { cancel }
    }
}

impl HighlightControl for CancelHighlight {
    fn poll_abort(&self) -> Option<HighlightAbort> {
        self.cancel
            .is_cancelled()
            .then_some(HighlightAbort::Cancelled)
    }
}

/// No deadline and no cancel. Direct highlight callers (tests, suggestion text)
/// use this so a small snippet is not subject to the diff-view budget.
pub struct OpenHighlight;

impl HighlightControl for OpenHighlight {
    fn poll_abort(&self) -> Option<HighlightAbort> {
        None
    }
}

/// Prefetch budget: a cancellation token plus [`PREFETCH_QUERY_BUDGET`].
///
/// Not used for the file on screen. An expired budget publishes the plain diff
/// for that prefetched file only.
pub struct HighlightBudget {
    cancel: CancellationToken,
    deadline: Instant,
}

impl HighlightBudget {
    pub fn new(cancel: CancellationToken, budget: Duration) -> Self {
        let deadline = Instant::now()
            .checked_add(budget)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
        Self { cancel, deadline }
    }

    /// A budget that is already expired. Tests use this to prove the deadline
    /// is consulted.
    pub fn already_expired(cancel: CancellationToken) -> Self {
        let deadline = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        Self { cancel, deadline }
    }
}

impl HighlightControl for HighlightBudget {
    fn poll_abort(&self) -> Option<HighlightAbort> {
        if self.cancel.is_cancelled() {
            Some(HighlightAbort::Cancelled)
        } else if Instant::now() >= self.deadline {
            Some(HighlightAbort::TimedOut)
        } else {
            None
        }
    }

    fn match_limit(&self) -> Option<u32> {
        Some(HIGHLIGHT_MATCH_LIMIT)
    }
}

/// Unified highlighter that can use either tree-sitter or syntect.
///
/// This type does not hold any mutable references to `ParserPool`, allowing
/// the pool to be borrowed again during injection processing (e.g., for Svelte).
///
/// Query compilation is deferred to `ParserPool::get_or_create_query()` for caching.
pub enum Highlighter {
    /// Tree-sitter CST highlighter for supported languages.
    Cst {
        /// The supported language (used to look up parser/query from pool)
        supported_lang: SupportedLanguage,
        /// Pre-computed style cache from the theme.
        style_cache: ThemeStyleCache,
    },
    /// Syntect regex-based highlighter for fallback.
    Syntect(HighlightLines<'static>),
    /// No highlighting available.
    None,
}

/// A single highlight capture with byte range and style.
#[derive(Clone, Debug)]
pub struct LineCapture {
    /// Start byte offset within the line (relative to line start)
    pub local_start: usize,
    /// End byte offset within the line (relative to line start)
    pub local_end: usize,
    /// Style for this capture
    pub style: Style,
}

/// Pre-computed highlights grouped by source line index.
///
/// This allows O(1) lookup of highlights per line, avoiding repeated tree traversal.
pub struct LineHighlights {
    /// Map from source line index to captures in that line
    captures_by_line: HashMap<usize, Vec<LineCapture>>,
}

impl LineHighlights {
    /// Create an empty LineHighlights.
    pub fn empty() -> Self {
        Self {
            captures_by_line: HashMap::new(),
        }
    }

    /// Get captures for a specific line index.
    pub fn get(&self, line_index: usize) -> Option<&[LineCapture]> {
        self.captures_by_line.get(&line_index).map(|v| v.as_slice())
    }
}

/// Parsed tree-sitter result.
///
/// Query is not included here - it should be obtained from `ParserPool::get_or_create_query()`
/// to benefit from query caching.
pub struct CstParseResult {
    pub tree: Tree,
    /// The language that was parsed (use this to get cached query from ParserPool)
    pub lang: SupportedLanguage,
}

impl Highlighter {
    /// Create a highlighter for the given filename.
    ///
    /// Attempts to use tree-sitter first, falling back to syntect if the language
    /// is not supported by tree-sitter.
    ///
    /// Note: This does not borrow `ParserPool`. The parser is borrowed only during
    /// `parse_source`, allowing the pool to be used for injection processing.
    pub fn for_file(filename: &str, theme_name: &str) -> Self {
        let ext = std::path::Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        if let Some(supported_lang) = SupportedLanguage::from_extension(ext) {
            let theme = get_theme(theme_name);
            let style_cache = ThemeStyleCache::new(theme);
            return Highlighter::Cst {
                supported_lang,
                style_cache,
            };
        }

        if let Some(syntax) = syntax_for_file(filename) {
            let theme = get_theme(theme_name);
            return Highlighter::Syntect(HighlightLines::new(syntax, theme));
        }

        Highlighter::None
    }

    /// Parse the entire source and return a tree for line-by-line highlighting.
    ///
    /// For CST highlighter, parses the source and returns the tree.
    /// For Syntect, this is a no-op (syntect processes line by line).
    ///
    /// # Arguments
    /// * `source` - The source code to parse
    /// * `parser_pool` - The parser pool to borrow a parser from (borrowed only for this call)
    pub fn parse_source(
        &self,
        source: &str,
        parser_pool: &mut ParserPool,
    ) -> Option<CstParseResult> {
        match self {
            Highlighter::Cst {
                supported_lang,
                style_cache: _,
            } => {
                let parser = parser_pool.get_or_create(supported_lang.default_extension())?;
                let tree = parser.parse(source, None)?;
                Some(CstParseResult {
                    tree,
                    lang: *supported_lang,
                })
            }
            _ => None,
        }
    }

    /// Get a reference to the style cache for CST highlighting.
    ///
    /// Returns `None` for Syntect or None variants.
    pub fn style_cache(&self) -> Option<&ThemeStyleCache> {
        match self {
            Highlighter::Cst { style_cache, .. } => Some(style_cache),
            _ => None,
        }
    }

    /// Highlight a single line of code.
    ///
    /// For CST, this should be called after parse_source() with the tree.
    /// For Syntect, this can be called directly.
    pub fn highlight_line(&mut self, line: &str, interner: &mut Rodeo) -> SpanVec {
        match self {
            Highlighter::Cst { .. } => {
                smallvec![InternedSpan {
                    content: interner.get_or_intern(line),
                    style: Style::default(),
                }]
            }
            Highlighter::Syntect(hl) => highlight_with_syntect(line, hl, interner),
            Highlighter::None => {
                smallvec![InternedSpan {
                    content: interner.get_or_intern(line),
                    style: Style::default(),
                }]
            }
        }
    }
}

/// Collect all highlights from the tree in a single pass.
///
/// This runs the query once over the entire tree and groups captures by line,
/// avoiding the O(N * tree_size) cost of querying per-line.
///
/// # Arguments
/// * `source` - The complete source code
/// * `tree` - The parsed tree
/// * `query` - The highlight query
/// * `capture_names` - The capture names from the query
/// * `style_cache` - Pre-computed style cache from the theme
///
/// # Returns
/// A `LineHighlights` struct with captures grouped by source line index.
pub fn collect_line_highlights(
    source: &str,
    tree: &Tree,
    query: &Query,
    capture_names: &[String],
    style_cache: &ThemeStyleCache,
) -> LineHighlights {
    collect_line_highlights_bounded(
        source,
        tree,
        query,
        capture_names,
        style_cache,
        &OpenHighlight,
    )
    .unwrap_or_else(|_| LineHighlights::empty())
}

/// [`collect_line_highlights`] with a cancel/time budget.
///
/// `Err` means the captures collected so far were discarded. Callers keep the
/// plain diff instead of applying a partial highlight.
pub fn collect_line_highlights_bounded(
    source: &str,
    tree: &Tree,
    query: &Query,
    capture_names: &[String],
    style_cache: &ThemeStyleCache,
    control: &dyn HighlightControl,
) -> Result<LineHighlights, HighlightAbort> {
    let line_offsets = line_offsets(source);
    let mut captures_by_line = HashMap::new();
    query_highlights(
        QuerySource {
            text: source,
            origin: 0,
            line_offsets: &line_offsets,
            source_len: source.len(),
        },
        tree,
        query,
        capture_names,
        style_cache,
        &mut captures_by_line,
        control,
    )?;

    for captures in captures_by_line.values_mut() {
        captures.sort_by_key(|c| c.local_start);
    }

    Ok(LineHighlights { captures_by_line })
}

fn line_offsets(source: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(source.bytes().enumerate().filter_map(
            |(i, b)| {
                if b == b'\n' {
                    Some(i + 1)
                } else {
                    None
                }
            },
        ))
        .collect()
}

/// One query's text, mapped back onto the parent source.
///
/// `origin` is added to each node's byte offset before the line lookup, so an
/// injection query over a substring still addresses the parent source.
struct QuerySource<'a> {
    text: &'a str,
    origin: usize,
    line_offsets: &'a [usize],
    source_len: usize,
}

/// Which cursor strategy `query_highlights` runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryPlan {
    /// One cursor over the whole tree. Identical to the historical path.
    Whole,
    /// One cursor per depth slab. Used only for a deep chain, where a single
    /// cursor is superlinear and each slab stays small.
    Slabs,
}

#[cfg(test)]
thread_local! {
    static FORCED_PLAN: Cell<Option<QueryPlan>> = const { Cell::new(None) };
    static RAW_TRACE: std::cell::RefCell<Option<Vec<RawCapture>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawCapture {
    pattern: usize,
    index: u32,
    start: usize,
    end: usize,
    finish: usize,
    depth: u32,
}

#[cfg(test)]
thread_local! {
    static TRACE_PATTERN: Cell<usize> = const { Cell::new(usize::MAX) };
    static TRACE_FINISH: Cell<usize> = const { Cell::new(usize::MAX) };
    static TRACE_DEPTH: Cell<u32> = const { Cell::new(u32::MAX) };
}

#[cfg(test)]
struct ForcePlan(Option<QueryPlan>);

#[cfg(test)]
impl ForcePlan {
    fn whole() -> Self {
        Self::set(Some(QueryPlan::Whole))
    }

    fn slabs() -> Self {
        Self::set(Some(QueryPlan::Slabs))
    }

    fn set(plan: Option<QueryPlan>) -> Self {
        let prev = FORCED_PLAN.with(|cell| cell.replace(plan));
        Self(prev)
    }
}

#[cfg(test)]
impl Drop for ForcePlan {
    fn drop(&mut self) {
        FORCED_PLAN.with(|cell| cell.set(self.0));
    }
}

struct CaptureSink<'a> {
    origin: usize,
    line_offsets: &'a [usize],
    source_len: usize,
    capture_names: &'a [String],
    style_cache: &'a ThemeStyleCache,
    out: &'a mut HashMap<usize, Vec<LineCapture>>,
}

impl CaptureSink<'_> {
    fn record(&mut self, captures: &[QueryCapture<'_>]) {
        for capture in captures {
            self.record_span(
                capture.index,
                self.origin + capture.node.start_byte(),
                self.origin + capture.node.end_byte(),
            );
        }
    }

    fn record_span(&mut self, index: u32, start_byte: usize, end_byte: usize) {
        #[cfg(test)]
        RAW_TRACE.with(|trace| {
            if let Some(trace) = trace.borrow_mut().as_mut() {
                trace.push(RawCapture {
                    pattern: TRACE_PATTERN.with(Cell::get),
                    index,
                    start: start_byte,
                    end: end_byte,
                    finish: TRACE_FINISH.with(Cell::get),
                    depth: TRACE_DEPTH.with(Cell::get),
                });
            }
        });
        let start_line = self
            .line_offsets
            .binary_search(&start_byte)
            .unwrap_or_else(|i| i.saturating_sub(1));
        let end_line = self
            .line_offsets
            .binary_search(&end_byte)
            .unwrap_or_else(|i| i.saturating_sub(1));

        let capture_name = &self.capture_names[index as usize];
        let style = self.style_cache.get(capture_name);
        if style == Style::default() {
            return;
        }

        for line_idx in start_line..=end_line {
            let line_start = self.line_offsets.get(line_idx).copied().unwrap_or(0);
            let line_end = self
                .line_offsets
                .get(line_idx + 1)
                .map(|&off| off.saturating_sub(1))
                .unwrap_or(self.source_len);

            let local_start = start_byte.saturating_sub(line_start);
            let local_end = end_byte
                .saturating_sub(line_start)
                .min(line_end - line_start);
            if local_start < local_end {
                self.out.entry(line_idx).or_default().push(LineCapture {
                    local_start,
                    local_end,
                    style,
                });
            }
        }
    }
}

/// `true` when the heaviest spine is a deep chain.
///
/// Slab queries used to keep every match until the merge. A narrow chain a
/// million nodes deep made that arena larger than the tree. Finalized hits
/// are spilled so only the open ancestor frames stay resident. A bushy tree
/// (many nodes per level) still spends more than the whole-tree cursor, and
/// stays on that cursor.
/// Descendant count is an upper bound on depth: a subtree that cannot reach
/// the required spine is skipped. The walk follows only the heaviest child.
fn chain_deep_enough(tree: &Tree, control: &dyn HighlightControl) -> Result<bool, HighlightAbort> {
    let nodes = tree.root_node().descendant_count();
    let per_level = DEEP_TREE_MAX_NODES_PER_LEVEL.max(1);
    let ratio_depth = u32::try_from(nodes.div_ceil(per_level)).unwrap_or(u32::MAX);
    let needed = DEEP_TREE_MIN_DEPTH.saturating_add(1).max(ratio_depth);
    if needed == u32::MAX {
        return Ok(false);
    }

    let mut cursor = tree.walk();
    let mut depth = 0u32;
    let mut steps = 0u32;
    loop {
        if depth >= needed {
            return Ok(true);
        }
        let here = cursor.node().descendant_count();
        let extra = u32::try_from(here.saturating_sub(1)).unwrap_or(u32::MAX);
        if depth.saturating_add(extra) < needed {
            return Ok(false);
        }
        // A million siblings cannot hide a deeper chain than `here - children`:
        // every sibling is at least one node. Bail before walking that list.
        let children = cursor.node().child_count();
        if children > DEEP_TREE_MAX_FANOUT {
            return Ok(false);
        }
        if children > 1 {
            let heaviest = u32::try_from(here.saturating_sub(children)).unwrap_or(u32::MAX);
            if depth.saturating_add(heaviest) < needed {
                return Ok(false);
            }
        }
        steps += 1;
        if steps.is_multiple_of(4096) {
            if let Some(reason) = control.poll_abort() {
                return Err(reason);
            }
        }
        if !cursor.goto_first_child() {
            return Ok(false);
        }
        depth += 1;
        let mut best_count = cursor.node().descendant_count();
        let mut index = 0u32;
        let mut best_index = 0u32;
        while cursor.goto_next_sibling() {
            index = index.saturating_add(1);
            steps += 1;
            if steps.is_multiple_of(4096) {
                if let Some(reason) = control.poll_abort() {
                    return Err(reason);
                }
            }
            let count = cursor.node().descendant_count();
            if count > best_count {
                best_count = count;
                best_index = index;
            }
        }
        let back = index.saturating_sub(best_index);
        for _ in 0..back {
            if !cursor.goto_previous_sibling() {
                break;
            }
        }
    }
}

/// `true` when `node` has a visible descendant deeper than `limit` levels down.
fn node_deeper_than(node: Node<'_>, limit: u32) -> bool {
    if node.descendant_count()
        <= usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    {
        return false;
    }
    let mut cursor = node.walk();
    let mut depth = 0u32;
    loop {
        if depth > limit {
            return true;
        }
        let extra = cursor.node().descendant_count().saturating_sub(1);
        let room = usize::try_from(limit.saturating_sub(depth)).unwrap_or(0);
        if extra > room && cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if depth == 0 {
                return false;
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return false;
            }
            depth -= 1;
        }
    }
}

/// The finish-byte read matches this build of tree-sitter.
///
/// `false` forces the whole-tree cursor. The probe runs once per process.
fn finish_layout_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(probe_finish_layout)
}

fn probe_finish_layout() -> bool {
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return false;
    }
    let source = "fn main() { let value = 1; }";
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    let Ok(query) = Query::new(&language, "(identifier) @name") else {
        return false;
    };
    let cursor = QueryCursor::new();
    let raw = QueryCursor::into_raw(cursor);
    // SAFETY: `into_raw` yields the non-null cursor and does not free it.
    // `from_raw` takes ownership back, so the cursor is dropped once.
    let mut cursor = unsafe { QueryCursor::from_raw(raw) };
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut saw_match = false;
    while let Some(mat) = matches.next() {
        let Some(capture) = mat.captures.first() else {
            return false;
        };
        if cursor_finish_byte(raw) != capture.node.start_byte() {
            return false;
        }
        saw_match = true;
    }
    saw_match
}

/// Capture sequence `record_span` sees, before the rendered-span sort.
#[cfg(test)]
fn trace_raw_captures(
    source: &str,
    tree: &Tree,
    query: &Query,
    capture_names: &[String],
    style_cache: &ThemeStyleCache,
) -> Vec<RawCapture> {
    RAW_TRACE.with(|trace| trace.replace(Some(Vec::new())));
    let result = collect_line_highlights_bounded(
        source,
        tree,
        query,
        capture_names,
        style_cache,
        &OpenHighlight,
    );
    let caps = RAW_TRACE
        .with(|trace| trace.replace(None))
        .unwrap_or_default();
    result.expect("trace highlight");
    caps
}

/// Visible depth of `tree`, counting the same nodes [`Tree::walk`] visits.
#[cfg(test)]
fn visible_tree_depth(tree: &Tree) -> u32 {
    let mut cursor = tree.walk();
    let mut depth = 0u32;
    let mut max_depth = 0u32;
    loop {
        max_depth = max_depth.max(depth);
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return max_depth;
            }
            depth = depth.saturating_sub(1);
        }
    }
}

fn select_plan(tree: &Tree, control: &dyn HighlightControl) -> Result<QueryPlan, HighlightAbort> {
    #[cfg(test)]
    {
        if let Some(plan) = FORCED_PLAN.with(|cell| cell.get()) {
            return Ok(plan);
        }
    }
    if !chain_deep_enough(tree, control)? || !finish_layout_ok() {
        return Ok(QueryPlan::Whole);
    }
    Ok(QueryPlan::Slabs)
}

#[cfg(test)]
fn selected_query_plan(tree: &Tree) -> Result<QueryPlan, HighlightAbort> {
    select_plan(tree, &OpenHighlight)
}

/// Run one highlight query, recording captures into `out`.
///
/// The progress callback is the point where a `spawn_blocking` highlight can
/// actually stop. `QueryCursor::matches` otherwise runs to completion.
/// Byte-range splitting was measured slower on the generated Haskell input
/// and was not capture-identical. Depth slabs are used only for a deep chain.
/// Bushy and ordinary trees take the original whole-tree cursor.
fn query_highlights(
    source: QuerySource<'_>,
    tree: &Tree,
    query: &Query,
    capture_names: &[String],
    style_cache: &ThemeStyleCache,
    out: &mut HashMap<usize, Vec<LineCapture>>,
    control: &dyn HighlightControl,
) -> Result<(), HighlightAbort> {
    let QuerySource {
        text,
        origin,
        line_offsets,
        source_len,
    } = source;
    if let Some(reason) = control.poll_abort() {
        return Err(reason);
    }
    let plan = select_plan(tree, control)?;
    let mut sink = CaptureSink {
        origin,
        line_offsets,
        source_len,
        capture_names,
        style_cache,
        out,
    };
    match plan {
        QueryPlan::Whole => query_whole(text, tree, query, &mut sink, control),
        QueryPlan::Slabs => query_slabs(text, tree, query, &mut sink, control),
    }
}

fn query_whole(
    text: &str,
    tree: &Tree,
    query: &Query,
    sink: &mut CaptureSink<'_>,
    control: &dyn HighlightControl,
) -> Result<(), HighlightAbort> {
    let mut cursor = QueryCursor::new();
    let match_limit = control.match_limit();
    if let Some(limit) = match_limit {
        cursor.set_match_limit(limit);
    }
    let aborted = Cell::new(None);
    let mut callback = |_state: &QueryCursorState| {
        if let Some(reason) = control.poll_abort() {
            aborted.set(Some(reason));
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = QueryCursorOptions::new().progress_callback(&mut callback);
    #[cfg(test)]
    let raw = QueryCursor::into_raw(cursor);
    // SAFETY: `into_raw` does not free the cursor. The rebuilt wrapper owns
    // it, and `raw` is only read while `matches` borrows that wrapper.
    // Test-only: production highlights do not need the finish node.
    #[cfg(test)]
    let mut cursor = unsafe { QueryCursor::from_raw(raw) };
    let mut matches =
        cursor.matches_with_options(query, tree.root_node(), text.as_bytes(), options);
    while let Some(mat) = matches.next() {
        if aborted.get().is_some() {
            break;
        }
        #[cfg(test)]
        {
            TRACE_PATTERN.with(|pattern| pattern.set(mat.pattern_index));
            TRACE_FINISH.with(|finish| finish.set(cursor_finish_byte(raw)));
            TRACE_DEPTH.with(|depth| depth.set(0));
        }
        sink.record(mat.captures);
    }
    drop(matches);

    if let Some(reason) = aborted.get() {
        return Err(reason);
    }
    if match_limit.is_some() && cursor.did_exceed_match_limit() {
        return Err(HighlightAbort::MatchLimit);
    }
    if let Some(reason) = control.poll_abort() {
        return Err(reason);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Cap {
    index: u32,
    start: u32,
    end: u32,
}

#[derive(Clone, Copy)]
struct SlabHit {
    pattern: u32,
    finish: u32,
    cap_at: u32,
    cap_len: u32,
}

/// One depth-bounded query. Hits live in a shared arena (`hit_at`/`hit_len`)
/// so a million slabs do not each allocate.
struct Slab {
    depth: u32,
    hit_at: u32,
    hit_len: u32,
}

fn query_slabs(
    text: &str,
    tree: &Tree,
    query: &Query,
    sink: &mut CaptureSink<'_>,
    control: &dyn HighlightControl,
) -> Result<(), HighlightAbort> {
    if u32::try_from(text.len()).is_err() || spill_unusable() {
        return query_whole(text, tree, query, sink, control);
    }
    let nested = super::parser_pool::nested_pattern_flags(query);
    match stream_slab_matches(text, tree, query, control) {
        Ok(stream) => expand_slab_stream(stream, nested, sink, control),
        // The tree is still alive. The whole-tree cursor renders the same
        // captures; it is the path ordinary files already use.
        Err(SlabQueryError::Spill) => {
            note_spill_unusable();
            query_whole(text, tree, query, sink, control)
        }
        Err(SlabQueryError::Abort(reason)) => Err(reason),
    }
}

/// Start a depth-slab query without building the line map.
///
/// `None` means the caller should run the whole-tree cursor. `Some` is a
/// spill of the deduped matches. The tree is only borrowed while this runs;
/// expanding the spill does not need it, so the caller can drop the tree
/// before the highlight output is allocated.
pub(crate) fn begin_slab_highlight(
    text: &str,
    tree: &Tree,
    query: &Query,
    control: &dyn HighlightControl,
) -> Result<Option<SlabStream>, HighlightAbort> {
    // Each viewed file gets its own attempt. A previous file's full disk
    // should not permanently disable the bounded path.
    SPILL_UNUSABLE.with(|flag| flag.set(false));
    if !matches!(select_plan(tree, control)?, QueryPlan::Slabs) {
        return Ok(None);
    }
    if u32::try_from(text.len()).is_err() || spill_unusable() {
        return Ok(None);
    }
    match stream_slab_matches(text, tree, query, control) {
        Ok(stream) => Ok(Some(stream)),
        Err(SlabQueryError::Spill) => {
            note_spill_unusable();
            Ok(None)
        }
        Err(SlabQueryError::Abort(reason)) => Err(reason),
    }
}

/// Turn a spilled slab query into per-line captures.
///
/// Safe to call after the syntax tree has been dropped.
pub(crate) fn finish_slab_highlight(
    stream: SlabStream,
    source: &str,
    query: &Query,
    capture_names: &[String],
    style_cache: &ThemeStyleCache,
    control: &dyn HighlightControl,
) -> Result<LineHighlights, HighlightAbort> {
    let line_offsets = line_offsets(source);
    let mut captures_by_line = HashMap::new();
    let nested = super::parser_pool::nested_pattern_flags(query);
    let mut sink = CaptureSink {
        origin: 0,
        line_offsets: &line_offsets,
        source_len: source.len(),
        capture_names,
        style_cache,
        out: &mut captures_by_line,
    };
    expand_slab_stream(stream, nested, &mut sink, control)?;
    for captures in captures_by_line.values_mut() {
        captures.sort_by_key(|capture| capture.local_start);
    }
    Ok(LineHighlights { captures_by_line })
}

/// Return freed heap pages to the OS.
///
/// The slab spill is written while the tree is still alive. Dropping the
/// tree leaves a hole in the glibc heap; `malloc_trim` releases it before
/// the highlight output is built, so the two never share one high-water mark.
pub(crate) fn release_highlight_heap() {
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: `malloc_trim` is process-global and only releases free
        // chunks. The tree that filled those chunks has already been dropped.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// Deduped slab matches, appended as they are discovered.
///
/// Records live in a temporary file so a multi-million-node chain does not
/// keep every capture next to the syntax tree. [`Drop`] removes the file.
pub(crate) struct SlabStream {
    /// Buffered handle. The short tail page is written here; bulk bytes are not.
    buffered: File,
    /// `O_DIRECT` handle when the filesystem allows it. Bulk spill bytes never
    /// enter the page cache, which is what kept a deep highlight under the
    /// cgroup cap.
    direct: Option<File>,
    path: PathBuf,
    /// Byte offset of each slab record, in preorder. The index is the merge group.
    offsets: Vec<u32>,
    /// Packed drop bits for every hit, in query order. One byte per eight hits.
    drops: Vec<u8>,
    len: u64,
    /// Page-aligned prefix already dropped from the page cache.
    released: u64,
}

impl Drop for SlabStream {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
struct SlabBudget {
    max_loaded: usize,
    total_hits: usize,
}

#[cfg(test)]
thread_local! {
    static SLAB_BUDGET: std::cell::RefCell<SlabBudget> = const { std::cell::RefCell::new(SlabBudget { max_loaded: 0, total_hits: 0 }) };
}

#[cfg(test)]
fn reset_slab_budget() {
    SLAB_BUDGET.with(|budget| *budget.borrow_mut() = SlabBudget::default());
}

#[cfg(test)]
fn note_slab_loaded(loaded: usize, added: usize) {
    SLAB_BUDGET.with(|budget| {
        let mut budget = budget.borrow_mut();
        budget.total_hits = budget.total_hits.saturating_add(added);
        budget.max_loaded = budget.max_loaded.max(loaded);
    });
}

/// `(max hits loaded for one parent/child dedup, total hits queried)`.
#[cfg(test)]
pub(super) fn slab_resident_budget() -> (usize, usize) {
    SLAB_BUDGET.with(|budget| {
        let budget = budget.borrow();
        (budget.max_loaded, budget.total_hits)
    })
}

struct LiveSlab {
    hits: Vec<SlabHit>,
    caps: Vec<Cap>,
    hashes: Vec<u128>,
    drop_hit: Vec<bool>,
}

struct SlabOut<'a> {
    caps: &'a mut Vec<Cap>,
    hits: &'a mut Vec<SlabHit>,
    hashes: &'a mut Vec<u128>,
}

struct ReusedQuery<'a> {
    cursor: &'a mut QueryCursor,
    raw: *mut tree_sitter::ffi::TSQueryCursor,
    enforce_match_limit: bool,
}

fn query_one_slab<'tree>(
    bytes: &[u8],
    root: Node<'tree>,
    query: &Query,
    reused: &mut ReusedQuery<'_>,
    out: &mut SlabOut<'_>,
    control: &dyn HighlightControl,
) -> Result<u32, HighlightAbort> {
    // Capping a subtree that is already shallower than the stride changes
    // the order of matches that finish on the same node, and saves nothing.
    // Reset the depth on every slab: the cursor is reused, and a cap left
    // over from a deeper sibling would reorder a shallow one.
    if node_deeper_than(root, QUERY_DEPTH_STRIDE) {
        reused.cursor.set_max_start_depth(Some(QUERY_DEPTH_STRIDE));
    } else {
        reused.cursor.set_max_start_depth(None);
    }
    let aborted = Cell::new(None);
    let mut callback = |_state: &QueryCursorState| {
        if let Some(reason) = control.poll_abort() {
            aborted.set(Some(reason));
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = QueryCursorOptions::new().progress_callback(&mut callback);
    let mut matches = reused
        .cursor
        .matches_with_options(query, root, bytes, options);
    let before = out.hits.len();
    while let Some(mat) = matches.next() {
        if aborted.get().is_some() {
            break;
        }
        if mat.captures.is_empty() {
            continue;
        }
        let Ok(finish) = u32::try_from(cursor_finish_byte(reused.raw)) else {
            continue;
        };
        let Ok(cap_at) = u32::try_from(out.caps.len()) else {
            continue;
        };
        let key_hash = match_key_hash(mat.pattern_index, mat.captures);
        for capture in mat.captures {
            let Ok(start) = u32::try_from(capture.node.start_byte()) else {
                continue;
            };
            let Ok(end) = u32::try_from(capture.node.end_byte()) else {
                continue;
            };
            out.caps.push(Cap {
                index: capture.index,
                start,
                end,
            });
        }
        let cap_len = u32::try_from(out.caps.len() - cap_at as usize).unwrap_or(0);
        if cap_len == 0 {
            continue;
        }
        let pattern = u32::try_from(mat.pattern_index).unwrap_or(u32::MAX);
        out.hashes.push(key_hash);
        out.hits.push(SlabHit {
            pattern,
            finish,
            cap_at,
            cap_len,
        });
    }
    drop(matches);
    if let Some(reason) = aborted.get() {
        return Err(reason);
    }
    if reused.enforce_match_limit && reused.cursor.did_exceed_match_limit() {
        return Err(HighlightAbort::MatchLimit);
    }
    u32::try_from(out.hits.len() - before).or(Err(HighlightAbort::MatchLimit))
}

#[derive(Clone, Copy)]
struct SlabFrame {
    /// Index of this slab's first drop bit in the in-memory bitset.
    bit_at: u32,
    hit_count: u32,
    /// File offset of the append-only hash list.
    hash_off: u32,
}

/// Deletes the spill unless the walk transferred it to a [`SlabStream`].
struct SpillCleanup {
    path: PathBuf,
    armed: bool,
}

impl Drop for SpillCleanup {
    fn drop(&mut self) {
        if self.armed && !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct SpillFile {
    buffered: File,
    direct: Option<File>,
    cleanup: SpillCleanup,
    /// Bytes not flushed yet. `tail_off` is the file offset of `tail[0]`.
    tail: Vec<u8>,
    tail_off: u64,
    end: u64,
    offsets: Vec<u32>,
    /// Packed drop bits. Kept in memory so a parent update does not rewrite a
    /// spilled page back into the cache. Eight million hits is about 1 MB.
    drops: Vec<u8>,
    drop_hits: u32,
}

fn stream_slab_matches(
    text: &str,
    tree: &Tree,
    query: &Query,
    control: &dyn HighlightControl,
) -> Result<SlabStream, SlabQueryError> {
    #[cfg(test)]
    reset_slab_budget();
    // Parsing the deep tree leaves freed scratch above the live nodes.
    // Give those pages back before the slab walk allocates its own buffers.
    release_highlight_heap();
    let mut spill = SpillFile::create()?;
    // Slab frames only. The navigation stack is one window per stride, not a
    // tree cursor: a cursor on this chain keeps an ancestor entry per level
    // and that alone was ~120 MB on top of the tree.
    let mut frames: Vec<SlabFrame> = Vec::new();
    let bytes = text.as_bytes();
    let match_limit = control.match_limit();
    let mut steps = 0u32;
    let mut query_cursor = QueryCursor::new();
    if let Some(limit) = match_limit {
        query_cursor.set_match_limit(limit);
    }
    let raw = QueryCursor::into_raw(query_cursor);
    // SAFETY: `into_raw` does not free the cursor. This wrapper owns it for
    // the rest of the walk, and `raw` is only read while `matches` borrows it.
    let mut query_cursor = unsafe { QueryCursor::from_raw(raw) };
    let mut wins = vec![StrideWin::new(tree.root_node(), 0)];

    loop {
        steps += 1;
        if steps.is_multiple_of(4096) {
            if let Some(reason) = control.poll_abort() {
                return Err(reason.into());
            }
        }
        let Some(win) = wins.last() else {
            debug_assert!(frames.is_empty());
            return spill.into_stream();
        };
        if !win.queried {
            let node = win.node;
            let depth = win.depth;
            wins.last_mut().expect("window").queried = true;
            let mut walk = SlabWalk {
                cursor: &mut query_cursor,
                raw,
                enforce_match_limit: match_limit.is_some(),
                spill: &mut spill,
                frames: &mut frames,
            };
            commit_slab(bytes, node, depth, query, &mut walk, control)?;
            continue;
        }

        let action = {
            let win = wins.last_mut().expect("window");
            let local = stride_local_node(win);
            let next = win.down[win.levels as usize];
            let count = u32::try_from(local.child_count()).unwrap_or(u32::MAX);
            if next < count {
                if let Some(child) = local.child(next) {
                    win.down[win.levels as usize] = next + 1;
                    if u32::from(win.levels) + 1 == QUERY_DEPTH_STRIDE {
                        Some(child)
                    } else {
                        win.levels += 1;
                        win.down[win.levels as usize] = 0;
                        None
                    }
                } else {
                    win.down[win.levels as usize] = next + 1;
                    None
                }
            } else if win.levels == 0 {
                // Finished. `None` child and we signal by emptying below.
                wins.pop();
                frames.pop();
                continue;
            } else {
                win.levels -= 1;
                None
            }
        };
        if let Some(child) = action {
            let depth = wins.last().expect("parent window").depth + QUERY_DEPTH_STRIDE;
            wins.push(StrideWin::new(child, depth));
        }
    }
}

/// One stride node plus the at-most-`QUERY_DEPTH_STRIDE` levels under it.
///
/// Deeper nodes become their own window, so this never stores the whole spine.
struct StrideWin<'tree> {
    node: Node<'tree>,
    depth: u32,
    queried: bool,
    /// Next child index to try at each relative depth.
    down: [u32; QUERY_DEPTH_STRIDE as usize],
    levels: u8,
}

impl<'tree> StrideWin<'tree> {
    fn new(node: Node<'tree>, depth: u32) -> Self {
        Self {
            node,
            depth,
            queried: false,
            down: [0; QUERY_DEPTH_STRIDE as usize],
            levels: 0,
        }
    }
}

fn stride_local_node<'a>(win: &StrideWin<'a>) -> Node<'a> {
    let mut node = win.node;
    for level in 0..win.levels {
        let index = win.down[level as usize] - 1;
        node = node
            .child(index)
            .expect("child index came from child_count");
    }
    node
}

struct SlabWalk<'a> {
    cursor: &'a mut QueryCursor,
    raw: *mut tree_sitter::ffi::TSQueryCursor,
    enforce_match_limit: bool,
    spill: &'a mut SpillFile,
    frames: &'a mut Vec<SlabFrame>,
}

fn commit_slab(
    bytes: &[u8],
    node: Node<'_>,
    depth: u32,
    query: &Query,
    walk: &mut SlabWalk<'_>,
    control: &dyn HighlightControl,
) -> Result<(), SlabQueryError> {
    let mut live = LiveSlab {
        hits: Vec::new(),
        caps: Vec::new(),
        hashes: Vec::new(),
        drop_hit: Vec::new(),
    };
    {
        let mut out = SlabOut {
            caps: &mut live.caps,
            hits: &mut live.hits,
            hashes: &mut live.hashes,
        };
        query_one_slab(
            bytes,
            node,
            query,
            &mut ReusedQuery {
                cursor: walk.cursor,
                raw: walk.raw,
                enforce_match_limit: walk.enforce_match_limit,
            },
            &mut out,
            control,
        )?;
    }
    live.drop_hit.resize(live.hits.len(), false);
    let child_hits = live.hits.len();
    let loaded = if let Some(parent) = walk.frames.last().copied() {
        let parent_hashes = walk.spill.read_hashes(parent.hash_off, parent.hit_count)?;
        let mut parent_drop = walk.spill.parent_drops(parent.bit_at, parent.hit_count);
        mark_overlap(
            &parent_hashes,
            &mut parent_drop,
            &live.hashes,
            &mut live.drop_hit,
        );
        walk.spill.store_drops(parent.bit_at, &parent_drop);
        parent.hit_count as usize + child_hits
    } else {
        child_hits
    };
    #[cfg(test)]
    note_slab_loaded(loaded, child_hits);
    #[cfg(not(test))]
    let _ = loaded;
    let frame = walk.spill.write_record(depth, &live)?;
    walk.frames.push(frame);
    Ok(())
}

fn expand_slab_stream(
    mut stream: SlabStream,
    nested: &[u8],
    sink: &mut CaptureSink<'_>,
    control: &dyn HighlightControl,
) -> Result<(), HighlightAbort> {
    let mut slabs = Vec::with_capacity(stream.offsets.len());
    let mut hits = Vec::new();
    let mut caps = Vec::new();
    let count = stream.offsets.len();
    for index in 0..count {
        let offset = stream.offsets[index];
        // Earlier records are fully consumed. Drop their pages before the
        // expanded arena grows, and keep this record's page so the read hits cache.
        stream.release_prefix(u64::from(offset));
        let (depth, kept_hits, kept_caps) = stream.read_kept(offset)?;
        let hit_at = u32::try_from(hits.len()).unwrap_or(u32::MAX);
        let mut hit_len = 0u32;
        for hit in kept_hits {
            let start = hit.cap_at as usize;
            let end = start + hit.cap_len as usize;
            let new_at = u32::try_from(caps.len()).unwrap_or(u32::MAX);
            if end > kept_caps.len() || new_at == u32::MAX {
                continue;
            }
            caps.extend_from_slice(&kept_caps[start..end]);
            hits.push(SlabHit {
                pattern: hit.pattern,
                finish: hit.finish,
                cap_at: new_at,
                cap_len: hit.cap_len,
            });
            hit_len = hit_len.saturating_add(1);
        }
        slabs.push(Slab {
            depth,
            hit_at,
            hit_len,
        });
    }
    stream.release_rest();
    emit_merged(&slabs, &hits, &caps, nested, sink, control)?;
    if let Some(reason) = control.poll_abort() {
        return Err(reason);
    }
    Ok(())
}

impl SpillFile {
    fn create() -> Result<Self, SlabQueryError> {
        #[cfg(test)]
        if spill_forced_to_fail() {
            return Err(SlabQueryError::Spill);
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!("octorus-slab-{}-{id}", std::process::id()));
        let buffered = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(spill_err)?;
        let direct = open_direct(&path);
        Ok(Self {
            buffered,
            direct,
            cleanup: SpillCleanup { path, armed: true },
            tail: Vec::with_capacity(128 * 1024),
            tail_off: 0,
            end: 0,
            offsets: Vec::new(),
            drops: Vec::new(),
            drop_hits: 0,
        })
    }

    fn into_stream(mut self) -> Result<SlabStream, SlabQueryError> {
        self.flush_all()?;
        self.cleanup.armed = false;
        let path = std::mem::take(&mut self.cleanup.path);
        Ok(SlabStream {
            buffered: self.buffered,
            direct: self.direct.take(),
            path,
            offsets: std::mem::take(&mut self.offsets),
            drops: std::mem::take(&mut self.drops),
            len: self.end,
            released: 0,
        })
    }

    fn write_record(&mut self, depth: u32, live: &LiveSlab) -> Result<SlabFrame, SlabQueryError> {
        let hit_count = u32::try_from(live.hits.len())
            .map_err(|_| SlabQueryError::Abort(HighlightAbort::MatchLimit))?;
        let cap_count = u32::try_from(live.caps.len())
            .map_err(|_| SlabQueryError::Abort(HighlightAbort::MatchLimit))?;
        let offset = u32::try_from(self.end)
            .map_err(|_| SlabQueryError::Abort(HighlightAbort::MatchLimit))?;
        let bit_at = self.drop_hits;
        if bit_at.checked_add(hit_count).is_none() {
            return Err(SlabQueryError::Abort(HighlightAbort::MatchLimit));
        }
        let mut buf = Vec::with_capacity(
            16 + live.hits.len() * 16 + live.caps.len() * 12 + live.hashes.len() * 16,
        );
        push_u32(&mut buf, depth);
        push_u32(&mut buf, hit_count);
        push_u32(&mut buf, cap_count);
        push_u32(&mut buf, bit_at);
        for hit in &live.hits {
            push_u32(&mut buf, hit.pattern);
            push_u32(&mut buf, hit.finish);
            push_u32(&mut buf, hit.cap_at);
            push_u32(&mut buf, hit.cap_len);
        }
        for cap in &live.caps {
            push_u32(&mut buf, cap.index);
            push_u32(&mut buf, cap.start);
            push_u32(&mut buf, cap.end);
        }
        let hash_off = u32::try_from(self.end + buf.len() as u64)
            .map_err(|_| SlabQueryError::Abort(HighlightAbort::MatchLimit))?;
        for hash in &live.hashes {
            buf.extend_from_slice(&hash.to_le_bytes());
        }
        self.append(&buf)?;
        self.push_drops(&live.drop_hit);
        self.offsets.push(offset);
        Ok(SlabFrame {
            bit_at,
            hit_count,
            hash_off,
        })
    }

    fn read_hashes(&mut self, hash_off: u32, hits: u32) -> Result<Vec<u128>, SlabQueryError> {
        if hits == 0 {
            return Ok(Vec::new());
        }
        let mut raw = vec![0u8; hits as usize * 16];
        self.read_at(u64::from(hash_off), &mut raw)?;
        Ok(raw
            .chunks_exact(16)
            .map(|chunk| u128::from_le_bytes(chunk.try_into().expect("16-byte hash")))
            .collect())
    }

    fn parent_drops(&self, bit_at: u32, hits: u32) -> Vec<bool> {
        let mut drops = vec![false; hits as usize];
        for (index, drop) in drops.iter_mut().enumerate() {
            let at = bit_at as usize + index;
            let byte = self.drops.get(at / 8).copied().unwrap_or(0);
            *drop = byte & (1 << (at % 8)) != 0;
        }
        drops
    }

    fn store_drops(&mut self, bit_at: u32, drops: &[bool]) {
        for (index, drop) in drops.iter().enumerate() {
            let at = bit_at as usize + index;
            if at / 8 >= self.drops.len() {
                break;
            }
            let mask = 1u8 << (at % 8);
            if *drop {
                self.drops[at / 8] |= mask;
            } else {
                self.drops[at / 8] &= !mask;
            }
        }
    }

    fn push_drops(&mut self, drops: &[bool]) {
        let start = self.drop_hits as usize;
        let need = (start + drops.len()).div_ceil(8);
        if self.drops.len() < need {
            self.drops.resize(need, 0);
        }
        for (index, drop) in drops.iter().enumerate() {
            if *drop {
                let at = start + index;
                self.drops[at / 8] |= 1 << (at % 8);
            }
        }
        self.drop_hits = self
            .drop_hits
            .saturating_add(u32::try_from(drops.len()).unwrap_or(u32::MAX));
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), SlabQueryError> {
        self.tail.extend_from_slice(bytes);
        self.end += bytes.len() as u64;
        self.flush_full_pages()
    }

    /// Move complete pages to disk. A small tail stays in this process so the
    /// parent slab's hashes are usually still a memory read.
    fn flush_full_pages(&mut self) -> Result<(), SlabQueryError> {
        let page = page_size() as usize;
        const KEEP: usize = 64 * 1024;
        if self.tail.len() < KEEP + page {
            return Ok(());
        }
        let n = self.tail.len() & !(page - 1);
        self.write_pages(n)
    }

    fn flush_all(&mut self) -> Result<(), SlabQueryError> {
        let page = page_size() as usize;
        let n = self.tail.len() & !(page - 1);
        if n > 0 {
            self.write_pages(n)?;
        }
        if self.tail.is_empty() {
            return Ok(());
        }
        // The last partial page cannot go through `O_DIRECT`.
        self.buffered
            .seek(SeekFrom::Start(self.tail_off))
            .map_err(spill_err)?;
        self.buffered.write_all(&self.tail).map_err(spill_err)?;
        self.tail_off += self.tail.len() as u64;
        self.tail.clear();
        let _ = self.buffered.sync_data();
        advise_whole_file(&self.buffered);
        Ok(())
    }

    fn write_pages(&mut self, n: usize) -> Result<(), SlabQueryError> {
        if n == 0 {
            return Ok(());
        }
        if let Some(direct) = self.direct.as_ref() {
            let aligned = align_copy(&self.tail[..n]);
            pwrite_all(direct, self.tail_off, aligned.as_slice())?;
        } else {
            self.buffered
                .seek(SeekFrom::Start(self.tail_off))
                .map_err(spill_err)?;
            self.buffered
                .write_all(&self.tail[..n])
                .map_err(spill_err)?;
            let _ = sync_and_drop_cache(&self.buffered, self.tail_off, n as u64);
        }
        self.tail.drain(..n);
        self.tail_off += n as u64;
        Ok(())
    }

    fn read_at(&mut self, offset: u64, dst: &mut [u8]) -> Result<(), SlabQueryError> {
        if dst.is_empty() {
            return Ok(());
        }
        if offset >= self.tail_off {
            let start = (offset - self.tail_off) as usize;
            let end = start + dst.len();
            if end > self.tail.len() {
                return Err(SlabQueryError::Abort(HighlightAbort::MatchLimit));
            }
            dst.copy_from_slice(&self.tail[start..end]);
            return Ok(());
        }
        if offset + dst.len() as u64 <= self.tail_off {
            return read_disk(
                self.direct.as_ref(),
                &self.buffered,
                self.tail_off,
                offset,
                dst,
            );
        }
        let n = (self.tail_off - offset) as usize;
        let tail_len = dst.len() - n;
        read_disk(
            self.direct.as_ref(),
            &self.buffered,
            self.tail_off,
            offset,
            &mut dst[..n],
        )?;
        dst[n..].copy_from_slice(&self.tail[..tail_len]);
        Ok(())
    }
}

impl SlabStream {
    fn read_kept(&mut self, offset: u32) -> Result<(u32, Vec<SlabHit>, Vec<Cap>), HighlightAbort> {
        let mut hdr = [0u8; 16];
        self.read_at(u64::from(offset), &mut hdr)?;
        let depth = u32::from_le_bytes(hdr[0..4].try_into().expect("depth"));
        let hit_count = u32::from_le_bytes(hdr[4..8].try_into().expect("hits")) as usize;
        let cap_count = u32::from_le_bytes(hdr[8..12].try_into().expect("caps")) as usize;
        let bit_at = u32::from_le_bytes(hdr[12..16].try_into().expect("bits")) as usize;
        let mut body = vec![0u8; hit_count * 16 + cap_count * 12];
        self.read_at(u64::from(offset) + 16, &mut body)?;
        let mut hits = Vec::with_capacity(hit_count);
        for chunk in body[..hit_count * 16].chunks_exact(16) {
            hits.push(SlabHit {
                pattern: u32::from_le_bytes(chunk[0..4].try_into().expect("pattern")),
                finish: u32::from_le_bytes(chunk[4..8].try_into().expect("finish")),
                cap_at: u32::from_le_bytes(chunk[8..12].try_into().expect("cap_at")),
                cap_len: u32::from_le_bytes(chunk[12..16].try_into().expect("cap_len")),
            });
        }
        let mut caps = Vec::with_capacity(cap_count);
        for chunk in body[hit_count * 16..].chunks_exact(12) {
            caps.push(Cap {
                index: u32::from_le_bytes(chunk[0..4].try_into().expect("index")),
                start: u32::from_le_bytes(chunk[4..8].try_into().expect("start")),
                end: u32::from_le_bytes(chunk[8..12].try_into().expect("end")),
            });
        }
        let kept = hits
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !self.bit_dropped(bit_at + index))
            .map(|(_, hit)| hit)
            .collect();
        Ok((depth, kept, caps))
    }

    fn bit_dropped(&self, index: usize) -> bool {
        let byte = self.drops.get(index / 8).copied().unwrap_or(0);
        byte & (1 << (index % 8)) != 0
    }

    fn read_at(&mut self, offset: u64, dst: &mut [u8]) -> Result<(), HighlightAbort> {
        read_disk(self.direct.as_ref(), &self.buffered, self.len, offset, dst).map_err(|err| {
            match err {
                SlabQueryError::Abort(reason) => reason,
                SlabQueryError::Spill => HighlightAbort::MatchLimit,
            }
        })
    }

    /// Drop pages fully before `consumed_until`. The live record may share
    /// its first page with the previous one, so that page stays.
    fn release_prefix(&mut self, consumed_until: u64) {
        let until = consumed_until & !(page_size() - 1);
        if until <= self.released {
            return;
        }
        advise_dontneed(&self.buffered, self.released, until - self.released);
        self.released = until;
    }

    fn release_rest(&mut self) {
        advise_whole_file(&self.buffered);
        self.released = u64::MAX;
    }
}

struct AlignedBytes {
    raw: Vec<u8>,
    off: usize,
    len: usize,
}

impl AlignedBytes {
    fn as_slice(&self) -> &[u8] {
        &self.raw[self.off..self.off + self.len]
    }
}

/// Copy `src` into a page-aligned allocation.
///
/// `O_DIRECT` rejects a buffer whose address is not a multiple of the page
/// size. `Vec<u8>` is only aligned to 1, so the bytes are copied forward to
/// the next aligned address in the same allocation.
fn align_copy(src: &[u8]) -> AlignedBytes {
    let page = page_size() as usize;
    let mut raw = vec![0u8; src.len() + page];
    let addr = raw.as_mut_ptr() as usize;
    let start = (addr + page - 1) & !(page - 1);
    let off = start - addr;
    raw[off..off + src.len()].copy_from_slice(src);
    AlignedBytes {
        raw,
        off,
        len: src.len(),
    }
}

fn open_direct(path: &std::path::Path) -> Option<File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // `O_DIRECT`. Bulk spill writes then skip the page cache.
        File::options()
            .read(true)
            .write(true)
            .custom_flags(0x4000)
            .open(path)
            .ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

fn pwrite_all(file: &File, offset: u64, bytes: &[u8]) -> Result<(), SlabQueryError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(bytes, offset).map_err(spill_err)
    }
    #[cfg(not(unix))]
    {
        let _ = (file, offset, bytes);
        Err(SlabQueryError::Spill)
    }
}

fn pread_all(file: &File, offset: u64, dst: &mut [u8]) -> Result<(), SlabQueryError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(dst, offset).map_err(spill_err)
    }
    #[cfg(not(unix))]
    {
        let _ = (file, offset, dst);
        Err(SlabQueryError::Spill)
    }
}

fn pread_direct(file: &File, offset: u64, dst: &mut [u8]) -> Result<(), SlabQueryError> {
    let page = page_size();
    let start = offset & !(page - 1);
    let end = (offset + dst.len() as u64 + page - 1) & !(page - 1);
    let len = (end - start) as usize;
    let mut raw = vec![0u8; len + page as usize];
    let addr = raw.as_mut_ptr() as usize;
    let aligned = (addr + page as usize - 1) & !(page as usize - 1);
    let off = aligned - addr;
    pread_all(file, start, &mut raw[off..off + len])?;
    let src = (offset - start) as usize;
    dst.copy_from_slice(&raw[off + src..off + src + dst.len()]);
    Ok(())
}

/// Read `dst` from the spill.
///
/// `file_len` is the flushed size. Whole pages were written with `O_DIRECT`
/// and are read the same way. A short tail, and a filesystem that rejected
/// `O_DIRECT`, go through the buffered handle and then drop those pages.
fn read_disk(
    direct: Option<&File>,
    buffered: &File,
    file_len: u64,
    offset: u64,
    dst: &mut [u8],
) -> Result<(), SlabQueryError> {
    if dst.is_empty() {
        return Ok(());
    }
    let page = page_size();
    let direct_end = file_len & !(page - 1);
    if let Some(direct) = direct {
        if offset + dst.len() as u64 <= direct_end {
            return pread_direct(direct, offset, dst);
        }
        if offset < direct_end {
            let n = (direct_end - offset) as usize;
            pread_direct(direct, offset, &mut dst[..n])?;
            pread_all(buffered, offset + n as u64, &mut dst[n..])?;
            advise_dontneed(
                buffered,
                direct_end,
                (offset + dst.len() as u64) - direct_end,
            );
            return Ok(());
        }
    }
    pread_all(buffered, offset, dst)?;
    let start = offset & !(page - 1);
    let end = (offset + dst.len() as u64 + page - 1) & !(page - 1);
    advise_dontneed(buffered, start, end.saturating_sub(start));
    Ok(())
}

fn push_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

/// A spill that cannot be written or read aborts the highlight. The caller
/// keeps the plain diff.
fn spill_err(_err: std::io::Error) -> SlabQueryError {
    SlabQueryError::Spill
}

/// Exclusive end of spill bytes that can leave the page cache.
///
/// The record that starts at `last_record` is still the parent's match list.
/// Anything from its first page onward stays resident.
#[cfg(test)]
fn spill_drop_end(last_record: u64, already: u64) -> u64 {
    let until = last_record & !(page_size() - 1);
    if until <= already {
        0
    } else {
        until
    }
}

fn page_size() -> u64 {
    static PAGE: OnceLock<u64> = OnceLock::new();
    *PAGE.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            extern "C" {
                fn sysconf(name: i32) -> i64;
            }
            // `_SC_PAGESIZE` on Linux.
            const SC_PAGESIZE: i32 = 30;
            // SAFETY: `sysconf` is process-global and only reads a kernel constant.
            let n = unsafe { sysconf(SC_PAGESIZE) };
            if n > 0 {
                n as u64
            } else {
                4096
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            4096
        }
    })
}

/// Flush and drop a page-aligned range.
///
/// `sync_file_range` returns `EINVAL` unless `offset` and `nbytes` are both
/// multiples of the page size (or `nbytes` runs to EOF). Slab records are not
/// page-aligned, and a failed flush leaves the pages dirty, which
/// `POSIX_FADV_DONTNEED` will not drop. Callers pass the live record's start;
/// this rounds inward so the live page stays.
fn sync_and_drop_cache(file: &File, start: u64, len: u64) -> bool {
    let page = page_size();
    let aligned_start = start.saturating_add(page - 1) & !(page - 1);
    let end = start.saturating_add(len);
    let aligned_end = end & !(page - 1);
    if aligned_end <= aligned_start {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let len = aligned_end - aligned_start;
        // WAIT_BEFORE | WRITE | WAIT_AFTER. WAIT_AFTER makes the pages clean
        // before the advice below, which ignores dirty pages.
        const SYNC_ALL: u32 = 1 | 2 | 4;
        let synced = sync_file_range_range(file, aligned_start, len, SYNC_ALL);
        let dropped = advise_dontneed(file, aligned_start, len);
        synced && dropped
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        true
    }
}

fn sync_file_range_range(file: &File, offset: u64, nbytes: u64, flags: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        extern "C" {
            fn sync_file_range(fd: i32, offset: i64, nbytes: i64, flags: u32) -> i32;
        }
        let fd = file.as_raw_fd();
        // SAFETY: `fd` belongs to `file`. The range covers bytes this process wrote.
        unsafe { sync_file_range(fd, offset as i64, nbytes as i64, flags) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, offset, nbytes, flags);
        true
    }
}

fn advise_whole_file(file: &File) {
    #[cfg(target_os = "linux")]
    {
        // A zero length advises every byte from the offset to EOF, including
        // the last partial page.
        raw_fadvise(file, 0, 0);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
    }
}

/// Drop clean page-cache pages. A partial page at either end is kept.
fn advise_dontneed(file: &File, start: u64, len: u64) -> bool {
    let page = page_size();
    let aligned_start = start.saturating_add(page - 1) & !(page - 1);
    let end = start.saturating_add(len);
    let aligned_end = end & !(page - 1);
    if aligned_end <= aligned_start {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        raw_fadvise(file, aligned_start, aligned_end - aligned_start)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        true
    }
}

fn raw_fadvise(file: &File, offset: u64, len: u64) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        extern "C" {
            fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
        }
        // POSIX_FADV_DONTNEED. The spill is on a real filesystem (`/tmp` here
        // is ext4), so this drops page cache and the bytes stay on disk.
        // The libc wrapper returns a positive errno, not -1.
        const DONTNEED: i32 = 4;
        let fd = file.as_raw_fd();
        // SAFETY: `fd` belongs to `file`. Advice does not change the file.
        unsafe { posix_fadvise(fd, offset as i64, len as i64, DONTNEED) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, offset, len);
        true
    }
}

/// Mark copies of one match that a parent slab and one child slab both saw.
///
/// Counts come from the original hit list, including hits an earlier child
/// already marked. A higher count wins; a tie keeps the shallower parent.
/// Every hit of the losing hash is marked, which is the buffered dedup.
fn mark_overlap(
    parent_hashes: &[u128],
    parent_drop: &mut [bool],
    child_hashes: &[u128],
    child_drop: &mut [bool],
) {
    if parent_hashes.is_empty() || child_hashes.is_empty() {
        return;
    }
    let mut parent_counts: FxHashMap<u128, u32> = FxHashMap::default();
    for hash in parent_hashes {
        *parent_counts.entry(*hash).or_default() += 1;
    }
    let mut child_counts: FxHashMap<u128, u32> = FxHashMap::default();
    for hash in child_hashes {
        if parent_counts.contains_key(hash) {
            *child_counts.entry(*hash).or_default() += 1;
        }
    }
    let mut drop_from_parent: FxHashMap<u128, ()> = FxHashMap::default();
    for (offset, hash) in child_hashes.iter().enumerate() {
        let Some(&child_count) = child_counts.get(hash) else {
            continue;
        };
        if parent_counts[hash] >= child_count {
            child_drop[offset] = true;
        } else {
            drop_from_parent.insert(*hash, ());
        }
    }
    if drop_from_parent.is_empty() {
        return;
    }
    for (offset, hash) in parent_hashes.iter().enumerate() {
        if drop_from_parent.contains_key(hash) {
            parent_drop[offset] = true;
        }
    }
}

fn match_key_hash(pattern: usize, captures: &[QueryCapture<'_>]) -> u128 {
    let mut low = FxHasher::with_seed(0x517c_c1b7);
    let mut high = FxHasher::with_seed(0x9e37_79b9);
    pattern.hash(&mut low);
    pattern.hash(&mut high);
    for capture in captures {
        capture.index.hash(&mut low);
        capture.index.hash(&mut high);
        capture.node.id().hash(&mut low);
        capture.node.id().hash(&mut high);
    }
    (u128::from(high.finish()) << 64) | u128::from(low.finish())
}

/// Two matches share a range when any capture covers the same bytes.
///
/// A signature pattern captures both the signature and the following name.
/// The name is the range it shares with `(decl name: (variable) @function)`.
fn same_capture_range(caps: &[Cap], left: &SlabHit, right: &SlabHit) -> bool {
    if left.finish != right.finish {
        return false;
    }
    let left_caps = &caps[left.cap_at as usize..(left.cap_at + left.cap_len) as usize];
    let right_caps = &caps[right.cap_at as usize..(right.cap_at + right.cap_len) as usize];
    // One pattern can capture the shared name and a larger sibling. The
    // ranges intersect even when the capture lists are not identical.
    left_caps.iter().any(|lhs| {
        right_caps
            .iter()
            .any(|rhs| lhs.start == rhs.start && lhs.end == rhs.end)
    })
}

fn capture_depth(nested: &[u8], pattern: u32) -> u8 {
    nested.get(pattern as usize).copied().unwrap_or(0)
}

/// Put a cross-slab pair back into whole-cursor order.
///
/// One cursor already emits the order the whole tree uses. The merge sees
/// the shallower slab first when both copies finish on the same byte, which
/// puts `(decl/bind name: (variable) @variable)` ahead of
/// `(decl name: (variable) @function)`. The deeper `@` is the one the
/// uncapped cursor emitted first. Matches from the same slab stay put:
/// `max_start_depth` is not applied to a subtree that is already shallow,
/// and reordering those matches changes Python builtins and Zig fields.
fn slab_hit<'a>(slabs: &[Slab], hits: &'a [SlabHit], group: u32, index: u32) -> &'a SlabHit {
    let slab = &slabs[group as usize];
    &hits[slab.hit_at as usize + index as usize]
}

fn order_nested_pairs(
    order: &mut [(u32, u32)],
    slabs: &[Slab],
    hits: &[SlabHit],
    caps: &[Cap],
    nested: &[u8],
) {
    if order.len() < 2 || !nested.iter().any(|depth| *depth > 0) {
        return;
    }
    for _ in 0..8 {
        let mut swapped = false;
        for i in 0..order.len() - 1 {
            if slabs[order[i].0 as usize].depth == slabs[order[i + 1].0 as usize].depth {
                continue;
            }
            let left = slab_hit(slabs, hits, order[i].0, order[i].1);
            let right = slab_hit(slabs, hits, order[i + 1].0, order[i + 1].1);
            let left_depth = capture_depth(nested, left.pattern);
            let right_depth = capture_depth(nested, right.pattern);
            if left_depth == 0
                || right_depth == 0
                || left_depth >= right_depth
                || !same_capture_range(caps, left, right)
            {
                continue;
            }
            order.swap(i, i + 1);
            swapped = true;
        }
        if !swapped {
            break;
        }
    }
}

fn emit_merged(
    slabs: &[Slab],
    hits: &[SlabHit],
    caps: &[Cap],
    nested: &[u8],
    sink: &mut CaptureSink<'_>,
    control: &dyn HighlightControl,
) -> Result<(), HighlightAbort> {
    let mut heap = BinaryHeap::new();
    for (group, slab) in slabs.iter().enumerate() {
        if slab.hit_len > 0 {
            let hit = slab_hit(slabs, hits, group as u32, 0);
            heap.push(MergeHead::new(hit, slab.depth, group as u32, 0));
        }
    }
    let mut order = Vec::new();
    while let Some(head) = heap.pop() {
        let group = head.group as usize;
        let index = head.index as usize;
        order.push((head.group, head.index));
        let next = index + 1;
        if next < slabs[group].hit_len as usize {
            let hit = slab_hit(slabs, hits, head.group, next as u32);
            heap.push(MergeHead::new(
                hit,
                slabs[group].depth,
                head.group,
                u32::try_from(next).unwrap_or(u32::MAX),
            ));
        }
    }
    order_nested_pairs(&mut order, slabs, hits, caps, nested);
    for (emitted, (group, index)) in order.into_iter().enumerate() {
        if emitted.is_multiple_of(4096) {
            if let Some(reason) = control.poll_abort() {
                return Err(reason);
            }
        }
        let hit = slab_hit(slabs, hits, group, index);
        #[cfg(test)]
        {
            TRACE_PATTERN.with(|pattern| pattern.set(hit.pattern as usize));
            TRACE_FINISH.with(|finish| finish.set(hit.finish as usize));
            TRACE_DEPTH.with(|depth| depth.set(slabs[group as usize].depth));
        }
        let start = hit.cap_at as usize;
        let end = start + hit.cap_len as usize;
        for cap in &caps[start..end] {
            sink.record_span(
                cap.index,
                sink.origin + cap.start as usize,
                sink.origin + cap.end as usize,
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct MergeHead {
    finish: u32,
    depth: u32,
    group: u32,
    index: u32,
}

impl MergeHead {
    fn new(hit: &SlabHit, depth: u32, group: u32, index: u32) -> Self {
        Self {
            finish: hit.finish,
            depth,
            group,
            index,
        }
    }
}

impl PartialEq for MergeHead {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MergeHead {}

impl PartialOrd for MergeHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MergeHead {
    fn cmp(&self, other: &Self) -> Ordering {
        // `BinaryHeap` pops the greatest item. Reverse the key so the next
        // whole-cursor match (smaller finish, then shallower slab) comes out
        // first. `group` and `index` only break ties.
        (other.finish, other.depth, other.group, other.index).cmp(&(
            self.finish,
            self.depth,
            self.group,
            self.index,
        ))
    }
}

/// Start byte of the node the query cursor is on.
///
/// tree-sitter 0.26.11 lays `TSQueryCursor` out as the query pointer followed
/// by `TSTreeCursor`. `ts_query_cursor_next_match` returns while that cursor
/// is still on the node where the pattern finished. No public accessor
/// exposes that node: `QueryCursorState` only has `current_byte_offset`, and
/// the progress callback does not run when a match is yielded.
/// [`finish_layout_ok`] compares this read with a known capture and
/// [`select_plan`] keeps the whole-tree cursor when they disagree.
fn cursor_finish_byte(cursor: *mut tree_sitter::ffi::TSQueryCursor) -> usize {
    // SAFETY: `cursor` came from `QueryCursor::into_raw` and the rebuilt
    // wrapper still owns it. The byte at `size_of::<usize>()` is the tree
    // cursor that follows the query pointer. The probe rejects a build where
    // that offset is not the finish node.
    unsafe {
        let tree_cursor = cursor
            .cast::<u8>()
            .add(std::mem::size_of::<usize>())
            .cast::<tree_sitter::ffi::TSTreeCursor>();
        tree_sitter::ffi::ts_node_start_byte(tree_sitter::ffi::ts_tree_cursor_current_node(
            tree_cursor,
        )) as usize
    }
}

/// Collect highlights with injection support for languages like Svelte.
///
/// This extends `collect_line_highlights` to handle embedded languages:
/// - Parses the parent language (e.g., Svelte)
/// - Extracts injection ranges using the language's injection query
/// - Highlights each injection range with the appropriate parser
/// - Merges all highlights into a single `LineHighlights`
///
/// # Arguments
/// * `source` - The complete source code
/// * `tree` - The parsed tree from the parent parser
/// * `query` - The highlight query for the parent language
/// * `capture_names` - Capture names from the parent query
/// * `style_cache` - Style cache for theme colors
/// * `parser_pool` - Parser pool for creating injection parsers and cached queries
/// * `parent_ext` - File extension of the parent language (e.g., "svelte")
pub fn collect_line_highlights_with_injections(
    source: &str,
    tree: &Tree,
    lang: SupportedLanguage,
    style_cache: &ThemeStyleCache,
    parser_pool: &mut ParserPool,
    parent_ext: &str,
) -> LineHighlights {
    collect_line_highlights_with_injections_bounded(
        source,
        tree,
        lang,
        style_cache,
        parser_pool,
        parent_ext,
        &OpenHighlight,
    )
    .unwrap_or_else(|_| LineHighlights::empty())
}

/// [`collect_line_highlights_with_injections`] with a cancel/time budget.
///
/// `Err` discards parent and injection captures alike.
pub fn collect_line_highlights_with_injections_bounded(
    source: &str,
    tree: &Tree,
    lang: SupportedLanguage,
    style_cache: &ThemeStyleCache,
    parser_pool: &mut ParserPool,
    parent_ext: &str,
    control: &dyn HighlightControl,
) -> Result<LineHighlights, HighlightAbort> {
    use crate::syntax::injection::{extract_injections, normalize_language_name};

    let query = match parser_pool.get_or_create_query(lang) {
        Some(q) => q,
        None => return Ok(LineHighlights::empty()),
    };
    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut result =
        collect_line_highlights_bounded(source, tree, query, &capture_names, style_cache, control)?;

    let parent_lang = match SupportedLanguage::from_extension(parent_ext) {
        Some(lang) => lang,
        None => return Ok(result),
    };

    let injection_query = match parent_ext {
        "svelte" => tree_sitter_svelte_ng::INJECTIONS_QUERY,
        "vue" => tree_sitter_vue3::INJECTIONS_QUERY,
        "md" | "markdown" => tree_sitter_md::INJECTION_QUERY_BLOCK,
        _ => return Ok(result), // No injection support for other languages yet
    };

    let ts_language = parent_lang.ts_language();
    let injections = extract_injections(tree, source.as_bytes(), &ts_language, injection_query);

    if injections.is_empty() {
        return Ok(result);
    }

    let line_offsets = line_offsets(source);

    for injection in injections {
        let mut normalized_lang = normalize_language_name(&injection.language);

        // Svelte/Vue injection query marks all raw_text as "javascript" by default,
        // but <style> content should be CSS. Use the parent node kind from the
        // syntax tree to determine the correct language.
        if normalized_lang == "javascript" && (parent_ext == "svelte" || parent_ext == "vue") {
            if let Some(ref parent_kind) = injection.parent_node_kind {
                // Check if this injection is inside a style element
                if parent_kind.contains("style") {
                    normalized_lang = "css";
                }
                // script_element keeps "javascript" (or typescript if lang attr is set)
            }
        }

        let ext = match normalized_lang {
            "typescript" => "ts",
            "javascript" => "js",
            "tsx" => "tsx",
            "jsx" => "jsx",
            "css" => "css",
            "markdown_inline" => "md_inline",
            "rust" => "rs",
            "python" => "py",
            "go" => "go",
            "ruby" => "rb",
            "c" => "c",
            "cpp" => "cpp",
            "java" => "java",
            "lua" => "lua",
            "bash" => "sh",
            "php" => "php",
            "swift" => "swift",
            "haskell" => "hs",
            "zig" => "zig",
            "moonbit" => "mbt",
            "html" => continue, // Skip HTML injections (handled by parent)
            _ => continue,      // Skip unsupported languages
        };

        let inj_source = &source[injection.range.clone()];

        let Some(inj_lang) = SupportedLanguage::from_extension(ext) else {
            continue;
        };

        let inj_tree = match parser_pool.get_or_create(ext) {
            Some(parser) => match parser.parse(inj_source, None) {
                Some(tree) => tree,
                None => continue,
            },
            None => continue,
        };
        let Some(inj_query) = parser_pool.get_or_create_query(inj_lang) else {
            continue;
        };

        let inj_capture_names: Vec<String> = inj_query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        if let Some(reason) = control.poll_abort() {
            return Err(reason);
        }
        query_highlights(
            QuerySource {
                text: inj_source,
                origin: injection.range.start,
                line_offsets: &line_offsets,
                source_len: source.len(),
            },
            &inj_tree,
            inj_query,
            &inj_capture_names,
            style_cache,
            &mut result.captures_by_line,
            control,
        )?;
    }

    // Re-sort captures within each line by start position
    // For captures starting at the same position, longer captures come first so that
    // shorter (more specific) captures can override them when we process in order
    for captures in result.captures_by_line.values_mut() {
        captures.sort_by(|a, b| {
            a.local_start.cmp(&b.local_start).then_with(|| {
                // Sort by length descending (longer first) so shorter captures
                // are processed later and override longer ones
                (b.local_end - b.local_start).cmp(&(a.local_end - a.local_start))
            })
        });
    }

    Ok(result)
}

/// Apply pre-computed highlights to a line, producing InternedSpans.
///
/// When captures overlap, the more specific (shorter) capture takes precedence.
/// This allows injection highlights to override parent language highlights.
///
/// # Arguments
/// * `line` - The line content
/// * `captures` - Pre-computed captures for this line (from `collect_line_highlights`)
/// * `interner` - String interner for deduplication
pub fn apply_line_highlights(
    line: &str,
    captures: Option<&[LineCapture]>,
    interner: &mut Rodeo,
) -> SpanVec {
    let captures = match captures {
        Some(c) if !c.is_empty() => c,
        _ => {
            return smallvec![InternedSpan {
                content: interner.get_or_intern(line),
                style: Style::default(),
            }];
        }
    };

    // O(m log m) where m is the number of captures, rather than O(n) where n is line length.
    // For long lines (e.g., minified code), this is much more efficient.
    let mut events: Vec<(usize, bool, usize)> = Vec::with_capacity(captures.len() * 2);

    for (idx, capture) in captures.iter().enumerate() {
        if capture.local_start >= capture.local_end || capture.local_end > line.len() {
            continue;
        }
        events.push((capture.local_start, true, idx)); // start event
        events.push((capture.local_end, false, idx)); // end event
    }

    if events.is_empty() {
        return smallvec![InternedSpan {
            content: interner.get_or_intern(line),
            style: Style::default(),
        }];
    }

    events.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut spans = SpanVec::new();
    let mut active_captures: Vec<usize> = Vec::new();
    let mut last_pos = 0;

    for (pos, is_start, capture_idx) in events {
        if pos > last_pos {
            let style = active_captures
                .last()
                .map(|&idx| captures[idx].style)
                .unwrap_or_default();
            let text = &line[last_pos..pos];
            if !text.is_empty() {
                spans.push(InternedSpan {
                    content: interner.get_or_intern(text),
                    style,
                });
            }
        }

        if is_start {
            active_captures.push(capture_idx);
        } else if let Some(idx) = active_captures.iter().rposition(|&c| c == capture_idx) {
            active_captures.remove(idx);
        }

        last_pos = pos;
    }

    if last_pos < line.len() {
        let style = active_captures
            .last()
            .map(|&idx| captures[idx].style)
            .unwrap_or_default();
        let text = &line[last_pos..];
        if !text.is_empty() {
            spans.push(InternedSpan {
                content: interner.get_or_intern(text),
                style,
            });
        }
    }

    if spans.is_empty() {
        spans.push(InternedSpan {
            content: interner.get_or_intern(line),
            style: Style::default(),
        });
    }

    spans
}

/// Highlight a line using syntect.
fn highlight_with_syntect(
    line: &str,
    hl: &mut HighlightLines<'_>,
    interner: &mut Rodeo,
) -> SpanVec {
    match hl.highlight_line(line, syntax_set()) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(style, text)| InternedSpan {
                content: interner.get_or_intern(text),
                style: convert_syntect_style(&style),
            })
            .collect::<SpanVec>(),
        Err(_) => {
            smallvec![InternedSpan {
                content: interner.get_or_intern(line),
                style: Style::default(),
            }]
        }
    }
}

#[cfg(test)]
mod deep_tree;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_roundtrip_keeps_hits_and_applies_drop_bits() {
        let mut spill = SpillFile::create().expect("spill");
        let page = page_size() as usize;
        // Enough records to flush full pages, so the first one is read back
        // from disk rather than from the tail buffer.
        let mut first = None;
        for record in 0u32..40 {
            let hits: Vec<SlabHit> = (0u32..300)
                .map(|index| SlabHit {
                    pattern: record,
                    finish: index,
                    cap_at: index,
                    cap_len: 1,
                })
                .collect();
            let caps: Vec<Cap> = (0u32..300)
                .map(|index| Cap {
                    index,
                    start: index,
                    end: index + 1,
                })
                .collect();
            let hashes: Vec<u128> = (0u32..300)
                .map(|index| u128::from(record) << 32 | u128::from(index))
                .collect();
            let drop_hit = vec![false; 300];
            let live = LiveSlab {
                hits,
                caps,
                hashes,
                drop_hit,
            };
            let frame = spill.write_record(record, &live).expect("record");
            if record == 0 {
                first = Some(frame);
            }
        }
        let first = first.expect("first record");
        assert!(
            spill.tail_off > page as u64,
            "expected a flushed page, tail_off {}",
            spill.tail_off
        );
        let hashes = spill
            .read_hashes(first.hash_off, first.hit_count)
            .expect("hashes");
        assert_eq!(hashes.len(), 300);
        assert_eq!(hashes[0], 0);
        assert_eq!(hashes[299], 299);
        let mut parent_drop = spill.parent_drops(first.bit_at, first.hit_count);
        assert!(parent_drop.iter().all(|drop| !drop));
        parent_drop[1] = true;
        parent_drop[2] = true;
        spill.store_drops(first.bit_at, &parent_drop);
        let mut stream = spill.into_stream().expect("stream");
        let (depth, kept, caps) = stream.read_kept(stream.offsets[0]).expect("read");
        assert_eq!(depth, 0);
        assert_eq!(kept.len(), 298, "two hits were marked dropped");
        assert_eq!(kept[0].finish, 0);
        assert_eq!(kept[1].finish, 3);
        assert_eq!(caps.len(), 300);
        #[cfg(target_os = "linux")]
        assert!(
            stream.direct.is_some(),
            "O_DIRECT should open on a real filesystem"
        );
    }

    #[test]
    fn spill_drop_end_keeps_the_live_records_page() {
        let page = page_size();
        assert_eq!(spill_drop_end(0, 0), 0);
        assert_eq!(spill_drop_end(page - 1, 0), 0);
        assert_eq!(spill_drop_end(page, 0), page);
        assert_eq!(spill_drop_end(page + 100, 0), page);
        let chunk = 2 * 1024 * 1024;
        assert_eq!(spill_drop_end(chunk + 100, 0), chunk);
        assert_eq!(spill_drop_end(8 * page, page), 8 * page);
        assert_eq!(spill_drop_end(page, page), 0);
        assert_eq!(spill_drop_end(100, page), 0);
    }

    /// `sync_file_range` rejects an unaligned length, and dirty pages then
    /// survive `DONTNEED`. The helper rounds inward and the middle pages go.
    #[cfg(target_os = "linux")]
    #[test]
    fn unaligned_spill_range_still_drops_page_cache() {
        let page = page_size() as usize;
        let len = 8 * 1024 * 1024 + page;
        let path =
            std::env::temp_dir().join(format!("octorus-fadv-{}-{}", std::process::id(), page));
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("spill test file");
        let chunk = vec![1u8; page];
        for _ in 0..(len / page) {
            file.write_all(&chunk).expect("write");
        }
        let before = resident_pages(&file, len);
        // Start and end are intentionally off a page boundary.
        let dropped = sync_and_drop_cache(&file, 100, (len - page) as u64);
        let after = resident_pages(&file, len);
        drop(file);
        let _ = std::fs::remove_file(&path);
        assert!(dropped, "aligned flush was rejected");
        assert!(
            before > 100,
            "expected the written file to be cached, got {before} pages"
        );
        assert!(
            after.saturating_mul(10) < before,
            "fadvise left {after} pages cached of {before}"
        );
    }

    #[cfg(target_os = "linux")]
    fn resident_pages(file: &File, len: usize) -> usize {
        use std::os::unix::io::AsRawFd;
        extern "C" {
            fn mmap(
                addr: *mut u8,
                len: usize,
                prot: i32,
                flags: i32,
                fd: i32,
                offset: i64,
            ) -> *mut u8;
            fn munmap(addr: *mut u8, len: usize) -> i32;
            fn mincore(addr: *mut u8, len: usize, vec: *mut u8) -> i32;
        }
        let page = page_size() as usize;
        // SAFETY: the mapping is shared and read-only over `file`, and it is
        // unmapped before this function returns. `mincore` does not fault.
        let addr = unsafe { mmap(std::ptr::null_mut(), len, 1, 1, file.as_raw_fd(), 0) };
        assert_ne!(addr as isize, -1, "mmap");
        let mut vec = vec![0u8; len.div_ceil(page)];
        let rc = unsafe { mincore(addr, len, vec.as_mut_ptr()) };
        assert_eq!(rc, 0, "mincore");
        let resident = vec.iter().filter(|byte| *byte & 1 != 0).count();
        unsafe { munmap(addr, len) };
        resident
    }

    #[test]
    fn test_highlighter_rust() {
        let highlighter = Highlighter::for_file("test.rs", "base16-ocean.dark");
        assert!(
            matches!(highlighter, Highlighter::Cst { .. }),
            "Expected Cst highlighter for Rust"
        );
    }

    #[test]
    fn test_highlighter_typescript() {
        let highlighter = Highlighter::for_file("test.ts", "base16-ocean.dark");
        assert!(matches!(highlighter, Highlighter::Cst { .. }));
    }

    #[test]
    fn test_highlighter_vue_cst() {
        // Vue is now supported with tree-sitter (Phase 3c)
        let highlighter = Highlighter::for_file("test.vue", "base16-ocean.dark");
        assert!(
            matches!(highlighter, Highlighter::Cst { .. }),
            "Expected Cst highlighter for Vue"
        );
    }

    #[test]
    fn test_highlighter_yaml_fallback() {
        let highlighter = Highlighter::for_file("test.yaml", "base16-ocean.dark");
        assert!(matches!(highlighter, Highlighter::Syntect(_)));
    }

    #[test]
    fn test_highlighter_unknown() {
        let highlighter = Highlighter::for_file("test.unknown", "base16-ocean.dark");
        assert!(matches!(highlighter, Highlighter::None));
    }

    #[test]
    fn test_cst_parse_and_highlight() {
        use crate::syntax::get_theme;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.rs", "base16-ocean.dark");

        let source = "fn main() {\n    let x = 42;\n}";

        if let Some(result) = highlighter.parse_source(source, &mut pool) {
            // Get cached query
            let query = pool.get_or_create_query(result.lang).unwrap();
            let capture_names: Vec<String> = query
                .capture_names()
                .iter()
                .map(|s| s.to_string())
                .collect();

            // Create style cache from theme for testing
            let theme = get_theme("base16-ocean.dark");
            let style_cache = ThemeStyleCache::new(theme);
            let line_highlights =
                collect_line_highlights(source, &result.tree, query, &capture_names, &style_cache);

            let mut interner = Rodeo::default();
            let line = "fn main() {";
            let captures = line_highlights.get(0);
            let spans = apply_line_highlights(line, captures, &mut interner);

            // Should have multiple spans with different styles
            assert!(!spans.is_empty());

            // Check that "main" is highlighted as a function name
            let main_text = spans
                .iter()
                .find(|s| interner.resolve(&s.content) == "main");
            assert!(main_text.is_some(), "Should highlight 'main' function name");
        }
    }

    #[test]
    fn test_syntect_highlight() {
        let mut highlighter = Highlighter::for_file("test.vue", "base16-ocean.dark");

        let mut interner = Rodeo::default();
        let spans = highlighter.highlight_line("<template>", &mut interner);

        assert!(!spans.is_empty());
    }

    #[test]
    fn test_cst_with_dracula_theme() {
        use crate::syntax::get_theme;
        use crate::syntax::themes::ThemeStyleCache;
        use ratatui::style::Color;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.rs", "Dracula");

        let source = "fn main() {\n    let x = 42;\n}";

        // Parse source (borrows pool only for this call)
        let result = highlighter
            .parse_source(source, &mut pool)
            .expect("Should parse Rust source");

        // Get cached query
        let query = pool.get_or_create_query(result.lang).unwrap();
        let capture_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Create style cache from Dracula theme
        let theme = get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);

        let line_highlights =
            collect_line_highlights(source, &result.tree, query, &capture_names, &style_cache);

        let mut interner = Rodeo::default();
        let line = "fn main() {";
        let captures = line_highlights.get(0);
        let spans = apply_line_highlights(line, captures, &mut interner);

        // "fn" should have Dracula pink color (keyword)
        let fn_span = spans.iter().find(|s| interner.resolve(&s.content) == "fn");
        assert!(fn_span.is_some(), "Should have 'fn' span");

        let fn_style = fn_span.unwrap().style;
        // Dracula keyword color is Rgb(255, 121, 198) (pink)
        match fn_style.fg {
            Some(Color::Rgb(r, g, b)) => {
                // Dracula pink is approximately Rgb(255, 121, 198)
                assert!(
                    r > 200 && g < 200 && b > 150,
                    "Expected Dracula pink-ish color for 'fn', got Rgb({}, {}, {})",
                    r,
                    g,
                    b
                );
            }
            other => {
                panic!("Expected Rgb color for 'fn' keyword, got {:?}", other);
            }
        }
    }

    #[test]
    fn test_use_keyword_with_dracula_theme() {
        use crate::syntax::get_theme;
        use crate::syntax::themes::ThemeStyleCache;
        use ratatui::style::Color;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.rs", "Dracula");

        let source = "use std::collections::HashMap;\n\nfn main() {}";

        let result = highlighter
            .parse_source(source, &mut pool)
            .expect("Should parse Rust source");

        // Get cached query
        let query = pool.get_or_create_query(result.lang).unwrap();
        let capture_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let theme = get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);

        let line_highlights =
            collect_line_highlights(source, &result.tree, query, &capture_names, &style_cache);

        let mut interner = Rodeo::default();
        let line = "use std::collections::HashMap;";
        let captures = line_highlights.get(0);
        let spans = apply_line_highlights(line, captures, &mut interner);

        // "use" should have Dracula pink color (keyword)
        let use_span = spans.iter().find(|s| interner.resolve(&s.content) == "use");
        assert!(use_span.is_some(), "Should have 'use' span");

        let use_style = use_span.unwrap().style;
        match use_style.fg {
            Some(Color::Rgb(255, 121, 198)) => {}
            Some(Color::Rgb(r, g, b)) => {
                panic!(
                    "'use' has wrong color. Expected Rgb(255, 121, 198), got Rgb({}, {}, {})",
                    r, g, b
                );
            }
            other => {
                panic!("Expected Rgb color for 'use' keyword, got {:?}", other);
            }
        }
    }

    #[test]
    fn test_vue_primed_highlighting() {
        use syntect::easy::HighlightLines;
        use syntect::highlighting::Color;

        let tf_ss = two_face::syntax::extra_newlines();
        let syntax = tf_ss.find_syntax_by_extension("vue").unwrap();
        let theme = crate::syntax::get_theme("Dracula");

        // Test with priming (our fix)
        let mut hl = HighlightLines::new(syntax, theme);
        // Prime with virtual <script> tag
        let _ = hl.highlight_line("<script lang=\"ts\">\n", &tf_ss);

        // Now highlight code without actual <script> tag in diff
        let regions = hl
            .highlight_line("const onClickPageName = () => {\n", &tf_ss)
            .unwrap();

        // Find the "const" token
        let const_region = regions.iter().find(|(_, text)| *text == "const");
        assert!(const_region.is_some(), "Should find 'const' token");

        // Dracula cyan is approximately (139, 233, 253)
        let (style, _) = const_region.unwrap();
        let Color { r, g, b, .. } = style.foreground;
        assert!(
            r < 200 && g > 200 && b > 200,
            "const should be cyan-ish, got ({}, {}, {})",
            r,
            g,
            b
        );

        // Find the function name
        let func_region = regions.iter().find(|(_, text)| *text == "onClickPageName");
        assert!(func_region.is_some(), "Should find 'onClickPageName' token");

        // Dracula green is approximately (80, 250, 123)
        let (style, _) = func_region.unwrap();
        let Color { r, g, b, .. } = style.foreground;
        assert!(
            r < 150 && g > 200 && b < 200,
            "onClickPageName should be green-ish, got ({}, {}, {})",
            r,
            g,
            b
        );
    }

    #[test]
    fn test_typescript_function_highlighting() {
        use crate::syntax::get_theme;
        use crate::syntax::themes::ThemeStyleCache;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.ts", "Dracula");

        // Arrow function assignment - common pattern in Vue/React
        let source = "const onClickPageName = () => {\n  const rootDom = store.tree\n}";

        let result = highlighter
            .parse_source(source, &mut pool)
            .expect("Should parse TypeScript source");

        // Get cached query
        let query = pool.get_or_create_query(result.lang).unwrap();
        let capture_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let theme = get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);

        let line_highlights =
            collect_line_highlights(source, &result.tree, query, &capture_names, &style_cache);

        let mut interner = Rodeo::default();
        let line = "const onClickPageName = () => {";
        let captures = line_highlights.get(0);
        let spans = apply_line_highlights(line, captures, &mut interner);

        // "const" should be highlighted as keyword
        let const_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "const");
        assert!(const_span.is_some(), "Should have 'const' span");
        assert!(
            const_span.unwrap().style.fg.is_some(),
            "'const' should have foreground color"
        );

        // "onClickPageName" should be highlighted as function
        let func_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "onClickPageName");
        assert!(
            func_span.is_some(),
            "Should have 'onClickPageName' span (function name)"
        );
        assert!(
            func_span.unwrap().style.fg.is_some(),
            "'onClickPageName' should have foreground color (function)"
        );
    }

    #[test]
    fn test_svelte_uses_cst_highlighter() {
        let highlighter = Highlighter::for_file("test.svelte", "Dracula");
        assert!(
            matches!(highlighter, Highlighter::Cst { .. }),
            "Svelte should use CST highlighter"
        );
    }

    #[test]
    fn test_svelte_script_injection_typescript() {
        use crate::syntax::get_theme;
        use crate::syntax::themes::ThemeStyleCache;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.svelte", "Dracula");

        // Svelte file with TypeScript in <script>
        let source = r#"<script lang="ts">
    const count: number = 0;
    function increment() {
        count += 1;
    }
</script>

<button on:click={increment}>
    {count}
</button>"#;

        let result = highlighter
            .parse_source(source, &mut pool)
            .expect("Should parse Svelte source");

        let theme = get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);

        // Use injection-aware highlighting
        let line_highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            &style_cache,
            &mut pool,
            "svelte",
        );

        let mut interner = Rodeo::default();

        // Line 2: "    const count: number = 0;"
        // "const" should be highlighted as keyword (TypeScript injection)
        let line = "    const count: number = 0;";
        let captures = line_highlights.get(1); // Line index 1
        let spans = apply_line_highlights(line, captures, &mut interner);

        let const_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "const");
        assert!(
            const_span.is_some(),
            "Should find 'const' in script injection"
        );
        assert!(
            const_span.unwrap().style.fg.is_some(),
            "'const' should have syntax highlighting from TypeScript parser"
        );

        // "number" should be highlighted as type
        let number_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "number");
        assert!(
            number_span.is_some(),
            "Should find 'number' type in script injection"
        );
        assert!(
            number_span.unwrap().style.fg.is_some(),
            "'number' should have syntax highlighting as type"
        );
    }

    #[test]
    fn test_svelte_style_injection_css() {
        use crate::syntax::get_theme;
        use crate::syntax::themes::ThemeStyleCache;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.svelte", "Dracula");

        // Svelte file with CSS in <style>
        let source = r#"<script>
    let visible = true;
</script>

<style>
    .container {
        color: red;
        display: flex;
    }
</style>

<div class="container">Hello</div>"#;

        let result = highlighter
            .parse_source(source, &mut pool)
            .expect("Should parse Svelte source");

        let theme = get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);

        let line_highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            &style_cache,
            &mut pool,
            "svelte",
        );

        let mut interner = Rodeo::default();

        // Line 6: "    .container {"
        let line = "    .container {";
        let captures = line_highlights.get(5); // Line index 5
        let spans = apply_line_highlights(line, captures, &mut interner);

        // ".container" or "container" should be highlighted as CSS class selector
        let has_class_highlight = spans.iter().any(|s| {
            let text = interner.resolve(&s.content);
            (text == ".container" || text == "container") && s.style.fg.is_some()
        });
        assert!(
            has_class_highlight,
            "CSS class selector should be highlighted in style injection"
        );

        // Line 7: "        color: red;"
        let line = "        color: red;";
        let captures = line_highlights.get(6);
        let spans = apply_line_highlights(line, captures, &mut interner);

        // "color" should be highlighted as CSS property
        let color_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "color");
        assert!(
            color_span.is_some(),
            "Should find 'color' CSS property in style injection"
        );
        assert!(
            color_span.unwrap().style.fg.is_some(),
            "'color' should have syntax highlighting as CSS property"
        );
    }

    /// Test that script blocks containing `<style` substring are NOT misclassified as CSS.
    ///
    /// This is a regression test for the issue where raw string search (`rfind("<style")`)
    /// would incorrectly detect `<style` inside JavaScript code (e.g., template strings,
    /// comments, DOM manipulation) and apply CSS highlighting to the script block.
    #[test]
    fn test_svelte_script_with_style_substring_not_misclassified() {
        use crate::syntax::get_theme;
        use crate::syntax::themes::ThemeStyleCache;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.svelte", "Dracula");

        // Svelte file with script containing "<style" as a string literal
        // This should NOT be misclassified as CSS
        let source = r#"<script lang="ts">
    const template = `<style>body { color: red; }</style>`;
    const element = document.querySelector("<style");
    function addStyle() {
        const style = "<style>test</style>";
        return style;
    }
</script>

<style>
    .real-css { color: blue; }
</style>

<div>{template}</div>"#;

        let result = highlighter
            .parse_source(source, &mut pool)
            .expect("Should parse Svelte source");

        let theme = get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);

        let line_highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            &style_cache,
            &mut pool,
            "svelte",
        );

        let mut interner = Rodeo::default();

        // Line 2: "    const template = `<style>body { color: red; }</style>`;"
        // "const" should be highlighted as JS/TS keyword (NOT CSS)
        let line = "    const template = `<style>body { color: red; }</style>`;";
        let captures = line_highlights.get(1); // Line index 1
        let spans = apply_line_highlights(line, captures, &mut interner);

        let const_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "const");
        assert!(
            const_span.is_some(),
            "Should find 'const' in script block with <style substring"
        );
        assert!(
            const_span.unwrap().style.fg.is_some(),
            "'const' should be highlighted as keyword (TypeScript), not misclassified as CSS"
        );

        // Line 4: "    function addStyle() {"
        // "function" should be highlighted as JS/TS keyword
        let line = "    function addStyle() {";
        let captures = line_highlights.get(3); // Line index 3
        let spans = apply_line_highlights(line, captures, &mut interner);

        let function_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "function");
        assert!(
            function_span.is_some(),
            "Should find 'function' in script block"
        );
        assert!(
            function_span.unwrap().style.fg.is_some(),
            "'function' should be highlighted as keyword"
        );

        // Line 11: "    .real-css { color: blue; }"
        // This is actual CSS in a <style> block, should be highlighted as CSS
        let line = "    .real-css { color: blue; }";
        let captures = line_highlights.get(10); // Line index 10
        let spans = apply_line_highlights(line, captures, &mut interner);

        // "color" in the actual CSS block should be highlighted
        let color_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "color");
        assert!(
            color_span.is_some(),
            "Should find 'color' in actual CSS block"
        );
        assert!(
            color_span.unwrap().style.fg.is_some(),
            "'color' in real <style> block should be highlighted as CSS property"
        );
    }

    #[test]
    fn test_vue_script_injection_typescript() {
        use crate::syntax::get_theme;
        use crate::syntax::themes::ThemeStyleCache;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.vue", "Dracula");

        // Vue file with TypeScript in <script>
        let source = r#"<script lang="ts">
    const count: number = 0;
    function increment() {
        count += 1;
    }
</script>

<template>
    <button @click="increment">
        {{ count }}
    </button>
</template>"#;

        let result = highlighter
            .parse_source(source, &mut pool)
            .expect("Should parse Vue source");

        let theme = get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);

        // Use injection-aware highlighting
        let line_highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            &style_cache,
            &mut pool,
            "vue",
        );

        let mut interner = Rodeo::default();

        // Line 2: "    const count: number = 0;"
        // "const" should be highlighted as keyword (TypeScript injection)
        let line = "    const count: number = 0;";
        let captures = line_highlights.get(1);
        let spans = apply_line_highlights(line, captures, &mut interner);

        // "const" is part of "    const " span due to how captures overlap
        let const_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content).contains("const"));
        assert!(
            const_span.is_some(),
            "Should find span containing 'const' in TypeScript script block"
        );
        assert!(
            const_span.unwrap().style.fg.is_some(),
            "'const' should be highlighted as keyword in Vue TypeScript block"
        );
    }

    #[test]
    fn test_vue_style_injection_css() {
        use crate::syntax::get_theme;
        use crate::syntax::themes::ThemeStyleCache;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.vue", "Dracula");

        // Vue file with CSS in <style>
        let source = r#"<template>
    <div class="container">Hello</div>
</template>

<style>
    .container {
        color: red;
    }
</style>"#;

        let result = highlighter
            .parse_source(source, &mut pool)
            .expect("Should parse Vue source");

        let theme = get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);

        let line_highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            &style_cache,
            &mut pool,
            "vue",
        );

        let mut interner = Rodeo::default();

        // Line 7: "        color: red;"
        let line = "        color: red;";
        let captures = line_highlights.get(6);
        let spans = apply_line_highlights(line, captures, &mut interner);

        // "color" should be highlighted as CSS property
        let color_span = spans
            .iter()
            .find(|s| interner.resolve(&s.content) == "color");
        assert!(
            color_span.is_some(),
            "Should find 'color' CSS property in Vue style injection"
        );
        assert!(
            color_span.unwrap().style.fg.is_some(),
            "'color' should have syntax highlighting as CSS property in Vue"
        );
    }

    /// Counts `poll_abort` calls and cancels once `after` calls have been made.
    struct CancelAfter {
        hits: std::sync::atomic::AtomicUsize,
        after: usize,
    }

    impl HighlightControl for CancelAfter {
        fn poll_abort(&self) -> Option<HighlightAbort> {
            let seen = self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if seen >= self.after {
                Some(HighlightAbort::Cancelled)
            } else {
                None
            }
        }
    }

    /// A cancelled highlight must not come back as captures. `after = 2` skips
    /// the pre-query check and trips the progress callback, so removing that
    /// callback makes this return `Ok`.
    #[test]
    fn progress_callback_cancel_discards_the_highlight() {
        use std::sync::atomic::AtomicUsize;

        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.rs", "Dracula");
        let source: String = (0..200)
            .map(|i| format!("fn f{i}() {{ let value = {i}; }}\n"))
            .collect();
        let parsed = highlighter
            .parse_source(&source, &mut pool)
            .expect("rust source parses");
        let query = pool.get_or_create_query(parsed.lang).unwrap();
        let capture_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let theme = crate::syntax::get_theme("Dracula");
        let style_cache = ThemeStyleCache::new(theme);
        let control = CancelAfter {
            hits: AtomicUsize::new(0),
            after: 2,
        };

        let result = collect_line_highlights_bounded(
            &source,
            &parsed.tree,
            query,
            &capture_names,
            &style_cache,
            &control,
        );

        assert_eq!(result.err(), Some(HighlightAbort::Cancelled));
        assert!(
            control.hits.load(std::sync::atomic::Ordering::Relaxed) >= 2,
            "cancel must be observed from inside the query, not only before it"
        );
    }
}

#[cfg(test)]
mod priming_injection_tests {
    use super::*;
    use crate::language::SupportedLanguage;
    use crate::syntax::get_theme;

    #[test]
    fn test_collect_highlights_primed_vue() {
        // Simulate primed source: wrapping plain script content in <script lang="ts">
        let source = r#"<script lang="ts">
import { ref } from 'vue'
const count = ref(0)
</script>
"#;

        // Parse with Vue parser
        let mut pool = ParserPool::new();
        let parser = pool.get_or_create("vue").unwrap();
        let tree = parser.parse(source, None).unwrap();

        // Get style cache
        let theme_name = "base16-ocean.dark";
        let theme = get_theme(theme_name);
        let style_cache = ThemeStyleCache::new(theme);

        // Collect highlights with injection
        let highlights = collect_line_highlights_with_injections(
            source,
            &tree,
            SupportedLanguage::Vue,
            &style_cache,
            &mut pool,
            "vue",
        );

        // Line 1 is import, line 2 is const (line 0 is <script lang="ts">)
        let line1_captures = highlights.get(1);
        let line2_captures = highlights.get(2);

        assert!(
            line1_captures.is_some() && !line1_captures.unwrap().is_empty(),
            "Should have highlights for import line"
        );
        assert!(
            line2_captures.is_some() && !line2_captures.unwrap().is_empty(),
            "Should have highlights for const line"
        );
    }

    #[test]
    fn test_highlighter_markdown_returns_cst() {
        let highlighter = Highlighter::for_file("README.md", "base16-ocean.dark");
        assert!(
            matches!(highlighter, Highlighter::Cst { .. }),
            "Markdown files should use CST highlighter"
        );
        assert!(highlighter.style_cache().is_some());
    }

    #[test]
    fn test_highlighter_markdown_parses_source() {
        let highlighter = Highlighter::for_file("README.md", "base16-ocean.dark");
        let code = "# Heading\n\nSome **bold** text.\n";
        let mut pool = ParserPool::new();
        let result = highlighter.parse_source(code, &mut pool);
        assert!(
            result.is_some(),
            "Markdown source should be parseable by tree-sitter"
        );
    }

    #[test]
    fn test_markdown_injection_inline_highlights() {
        let source = "# Title\n\nSome **bold** and *italic* text.\n";
        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.md", "base16-ocean.dark");
        let result = highlighter.parse_source(source, &mut pool).unwrap();
        let style_cache = highlighter.style_cache().unwrap();

        let line_highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            style_cache,
            &mut pool,
            "md",
        );

        // Line 0 is "# Title" — should have at least heading highlights
        let line0 = line_highlights.get(0);
        assert!(
            line0.is_some(),
            "First line (heading) should have highlights"
        );
    }

    #[test]
    fn test_markdown_code_fence_injection() {
        let source = "# Title\n\n```rust\nfn main() {}\n```\n";
        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.md", "base16-ocean.dark");
        let result = highlighter.parse_source(source, &mut pool).unwrap();
        let style_cache = highlighter.style_cache().unwrap();

        let line_highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            style_cache,
            &mut pool,
            "md",
        );

        // Line 3 is "fn main() {}" inside the code fence
        // If injection works, it should have highlights from Rust parser
        let code_line = line_highlights.get(3);
        assert!(
            code_line.is_some(),
            "Code fence content should be reachable in line highlights"
        );
    }

    /// Format line highlights into a readable snapshot string.
    fn format_line_highlights(source: &str, highlights: &LineHighlights) -> String {
        use ratatui::style::Modifier;

        source
            .lines()
            .enumerate()
            .map(|(i, line_content)| {
                let hl = highlights.get(i);
                let hl_str = match hl {
                    Some(captures) if !captures.is_empty() => {
                        let parts: Vec<String> = captures
                            .iter()
                            .map(|cap| {
                                let start = cap.local_start.min(line_content.len());
                                let end = cap.local_end.min(line_content.len());
                                let text = &line_content[start..end];
                                let mut style_parts = Vec::new();
                                if let Some(fg) = cap.style.fg {
                                    style_parts.push(format!("fg:{:?}", fg));
                                }
                                if cap.style.add_modifier.contains(Modifier::BOLD) {
                                    style_parts.push("BOLD".to_string());
                                }
                                if cap.style.add_modifier.contains(Modifier::ITALIC) {
                                    style_parts.push("ITALIC".to_string());
                                }
                                if cap.style.add_modifier.contains(Modifier::UNDERLINED) {
                                    style_parts.push("UNDERLINED".to_string());
                                }
                                let style_str = if style_parts.is_empty() {
                                    "default".to_string()
                                } else {
                                    style_parts.join(",")
                                };
                                format!("{:?}[{}]", text, style_str)
                            })
                            .collect();
                        parts.join(", ")
                    }
                    _ => "(none)".to_string(),
                };
                format!("L{}: {:?} => {}", i, line_content, hl_str)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_snapshot_markdown_inline_highlights() {
        use insta::assert_snapshot;

        let source = "# Hello World\n\nSome text here.\n";
        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("test.md", "base16-ocean.dark");
        let result = highlighter.parse_source(source, &mut pool).unwrap();
        let style_cache = highlighter.style_cache().unwrap();

        let highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            style_cache,
            &mut pool,
            "md",
        );

        assert_snapshot!(format_line_highlights(source, &highlights), @r##"
        L0: "# Hello World" => "#"[fg:Gray], "Hello World"[fg:Rgb(143, 161, 179)]
        L1: "" => (none)
        L2: "Some text here." => (none)
        "##);
    }

    #[test]
    fn test_snapshot_markdown_code_fence_highlights() {
        use insta::assert_snapshot;

        let source = "# Title\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n";
        let mut pool = ParserPool::new();
        let highlighter = Highlighter::for_file("doc.md", "base16-ocean.dark");
        let result = highlighter.parse_source(source, &mut pool).unwrap();
        let style_cache = highlighter.style_cache().unwrap();

        let highlights = collect_line_highlights_with_injections(
            source,
            &result.tree,
            result.lang,
            style_cache,
            &mut pool,
            "md",
        );

        assert_snapshot!(format_line_highlights(source, &highlights), @r##"
        L0: "# Title" => "#"[fg:Gray], "Title"[fg:Rgb(143, 161, 179)]
        L1: "" => (none)
        L2: "```rust" => "```rust"[fg:Green], "```"[fg:Gray]
        L3: "fn main() {" => "fn main() {"[fg:Green], "fn"[fg:Rgb(180, 142, 173)], "main"[fg:Rgb(143, 161, 179)], "("[fg:Gray], ")"[fg:Gray], "{"[fg:Gray]
        L4: "    println!(\"hello\");" => "    println!(\"hello\");"[fg:Green], "println"[fg:Rgb(143, 161, 179)], "!"[fg:Rgb(143, 161, 179)], "("[fg:Gray], "\"hello\""[fg:Rgb(163, 190, 140)], ")"[fg:Gray], ";"[fg:Gray]
        L5: "}" => "}"[fg:Green], "}"[fg:Gray]
        L6: "```" => "```"[fg:Green], "```"[fg:Gray]
        "##);
    }
}
