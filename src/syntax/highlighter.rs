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
use std::hash::{Hash, Hasher};
use std::ops::ControlFlow;
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
/// Slab queries keep every match until the slabs can be merged, so a bushy
/// tree (many nodes per level) spends more memory than the whole-tree cursor.
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

/// Every node whose depth is a multiple of `stride`, preorder.
///
/// [`query_slabs`] merges the kept matches by finish byte. Visit order is the
/// order each slab's own cursor yielded, which the merge does not reshuffle.
fn collect_stride_roots<'tree>(
    tree: &'tree Tree,
    stride: u32,
    control: &dyn HighlightControl,
) -> Result<Vec<(Node<'tree>, u32, u32)>, HighlightAbort> {
    debug_assert!(stride > 0);
    let mut roots = Vec::new();
    let mut stack: Vec<(u32, u32)> = Vec::new();
    let mut cursor = tree.walk();
    let mut depth = 0u32;
    let mut steps = 0u32;
    loop {
        steps += 1;
        if steps.is_multiple_of(4096) {
            if let Some(reason) = control.poll_abort() {
                return Err(reason);
            }
        }
        if depth.is_multiple_of(stride) {
            while stack.last().is_some_and(|(seen, _)| *seen >= depth) {
                stack.pop();
            }
            let parent = stack.last().map(|(_, index)| *index).unwrap_or(u32::MAX);
            let index = u32::try_from(roots.len()).unwrap_or(u32::MAX);
            stack.push((depth, index));
            roots.push((cursor.node(), depth, parent));
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(roots);
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
    /// Index of the slab one stride shallower, or `u32::MAX` at the root.
    /// A match is repeated only by that parent, and only when it started on
    /// the shared boundary level.
    parent: u32,
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
    if u32::try_from(text.len()).is_err() {
        return query_whole(text, tree, query, sink, control);
    }
    let roots = collect_stride_roots(tree, QUERY_DEPTH_STRIDE, control)?;
    let bytes = text.as_bytes();
    let match_limit = control.match_limit();
    let nested = super::parser_pool::nested_pattern_flags(query);
    let mut slabs: Vec<Slab> = Vec::with_capacity(roots.len());
    let mut caps = Vec::new();
    let mut hits = Vec::new();
    let mut hashes = Vec::new();

    for (root, depth, parent) in roots {
        if let Some(reason) = control.poll_abort() {
            return Err(reason);
        }
        let hit_at = u32::try_from(hits.len()).unwrap_or(u32::MAX);
        let mut out = SlabOut {
            caps: &mut caps,
            hits: &mut hits,
            hashes: &mut hashes,
        };
        let added = query_one_slab(bytes, root, query, match_limit, &mut out, control)?;
        slabs.push(Slab {
            depth,
            parent,
            hit_at,
            hit_len: added,
        });
    }

    // The parent slab is the only other cursor that can see a match, and only
    // when the pattern started on the shared boundary. Keep the copy with the
    // higher count; a tie keeps the shallower cursor. Copies inside the
    // winning cursor stay, in the order that cursor yielded them.
    dedup_parent_overlaps(&mut slabs, &mut hits, &hashes);
    drop(hashes);

    emit_merged(&slabs, &hits, &caps, nested, sink, control)?;
    if let Some(reason) = control.poll_abort() {
        return Err(reason);
    }
    Ok(())
}

struct SlabOut<'a> {
    caps: &'a mut Vec<Cap>,
    hits: &'a mut Vec<SlabHit>,
    hashes: &'a mut Vec<u128>,
}

fn query_one_slab<'tree>(
    bytes: &[u8],
    root: Node<'tree>,
    query: &Query,
    match_limit: Option<u32>,
    out: &mut SlabOut<'_>,
    control: &dyn HighlightControl,
) -> Result<u32, HighlightAbort> {
    let mut cursor = QueryCursor::new();
    if let Some(limit) = match_limit {
        cursor.set_match_limit(limit);
    }
    // Capping a subtree that is already shallower than the stride changes
    // the order of matches that finish on the same node, and saves nothing.
    if node_deeper_than(root, QUERY_DEPTH_STRIDE) {
        cursor.set_max_start_depth(Some(QUERY_DEPTH_STRIDE));
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
    let raw = QueryCursor::into_raw(cursor);
    // SAFETY: `into_raw` does not free the cursor. The rebuilt wrapper owns
    // it until the end of this function, and `raw` is only read while
    // `matches` borrows that wrapper. The public cursor has no finish-node
    // accessor; [`cursor_finish_byte`] documents the layout it reads.
    let mut cursor = unsafe { QueryCursor::from_raw(raw) };
    let mut matches = cursor.matches_with_options(query, root, bytes, options);
    let before = out.hits.len();
    while let Some(mat) = matches.next() {
        if aborted.get().is_some() {
            break;
        }
        if mat.captures.is_empty() {
            continue;
        }
        let Ok(finish) = u32::try_from(cursor_finish_byte(raw)) else {
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
    if match_limit.is_some() && cursor.did_exceed_match_limit() {
        return Err(HighlightAbort::MatchLimit);
    }
    u32::try_from(out.hits.len() - before).or(Err(HighlightAbort::MatchLimit))
}

/// Drop the losing copy of a match that two adjacent slabs both found.
fn dedup_parent_overlaps(slabs: &mut [Slab], hits: &mut Vec<SlabHit>, hashes: &[u128]) {
    let mut drop_hit = vec![false; hits.len()];
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for (index, slab) in slabs.iter().enumerate() {
        if slab.parent != u32::MAX && slab.hit_len > 0 && slabs[slab.parent as usize].hit_len > 0 {
            pairs.push((slab.parent, u32::try_from(index).unwrap_or(u32::MAX)));
        }
    }
    pairs.sort_unstable_by_key(|(parent, _)| *parent);
    let mut child_counts: FxHashMap<u128, u32> = FxHashMap::default();
    let mut parent_counts: FxHashMap<u128, u32> = FxHashMap::default();
    let mut drop_from_parent: FxHashMap<u128, ()> = FxHashMap::default();
    let mut pair_at = 0;
    while pair_at < pairs.len() {
        let parent_index = pairs[pair_at].0 as usize;
        let parent_at = slabs[parent_index].hit_at as usize;
        let parent_len = slabs[parent_index].hit_len as usize;
        parent_counts.clear();
        drop_from_parent.clear();
        for hash in &hashes[parent_at..parent_at + parent_len] {
            *parent_counts.entry(*hash).or_default() += 1;
        }
        while pair_at < pairs.len() && pairs[pair_at].0 as usize == parent_index {
            let child_index = pairs[pair_at].1 as usize;
            pair_at += 1;
            let child_at = slabs[child_index].hit_at as usize;
            let child_len = slabs[child_index].hit_len as usize;
            child_counts.clear();
            for hash in &hashes[child_at..child_at + child_len] {
                if parent_counts.contains_key(hash) {
                    *child_counts.entry(*hash).or_default() += 1;
                }
            }
            for (offset, hash) in hashes[child_at..child_at + child_len].iter().enumerate() {
                let Some(&child_count) = child_counts.get(hash) else {
                    continue;
                };
                // Higher count wins. A tie keeps the shallower parent.
                if parent_counts[hash] >= child_count {
                    drop_hit[child_at + offset] = true;
                } else {
                    drop_from_parent.insert(*hash, ());
                }
            }
        }
        if drop_from_parent.is_empty() {
            continue;
        }
        for (offset, hash) in hashes[parent_at..parent_at + parent_len].iter().enumerate() {
            if drop_from_parent.contains_key(hash) {
                drop_hit[parent_at + offset] = true;
            }
        }
    }

    let mut dst = 0usize;
    for slab in slabs.iter_mut() {
        let start = slab.hit_at as usize;
        let end = start + slab.hit_len as usize;
        let new_at = u32::try_from(dst).unwrap_or(u32::MAX);
        for index in start..end {
            if !drop_hit[index] {
                hits[dst] = hits[index];
                dst += 1;
            }
        }
        slab.hit_at = new_at;
        slab.hit_len = u32::try_from(dst - new_at as usize).unwrap_or(0);
    }
    hits.truncate(dst);
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
