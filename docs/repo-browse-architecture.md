# Repository Browser — Architecture

The structure of the screen opened by `or --browse` / `b` / Cockpit → Repo Browse.
Read this before adding features.

## 1. Module layout

| File | Role |
|------|------|
| `src/app/browse.rs` | State definitions, file loading, spawning and collecting async tasks |
| `src/code_index.rs` | One-pass Hearth symbol/import repository analysis |
| `src/symbols.rs` | octorus compatibility facade over the Hearth symbol engine (see [symbol-index.md](symbol-index.md)) |
| `src/module_graph.rs` | octorus compatibility facade over Hearth import resolution and graph queries (see [module-graph.md](module-graph.md)) |
| `src/ui/browse.rs` | Rendering |
| `src/app/input_browse.rs` | Key handling (3 focusable panes + 2 browser overlays, plus PR/discussion modals) |

Do not put line counts in this table; they become stale whenever the browser or
its compatibility facade changes. Count the current checkout when needed.

```bash
wc -l src/app/browse.rs src/symbols.rs src/ui/browse.rs src/app/input_browse.rs
```

Changes to existing files are kept minimal:

- `src/app/types.rs` — 2 variants on `AppState` plus `is_repo_browse()`, 1 variant on `CockpitMenuItem`
- `src/app/mod.rs` — module declarations for `browse` / `input_browse`, the `browse_state: Option<BrowseState>` field, `poll_browse_updates()` added to the polling loop
- `src/app/input.rs` — 2 dispatch arms, the `b` branch in the file list
- `src/ui/mod.rs` — module declaration and 1 dispatch line
- `src/config/keybindings.rs` — `repo_browse` / `symbol_outline` / `symbol_search` / `module_graph` / `toggle_blame` / `open_blame_commit` / `open_blame_pr` / `open_line_discussion`
- `src/main.rs` — the `--browse` flag
- `src/ui/help.rs` — help entries
- `src/app/cockpit.rs` / `src/ui/cockpit.rs` — the Cockpit menu item that opens the browser
- `src/language.rs` — highlighting-language detection plus the public tags-query compatibility accessor
- `src/lib.rs` — `pub mod symbols;` and the public `ParserPool` re-export
- `src/syntax/parser_pool.rs` — highlight caches plus the Hearth parser/query cache
- `src/queries/moonbit/tags.scm` — the host-owned MoonBit symbol query; bundled-language queries are owned by Hearth
- `Cargo.toml` / `Cargo.lock` — the exact `hearth-graph` dependency, Rust version, and the `symbol_index` bench target
- `benches/ui_rendering.rs` — the `browse_render` group / `benches/symbol_index.rs` — facade extraction, build, and query benchmarks
- `tests/cli.rs` — e2e tests that launch the binary

The code-intelligence implementation depends on exact registry release
`hearth-graph 0.3.0` with `bundled-languages`, `fs`, `resolve-js`, and
`resolve-rust`, with default features disabled. See
[symbol-index.md](symbol-index.md) and [module-graph.md](module-graph.md) for
ownership and compatibility boundaries.

## 2. State machine

Following project principle 4 — "a state machine instead of individual
conditional flags" — **not a single boolean flag representing a screen, a mode,
or a load state was added**. `BrowseState` has no `bool` fields; "which screen",
"is it loading", and "is the index ready" are all answered by the enums below.

Persistent browser modes and loading transitions remain represented by enums,
not boolean flags. `OpenFile::viewable` is a property of loaded content, and
`Args::browse` is a one-shot CLI input rather than runtime state. The symbol
facade introduces no persistent boolean state: cancellation is read through a
stateless `CancelSignal` and Hearth returns an `IndexBuild` outcome.

```
AppState
 ├ RepoBrowseTree    focus on the tree pane
 ├ RepoBrowseFile    focus on the file content pane
 └ RepoBrowseGraph   focus on the right-side module graph pane
```

Inside `BrowseState`:

```
paths:     LoadState<Vec<String>>
           NotLoaded → Loading → Loaded(paths) | Error(msg)

index:     IndexState
           Idle → Building → Ready(Arc<SymbolIndex>) | Failed

graph:     ModuleGraphState
           Idle → Building → Ready(Arc<ModuleGraph>) | Failed

graph pane: ModuleGraphPaneState
            Closed → Loading { request_id, path } → Ready(ModuleGraphPanel)
                          ↑                           │
                          └──── Waiting { path } ←────┘

universe:  SourceUniverse
           Partial | Complete

open_load: OpenLoad
           Idle → Pending { path, line, scroll, cancel } → Idle | Failed { path, message }

blame:     BlameState
           Off → Waiting { path } → Loading { path, cancel }
               → Ready { path, gutter } | Failed

commit:    BrowseCommitDiffState
           Off → Loading { request_id, annotation, cancel }
               → Ready { annotation, cache, scroll }
               | Failed { annotation, message }

PR lookup: PrLookupState
           Idle → Loading { request_id, sha, cancel }
                → Selecting { sha, pulls, selected }
                | Failed { sha, failure }

overlay:   BrowseOverlay
           None | Outline { selected } | SymbolSearch { query, selected }

filter:    Option<ListFilter>   ← reuses the existing list filter
```

`IndexState` and `ModuleGraphState` remain independent typed lifecycles even
though one `CodeIndex` worker transitions them together. Code intelligence is
an accelerator, not a precondition: while it is building, tree browsing, file
viewing, and filtering all keep working. Symbol consumers `o` / `s` / `gd` and
the graph consumer `i` put the reason into the footer instead of opening an
overlay. A ready graph does not make a high-fan-in query synchronous:
`i` transitions the right-side pane to `ModuleGraphPaneState::Loading`, runs
both directions in `spawn_blocking`, and installs `Ready(ModuleGraphPanel)` only
when the request id and open path still match. The pane lifecycle is independent
of focus: `Esc` can return to `RepoBrowseFile` while the request continues.
Opening another file while the pane is visible cancels that request and enters
`Waiting`. Only the final successfully loaded file starts a new graph query, so
superseded file-load cancellation coalesces rapid navigation instead of
launching uninterruptible Hearth sort work for every intermediate path. `i`
from graph focus closes the pane and cancels; closing the browser cancels the
parent session token.

More than one message can come out. `o` and `gd` **check `open_is_pending()`
first**, so while a file is loading, `Still opening this file` wins regardless
of the index state (`browse_run_go_to_definition()` and
`open_browse_outline()`; in both, placing that check **above** the
`index.ready()` inspection is itself the contract — swap the order and both
messages still get emitted). An `open` that is still loading is a placeholder
with no lines and no symbols, so answering "no symbols here" or "no definition
found" from it would be a verdict about a file that has not been read. `s` is a
repository-wide search that does not depend on the currently open file, so it
does not carry this check.

| Key | While loading (even with the index incomplete) | Index not `Ready` | Otherwise |
|-----|-----------------------------------------------|-------------------|-----------|
| `o` | `Still opening this file` | `Symbol index is still building` | `No symbols in this file` if the target file has none, otherwise opens the outline |
| `s` | (not checked) | `Symbol index is still building` | opens the search overlay |
| `gd` | `Still opening this file` | `Symbol index is still building` | `No definition found` when it cannot resolve |
| `i` | `Still opening this file` | `Module graph is still building` | opens/focuses a cancellable right-side pane, or reports unsupported/unavailable analysis |

`Symbol index is still building` is emitted on `index.ready().is_none()`.
`IndexState::Failed` also satisfies that condition, so after a failed build the
footer keeps saying "building". The header shows `symbols: unavailable` in red,
so the same screen contradicts itself (a known limitation, §8).

`OpenLoad` is the single authority over a file being opened: "is something
loading", "which path's result are we waiting for", and "where does the cursor
go" all live in one place.

`BlameState` follows the same principle: shown-or-not, fetching, ready, and
failed live in one enum. `Waiting` is the state after opening another file
while blame is shown, lasting until that file is known to be viewable. As with
`IndexState::Failed`, failure reasons are not carried in the variant; they go
into `BrowseState::status` for the footer.

`BrowseCommitDiffState` is not a new screen; it is an in-browser mode describing
what the file content pane currently shows. `AppState` stays `RepoBrowseFile`,
so tree, open file, cursor, and blame are never stashed away — returning to
`Off` simply re-renders the original line.

Git Ops' `AppState::GitOpsSplitDiff` is not a commit-diff-only screen. It is the
right-pane focus of the Git Ops split view, and with the left tree selected it
shows the working-tree diff. Showing a commit diff there presupposes
`CommitLogState.commits[selected]` and `pending_diff_sha`, following the
paginated list selection. An arbitrary SHA coming from blame has no guarantee of
existing in that list, which is why C-2 does not reuse this state model — it
reuses only the fetch functions and `build_commit_diff_cache()`.

## 3. Data flow

```
open_repo_browse()
   │
   ├─ spawn_blocking: git ls-files -z --cached --others --exclude-standard ─┐
   │                (tracked files + untracked files that are not ignored)  │ paths_receiver
   ▼                                                                        ▼
AppState::RepoBrowseTree                                        poll_browse_updates()
                                                                           │
                                                              set_paths() → rebuild_tree()
                                                                           │
                                                              start_symbol_index_build()
                                                                           │
                                                       spawn_blocking: CodeIndex::build_cancellable
                                                                           │
                                                        hearth_graph::analyze_paths (one parse pass)
                                                                   ┌───────┴────────┐
                                                                   ▼                ▼
                                                           SymbolIndex         ModuleGraph
                                                                   └───────┬────────┘
                                                                           │ index_receiver
                                                                           ▼
                                                         IndexState::Ready + ModuleGraphState::Ready
                                                                           │
                                                                 refresh_open_file_symbols()
```

Opening a file:

```
browse_open_path(path, line)
   │
   ├─ cancel the previous request's token, drop the file/highlight receivers
   ├─ OpenLoad::Pending { path, line, scroll, cancel }
   │     └ open is a loading placeholder
   ├─ install a new file_receiver
   │
   └─ spawn_blocking: load_file()
         ├ run each step through stage(cancel, ..) (the closure is not called once cancelled)
         ├ decided from metadata alone: directory / over 8 MiB
         ├ decided after reading: non-UTF-8 / over 100,000 lines / a line over 10,000 bytes
         ├ build_file_patch()          ← pseudo patch with every line as context
         └ build_plain_diff_cache()    ← no tree-sitter involved
                    │
              deliver_file_load()      ← Ready is not sent once cancelled
                    │ file_receiver
                    ▼
            poll_browse_updates()
               ├ path mismatch → keep the receiver and keep waiting
               ├ path match + Err → install_file_load_failure()
               │                    └ OpenLoad::Failed
               ├ Disconnected while Pending
               │    └ "<path>: file loading task ended" → OpenLoad::Failed
               ├ path match + Ok
               │    ├ install_open_file() → OpenLoad::Idle
               │    └ file_ready = true
               └ after the 4 receiver arms are processed, file_ready == true
                    └ start_browse_highlight()
                         └ spawn_blocking: build_diff_cache()
                               ← tree-sitter highlighting
                                  │ highlight_receiver
                                  ▼
                           apply_highlighted_cache()
                               ← swapped in only when the path matches
```

Enabling the blame gutter:

```
toggle_browse_blame() (`gb`)
   │
   ├─ cancel the previous request's token, drop the blame_receiver
   ├─ BlameState::Waiting { path }
   │
   └─ only for a viewable OpenFile: BlameState::Loading { path, cancel }
          └ spawn_blocking: crate::github::blame_file()
                    │
              deliver_blame_load()  ← not sent once cancelled
                    │ blame_receiver
                    ▼
            poll_browse_updates()
               ├ path mismatch → keep the receiver and keep waiting
               ├ Err → BlameState::Failed + status
               └ Ok  → prepare the per-line display strings exactly once
                        → BlameState::Ready { path, gutter }
```

Opening another file while blame is shown cancels the old fetch and moves to
`Waiting`. The fetch starts only after the file load completes and
`OpenFile::viewable` has been checked, so `git blame` is never launched against
binary or over-limit files. A result whose path differs from the currently open
path is likewise discarded on the poll side.

Opening the commit diff behind a blame line:

```
open_browse_blame_commit() (`gc`)
   │
   ├─ take the full SHA from the cursor line's shared BlameAnnotation
   ├─ save path / line / scroll via browse_push_jump()
   ├─ BrowseCommitDiffState::Loading { request_id, annotation, cancel }
   └─ tokio task: fetch_local_commit_diff / fetch_commit_diff
          └─ spawn_blocking: build_commit_diff_cache()
                    │ commit_diff_receiver
                    ▼
            poll_browse_updates()
               ├─ request_id or SHA mismatch → keep the receiver, drop the stale result
               ├─ Err → Failed { annotation, message }
               └─ Ok  → Ready { annotation, cache, scroll }
```

Source selection matches Git Ops: local git in local mode or when no PR is
selected, otherwise the GitHub commit diff API. Both the fetch and the cache
build happen outside the draw loop. A diff over 32 MiB gets no cache — it
becomes `Failed`, and the error plus the way back are shown in both the content
pane and the footer. Cancellation does not guarantee the external process stops
mid-run, but the request-scoped token, receiver replacement, and request id form
three layers that keep a stale result from being installed.

Opening the pull request that introduced a blame line:

```
open_browse_blame_pr() (`gp`)
   │
   ├─ top-level RepositoryAvailability is Unavailable → footer (the API is not called)
   ├─ take the full SHA and subject from the cursor line's BlameAnnotation
   ├─ SessionCache[sha] hit → apply the resolution as-is
   └─ miss → PrLookupState::Loading { request_id, sha, cancel }
                │ tokio task: gh api repos/{owner}/{repo}/commits/{sha}/pulls
                ▼
          poll_browse_updates()
             ├─ request_id / SHA / current cursor SHA mismatch → drop the stale result
             ├─ typed failure → Failed + fixed footer
             ├─ API returns 1 → select_pr(number)
             ├─ API returns several → Selecting popup
             ├─ API [] + subject ending in ` (#123)` → select_pr(number) as inferred
             └─ API [] + no fallback → cache NotFound
```

A non-empty API array is always the source of truth; the commit subject is a
second-best fallback used only for a well-formed empty array `[]`.
Malformed/null/empty responses and CLI/API failures are never papered over with
the subject. An inferred transition keeps showing
`PR #N inferred from commit subject; GitHub did not confirm it` from the moment
PR data starts loading until the PR screen is left.

Only successful lookup results (confirmed / inferred / not found) are stored in
the session cache, keyed by SHA. Missing CLI, unauthenticated, rate limit, API
failure, empty response, and malformed response are not cached so they can be
retried. Even when the same commit appears on many blame-gutter lines, the
network call happens once.

`build_plain_diff_cache()` is a single pass of `expand_tabs` → `classify_line` →
string interning and takes no `ParserPool`. This is why a file can be displayed
without waiting for highlighting; the replacement produced by
`build_diff_cache()` arrives afterwards.

`start_browse_highlight()` passes `App::is_markdown_rich()` through to
`build_diff_cache()`, so the browser shares the diff view's markdown rich
display. `M` (`toggle_markdown_rich`) in the file pane flips the flag via
`toggle_browse_markdown_rich()` and re-runs `start_browse_highlight()` — but
only for an open markdown file (the flag changes no other language's output),
and never while `open_is_pending()`: a pending `open` is a placeholder, and
highlighting it would race the landing file's cache with an empty one. The
landing load starts its own highlight and reads the new flag then.

Two verdicts come from a single `std::fs::metadata` call. Anything that is not a
regular file (directories included) makes `file_metadata()` return
`not a regular file` and the load becomes `FileLoad::Failed`; files over
`MAX_VIEWABLE_FILE_BYTES = 8 * 1024 * 1024` become a notice from the
`metadata.len()` comparison alone. Neither reads the contents. The remaining
three — the non-UTF-8 binary notice, `MAX_VIEWABLE_FILE_LINES = 100_000`
(exactly that many lines still opens), and `MAX_VIEWABLE_LINE_BYTES = 10_000`
against the longest line — can only be decided after reading the file.
On the listing side, the full `git ls-files` output is parsed and then truncated
to `MAX_BROWSE_FILES = 200_000` entries. The truncation is recorded in `total`,
and the footer reports the count.

`BrowseState::open_is_pending()` is the single entry point for asking whether
the file being opened is still loading. A loading `open` is a placeholder, so
code that answers questions about the open file checks this method first, to
avoid treating an unread file as one that has no content.

`start_browse_highlight()` returns on the spot when `open.viewable` is false, so
binary and over-limit files never reach the highlighter. This is not a promise
you verify by reading — it is pinned by a test:
`test_unviewable_files_never_start_background_highlighting` actually creates a
binary file and an over-8-MiB file, settles them through `browse_open_path`, and
asserts that not a single `highlight_receiver` is installed. Remove the guard
and it fails with `receivers were installed for ["binary", "oversized"]` (both
kinds are reported, so a fix that lets only one of them through is also caught).

**The pseudo-patch trick** is the single most effective design decision.
Converting the file contents into a `@@ -1,N +1,N @@` patch with every line
prefixed by a space and feeding it through the existing `build_diff_cache`
means:

- highlighting, themes, tab expansion, and the Vue/Svelte/Markdown injections all work as-is
- the rendering path stays unified with the diff view (improvements benefit both; neither rots alone)

The same trick was already in use in `build_pr_description_patch()`, and this
follows it.

`paths_receiver` / `index_receiver` / `module_graph_query_receiver` /
`file_receiver` / `highlight_receiver` / `blame_receiver` /
`commit_diff_receiver` are all drained with `try_recv()` in
`poll_browse_updates()`. `index_receiver` carries the combined symbol/module
build result, so no second parser task or graph-build channel exists. The
separate module-graph receiver carries only request-scoped, already-projected
panels. The draw loop never blocks.

## 4. Key-handling layers

At the top of `handle_repo_browse_{tree,file,graph}_input`, input is fed through
the layers from the top down.

```
1. Overlays (Outline / SymbolSearch)   ← modal; consume everything while open
2. commit diff mode (file pane only)  ← blocks the source-file actions
3. Filter input bar (tree pane only)  ← consumes all character input while open
4. Shared browse actions (s / ? / Z / Ctrl-o)
5. Sequences (tree: Space / and gg / file: gb, gc, gp, gr, gd, gf, gg / graph: configured i)
6. Focus-specific single keys
```

**Why the sequence layer exists**: the default for `filter` is the two-key
sequence `Space /`, which `matches_single_key` can never match. It follows the
same `push_pending_key` / `try_match_sequence` convention as the existing file
list / diff view.

**Input rules for the symbol search overlay**: character input has priority, so
`j` / `k` go into the query. Selection moves only with `↑` `↓` and `Ctrl-p`
`Ctrl-n` (clear the whole query with `Ctrl-u`). A search UI where you cannot
type `j` is infuriating, so this is deliberate.

**Input rules for the module graph pane**: `i` from file focus opens or focuses
the pane; `l` / `→` also focuses an already-visible pane. While waiting/loading,
graph navigation is ignored, but `Esc` / `q` can return to code without cancelling and
`i` closes and cancels. Once ready, `j/k` and arrows move, `Tab` or `h/l`
switches Imports / Imported by, and `Enter` opens only a target represented in
the original Git listing. Nodes without a jump stay visible and report why they
cannot be opened. Opening a node returns focus to code and refreshes the still
visible pane for that new current file.

**Mouse routing**: mouse events do not pass through these key layers. Instead,
renderers register hit regions into `App::hit_map` (`src/ui/hit.rs`) each frame
and `handle_mouse` (`src/app/input_mouse.rs`) resolves the topmost region —
overlay z-order is expressed by registration order (backdrop → surface → rows),
so a click can never reach a layer the keyboard could not. Mouse actions call
the same `pub(crate)` methods the key branches call (`browse_tree_move_down`,
`browse_file_click_line`, `browse_graph_move`, the four `browse_*_dismiss`
overlay closers, …), so behaviour cannot drift between the two input paths.

## 5. Keybinding registration caveats

`KeybindingsConfig::validate()` detects exact duplicates between single keys and
sequences, and collisions between single keys and sequence prefixes, but the
loader prints `Warning:` to stderr and starts anyway. New default keys collide
with existing ones easily:

- `b` … collides with `rally_background`
- `o` … collides with `filter_open`
- `s` … collides with `suggestion` / `git_ops_stage_all`
- `i` … is reserved for the browser module graph and may collide with future input actions

The defaults are separated by registering screen-specific actions in
`is_context_compatible()`'s `SCREEN_SPECIFIC_KEYS`. `module_graph` is not a
wildcard inside the browser file pane: validation explicitly rejects overlaps
with outline, search, movement, and active browser sequence prefixes. The
single-key ownership map retains every primary and alternative owner, and every
multi-key alternative contributes its prefix, so an earlier action from another
context cannot hide a later same-pane collision. When adding a key, touch all
of:

1. the `KeybindingsConfig` field
2. the `Default` impl
3. the `bindings` array in `validate()`
4. `SCREEN_SPECIFIC_KEYS` (only when the single key really is valid on one screen only)
5. `serialize_entry` in the `Serialize` impl

`toggle_markdown_rich` (`M`) is a pre-existing global binding that the file
pane merely dispatches; nothing new is registered, and because the same key is
valid in the diff view and issue detail too, it must not be added to
`SCREEN_SPECIFIC_KEYS`.

The blame gutter registers into the file pane's existing sequence layer as
`toggle_blame = ["g", "b"]`. It shares the prefix with `gg` / `gd` / `gf` but
differs in the second key. Because `validate()` puts only the first key of a
sequence into `sequence_prefixes`, `toggle_blame` **must not** be registered in
`SCREEN_SPECIFIC_KEYS`. Registering it would suppress single-key collisions
against the entire `g` prefix as context-compatible.
`open_blame_commit = ["g", "c"]` stays out for the same reason. The validator
also detects two fully identical sequences, so configuring something like `gb`
for an existing action is rejected. `open_blame_pr = ["g", "p"]` and
`open_line_discussion = ["g", "r"]` likewise stay out. All four bindings remain
visible to the duplicate detector as `g`-prefix owners, and if any of them is
configured as a single key it executes in the single-key dispatch that runs
before the sequence layer.

## 6. Rendering

`src/ui/browse.rs`. In zen mode the header and footer are dropped and the whole
frame becomes the active panes.

Renderers also populate the frame's mouse hit map while they draw: the tree
pane registers one `ListRow` per visible row using the same centered offset it
renders with, the content pane registers one `ContentLine` per rendered row
with the logical file line resolved at wrap time (`WrappedRows` keeps rendered
rows and logical lines in lockstep), the graph pane registers one three-row
region per visible node plus a title-row direction toggle, and each overlay
registers backdrop → surface → rows on top. Because registration happens at
draw time with the renderer's own offsets, the input side never re-derives
layout math.

- Tree pane: "loading spinner", "error", or "tree" according to `LoadState`
- Content pane: nothing selected / the binary or oversized-file notice / the contents
- Graph pane: absent when `Closed`; otherwise the remaining non-tree width is
  split evenly between code and graph. `Waiting` shows the target module while
  its file load settles; `Loading` shows an explicit dependency-resolving state.
  `Ready` shows the double-line current module plus
  three-line single-border relationship boxes and directional connectors
- The content pane cursor scrolls with the diff view's margin behaviour:
  `clamp_scroll` calls the shared `margin_scroll_offset` (`src/diff_store.rs`),
  so the cursor rides the viewport centre and only walks to the edge at file
  boundaries. The diff view clamps overscroll at render time; the browser clamps
  in state because `content_window` consumes the offset directly. The in-browser
  commit diff pane uses `ScrollMode::Margin` with the same render-side clamp as
  the diff view
- The line-number gutter has a minimum width of `LINE_NUMBER_WIDTH = 5` columns,
  and `gutter_width()` widens it to match the digit count of the total line
  count. A 100,000-line file gets 6 digits, and if the cap is ever raised past
  999,999 lines it widens again automatically. The cursor line turns its gutter
  yellow, and when `diff.bg_color` is enabled the line background is painted too
- In `BlameState::Ready` the blame gutter sits to the left of the line numbers.
  When the result arrives, the per-commit strings — full (sha + author +
  relative time) / without time / identity — are truncated to display width and
  prepared once; consecutive lines of the same commit are left blank. Rendering
  only references the prepared `&str`s, degrading as the pane narrows:
  full → without time → identity → hidden. Uncommitted lines never show the zero
  SHA and epoch 0 — they show `Uncommitted`
- **Blame line count and buffer line count can disagree** (the file on disk
  changed after the browser opened it). `BlameGutter::from_file` always builds
  exactly as many rows as the buffer has lines and records the mismatch in
  `BlameCoverage` (`Exact` / `ShorterThanBuffer` / `LongerThanBuffer`). A line
  with no blame shows `[not blamed]`, distinct from the blank that means "same
  commit as above", and when coverage is not `Exact` the footer shows
  `blame covers N lines, this file shows M — reopen the file to refresh` (lower
  priority than `status`). **Never truncate silently** — truncation would make
  "a file with no history" and "a display that is out of sync" look identical
- When `BrowseCommitDiffState` is not `Off`, only the content pane is replaced
  by the commit diff. Loading / empty / error each have explicit bodies; ready
  borrows just the viewport's neighbourhood of the cached lines. `j/k`, the page
  keys, and `gg/G` move only the commit diff's `DiffScrollState`
- `LineType::Header` rows coming from the pseudo patch (`@@ ... @@`) are
  filtered out before rendering. `content_window()` exploits the fact that
  headers are a leading prefix and borrows a single contiguous slice, so the
  work scales with the viewport width. **Line N of the file is line N+1 of the
  cache**
- One context-marker space is stripped from the leading span of every line
- A file line wider than the pane wraps onto continuation rows
  (`push_wrapped_line()`), each carrying a blank prefix as wide as the whole
  gutter — blame and discussion columns included — so the line-number column
  stays straight. Rows break on character boundaries: CJK prose has no spaces,
  and ratatui's word wrapper would push such a paragraph onto its own rows
  below a lonely line number, which is why the pane does not use
  `Paragraph::wrap`. Sub-spans borrow slices of the interned text, and
  `content_lines()` stops emitting at the viewport height (`max_rows`), so
  rendering stays O(viewport) even when every visible line wraps. The
  cursor-line background extends across a wrapped line's continuation rows

Overlays are drawn centred, after going through `clear_overlay_area()`. The
function `Clear`s the target area and, if the column just left of it holds a
double-width glyph (CJK etc.), replaces that cell with a space. When a
double-width glyph straddles the left border, ratatui's buffer diff skips the
next cell and the border line never reaches the terminal. The right edge needs
no such repair: overwriting the leading cell already leaves a space in the
continuation cell. `render_outline` (60%×70%) and `render_symbol_search`
(80%×70%) both go through it. The module graph is a real pane and deliberately
does not clear or cover adjacent panes. Any new overlay added to
`src/ui/browse.rs` must also use `clear_overlay_area()` rather than a bare
`Clear`. A continuation cell
is a space at the text level, so text snapshots alone cannot catch this mistake.
Removing the repair fails
`test_overlay_left_border_survives_a_wide_glyph_straddling_it`.

`overlay_rect()`'s centring and staying in bounds are verified by
`test_overlay_rect_is_centred_and_bounded`. On 100×40, an 80%×50% rect becomes
80×20 at (10, 10), and even on 10×4 it stays inside the terminal.
`test_symbol_search_overlay_is_reviewably_clipped_in_a_tiny_terminal` and
`test_empty_module_graph_pane_clips_in_a_tiny_terminal` pin both overlay and pane
clipping at 20×5.

The module graph pane never queries Hearth from the input or draw path.
`open_browse_module_graph()` starts a request-scoped blocking task; its delivery
converts direct and reverse results into a `ModuleGraphPanel` only if the
request/path context is still current. Each direction retains at most 200 nodes
plus the full edge count; components are capped at 512 Unicode scalars and both
final node and edge labels at 240 terminal cells. `render_module_graph_ready_pane()`
formats only `skip(offset).take(visible_nodes)`, three lines per visible UML
node, preserving O(viewport) redraw cost even when the repository graph is
large. Hearth 0.1.1 still materializes and sorts all matching edges inside the
blocking task before octorus can truncate them; see
[module-graph.md](module-graph.md).

## 7. Tests

**This document does not state test counts.** It used to, and for three
consecutive rounds the numbers stayed wrong (the previous revision said 194
total, 79 in `src/app/browse.rs`, 52 in `src/symbols.rs`, while counting the
same revision's code with the command below gave `app::browse` 82 and
`symbols::tests` 53). When you need a count, count. What a reader usually wants
is "where are the tests for what", so that table stays:

| Where | What |
|-------|------|
| `src/app/browse.rs` | `git ls-files` parsing, pseudo-patch conversion, tree, filter, cursor/scroll, file loading/cancellation, graph query failure/high-fan/label bounds |
| `src/symbols.rs` | Hearth-registry extraction snapshots, compatibility boundaries (including C macro merging, empty files, duplicate paths, CJK and checked conversion), projected index refs, search, and build cancellation |
| `src/code_index.rs` | one-pass combined symbol/import result and cancellation |
| `src/module_graph.rs` | JS/TS/Rust import extraction and resolution, listed non-source navigation, path projection, direct/reverse guarantees, cancellation |
| `src/ui/browse.rs` | inline rendering snapshots, graph-pane loading/outgoing/incoming/truncated/long-CJK/empty/tiny states, focus borders, wide-glyph overlay repair |
| `src/app/input_browse.rs` | **scenario tests** (tree navigation, outline/search, graph focus/close/cancel/restart/CJK JSON/empty/switch/jump/back, blame/history) |
| `src/main.rs` | the flag set allowed to start without a repository (including `--browse`) |
| `tests/cli.rs` | e2e launching the binary via `assert_cmd` (non-git directory, git repo without a GitHub remote) |

Count the current per-module tests rather than relying on historical branch
deltas:

```bash
cargo test --lib -- --list | grep ': test$' | awk -F'::' '{print $1"::"$2}' | sort | uniq -c
```

This command counts only what is registered as a `#[test]`. Helper functions
like `fn test_symbol(..)` are caught by `grep -c 'fn test_'` but do not appear
in this listing, so counting by grep runs one high in each of `src/symbols.rs`
and `src/app/input_browse.rs`.

`src/main.rs` and `tests/cli.rs` are pre-existing files, so they are outside
`--lib` and absent from that listing. Count them with
`cargo test --bin or -- --list` / `cargo test --test cli -- --list`
respectively. Both report only per-target totals, not per-module ones, so when
you want just this branch's additions use
`git diff main -- <file> | grep '^+' | grep -E 'fn +[a-z_]+\('`
(test function names in `tests/cli.rs` do not start with `test_`).

### Updating insta inline snapshots

This environment has `cargo-insta 1.46.3` (`~/.cargo/bin/cargo-insta`). Inline
snapshots can be updated with it.

```bash
cargo insta test --accept --lib -- <test_name>   # run and accept the diffs in place
cargo insta test --lib -- <test_name>            # write .pending-snap only, no acceptance
cargo insta accept                               # apply the accumulated .pending-snap files
cargo insta review                               # review interactively, one at a time
```

`--accept` and `cargo insta accept` rewrite the `assert_snapshot!(..., @"...")`
literals directly in the source. Content that needs no escaping may get
normalised from a raw string `@r"..."` to `@"..."` in the process, so after
accepting, re-run the affected tests and take `cargo fmt --all -- --check`
through as well. In practice, breaking one inline snapshot in `src/ui/browse.rs`
and letting `--accept` repair it restores the content but leaves a diff where
only `@r"` turned into `@"`.

`INSTA_FORCE_UPDATE=1` on a plain `cargo test` does not rewrite inline
snapshots. A `.pending-snap` appears but the source stays untouched and the test
keeps failing. The fallback for environments without cargo-insta is

```bash
cargo test --lib <test_name> 2>&1 | sed -n '/Snapshot Summary/,/insta review/p'
```

reading the `+new results` side and hand-replacing the inline string in the
source — but that is not the default procedure.

### Tests that need a tokio runtime

`browse_open_path()` calls `spawn_blocking`, so a plain `#[test]` panics with
"there is no reactor running". Use `#[tokio::test] async fn`.

## 8. Known limitations

| Limitation | Impact | Possible fix |
|------------|--------|--------------|
| The index is built once, when `BrowseState` is created | Re-entering the same root does not rebuild, so the index stays stale after external changes. No double start while building. Closing and reopening rebuilds everything from zero rather than incrementally | There is no refresh key today. Rebuild on `R`, or piggyback on the existing file watcher |
| The file listing is also produced once, at `BrowseState` creation | Re-entering the same root does not re-enumerate, so new files do not appear in the tree. Closing and reopening re-enumerates everything | Same as above |
| Search results are cut off at 200 | Only `MAX_SYMBOL_SEARCH_RESULTS` entries are returned and the rest are lost. The returned order equals sorting all matches, so ranking is not lost | `matches` is the post-truncation `hits.len()`, so it saturates at `200 matches` and truncation cannot be told apart. Paging is unimplemented |
| Cursor/scroll arithmetic is logical-line-based under wrapping | Long lines wrap instead of being cut off, but `clamp_scroll` still counts logical lines, so when several wrapped lines share the viewport the cursor line can sit below the last visible row (the diff view accepts the same imperfection) | Make margin scrolling visual-row-based, the way `pr_description` counts wrapped rows with `Paragraph::line_count` |
| `gd` jumps to the first identifier that resolves | Columns are ignored; identifiers are scanned from the start of the line, minus duplicates and common keywords. For `foo.bar()`, if `foo` has a definition it jumps there, otherwise it tries `bar` | Show the existing `SymbolPopupState` when there are several candidates |
| symbol references unimplemented | The `@reference.*` captures present in the tags queries are still dropped; the module graph models file imports, not arbitrary symbol references | Keep import navigation file-level, or add a separate reference index with explicit per-language coverage |
| Module graph is built once | Imports and resolver configuration become stale after external edits; no watcher-driven upsert or re-resolution exists | Re-analyze changed files and use Hearth incremental graph APIs; clear resolver caches when config dependencies change |
| Rust graph answers are approximate | Hearth 0.1.1 does not model Cargo targets, `cfg`, `#[path]`, macros, or inline modules completely | Supply Cargo metadata and a declaration-tree model upstream |
| Only a root JS config is selected explicitly | Complex workspaces may need package-local tsconfig policy beyond current `oxc_resolver` behavior | Discover project configs and define deterministic ownership before building resolvers |
| Only one file-contents cache | Going through `OpenLoad` keeps the UI thread free, but every revisit re-reads the whole file | Give it an LRU like `DiffCacheStore` |
| `o` / `s` / `gd` keep saying "building" after a failed index build | All three test `index.ready().is_none()`, so `IndexState::Failed` falls into the same branch. The header shows `symbols: unavailable` in red, so two places on the same screen disagree | Match on `IndexState` in the footer too, and on `Failed` show the actual failure reason held in `BrowseState::status` |

### Gates verified by revert

Every entry below was actually exercised: revert the named spot and the named
test fails. Maintain this mapping when editing.

- **Cancellation of superseded background file loads**
  - remove `cancel.cancel()` from `browse_open_path_at` →
    `test_opening_a_second_file_cancels_the_first_request` (`first_request.is_cancelled()`),
    `test_opening_a_newer_file_supersedes_an_in_flight_background_load`
    (`receiver replacement alone must not be mistaken for work cancellation`)
  - revert `stage()` to an unconditional `Some(work())` →
    `test_cancelled_stage_does_not_run_its_work`,
    `test_pre_cancelled_file_load_skips_metadata_work`,
    `test_pre_cancelled_file_contents_skip_read_work`
  - drop the `if !cancel.is_cancelled()` send guard in `deliver_file_load()` →
    `test_cancelled_ready_load_is_not_delivered`

- **Cancellation of the index build**
  - stop adapting `CancelSignal` to the closure passed to
    `hearth_graph::build_index` → `test_cancelled_build_stops_metadata_prefilter`,
    `test_cancelled_build_stops_scanning_early`, and
    `test_precancelled_build_scans_nothing`
  - remove `state.cancel_token.cancel()` in `open_repo_browse` (re-entry to a
    different root) →
    `test_open_repo_browse_replaces_different_root_and_cancels_old_session`
  - change `IndexBuild::Cancelled { .. } => {}` in `start_symbol_index_build` to
    "send something" → `test_pre_cancelled_real_symbol_index_build_delivers_nothing`.
    This runs the real Hearth-backed build through `spawn_blocking` and checks
    that a cancelled session delivers nothing on the channel

- **Nothing reaches the highlighter**: §3's
  `test_unviewable_files_never_start_background_highlighting`

- **Overlay left-border repair**: remove the double-width-cell replacement loop
  from `clear_overlay_area()` →
  `test_overlay_left_border_survives_a_wide_glyph_straddling_it`

### Properties not protected by automated gates

- **Re-entry stopping a build that is already running**: the gates above pin
  (a) a cancelled build stops early, (b) re-entry cancels the old session token,
  and (c) a real build launched with a pre-cancelled token delivers nothing. One
  hole remains: no test drives the path where re-entry stops a build that is
  already mid-flight. (c) covers cancellation completing **before** the build
  starts, not cancellation triggered by `open_repo_browse` re-entry. If this
  breaks, the rendered result does not change — the damage is wasted CPU and an
  overwrite by a stale index.

- **The line-count / line-length caps being evaluated before construction**:
  `load_file_contents` evaluates both caps **before** building the lines vector,
  the pseudo patch, and the plain cache. That ordering is the caps' entire
  purpose (never allocate per-line render state for a huge file), yet swapping
  the order returns notices identical to the letter, so **no test notices at
  all** (measured in a revert sweep). With 500k lines or a minified bundle, the
  full lines vector, patch, and cache would be allocated before rejection —
  exactly the seconds-long freeze and memory spike the caps exist to prevent.
  The cap **decisions** themselves are pinned at the inclusive boundary by
  `test_line_count_cap_admits_its_own_value_and_rejects_one_more` and
  `test_line_length_cap_admits_its_own_value_and_rejects_one_more`, and the
  byte-based measurement by
  `test_the_line_length_cap_counts_bytes_not_characters`. Only the **position**
  is unguarded. When reordering, verify it yourself.

- **Rendering cost being O(viewport)**: `cargo test` pins the shape only.
  `test_content_window_finds_the_content_start_by_prefix_not_by_filtering` pins
  the contract "headers are a leading prefix and the window is one contiguous
  slice", but measures no cost. The cost measurement lives in the
  `browse_render` group of `benches/ui_rendering.rs`. That is one of the two
  benches `.github/workflows/benchmark.yml` runs at line 26 via
  `cargo bench --bench ui_rendering --bench diff_parsing`, subject to
  `alert-threshold: '150%'` at line 36. Injecting a raw
  `open.cache.lines.iter().filter(..)` walk into `render_content`'s per-frame
  path measured as follows (Criterion point estimates, same machine and session,
  `--bench ui_rendering -- browse_render`):

  | State | browse_render/200 | browse_render/30000 |
  |---|---|---|
  | Current (clean, run 1) | 45.692 us | 49.350 us |
  | Current (clean, run 2) | 47.840 us | 46.182 us |
  | O(file) walk injected (`.collect()`) | 46.903 us | 89.295 us |
  | Same walk as `.count()` | 46.023 us | 66.182 us |

  The `.collect()` case at 30000 is `89.295 / 49.350 = 1.809` against one clean
  run and `89.295 / 46.182 = 1.934` against the other — reliably past the 150%
  threshold. The 200-line case is `46.903 / 45.692 = 1.027`, within noise:
  exactly the O(viewport) property, unmoved by file length.

  - **The assertions do not detect it**: with the walk injected, `cargo test`
    still passes all three test binaries at 1485 / 82 / 18, and
    `cargo bench --bench ui_rendering -- --test` reports Success for both
    `browse_render/200` and `browse_render/30000`. Only an actual Criterion
    sampling run detects it. This is not the assertions being skipped:
    `assert_browse_frames_are_comparable` does run, and changing
    `assert_eq!(small_lines.len(), 18)` to 17 makes `-- --test` panic at
    `benches/ui_rendering.rs:98` with `left: 18 / right: 17`.

  - **Sensitivity has a floor**: with the same walk as `.count()` instead of
    `.collect()`, the ratio is `66.182 / 49.350 = 1.341`, and even against the
    faster clean run `66.182 / 46.182 = 1.433` — both below the 1.5 threshold,
    so no alert. On top of that, the two clean baselines themselves wobble by
    1.047× on browse_render/200 and 1.069× on browse_render/30000, so the ratio
    swings between 1.34 and 1.43 depending on which baseline you land on. The
    alert catches gross regressions, not fine ones.

  - **The alert blocks nothing**: `benchmark.yml` sets `fail-on-alert: false`
    at line 38, and its only triggers are the `workflow_dispatch` and weekly
    cron at lines 5-8. When it fires, it merely comments on a non-blocking run
    tied to no PR.

  `lint.yml` runs `cargo clippy --all-targets --workspace -- -D warnings` at
  line 46, which compiles the benches but does not run them. Its triggers are
  `pull_request` and `push: branches: [main]` at lines 3-6, so on a feature
  branch it does not run until a PR is opened. Adding
  `cargo bench --bench ui_rendering -- --test` to the job would gate only the
  frame-shape assertions, not the O(viewport) cost. There is currently no cheap
  assertion form for this cost property.

## 9. Extension points

- **A new overlay**: add a variant to `BrowseOverlay` and fill in the matches in
  `handle_browse_overlay_input` and `render_overlay`. The compiler reports what
  you missed. Precompute graph-sized data before opening; rendering should own
  only viewport work
- **A new import language**: register its `ImportSpec` and resolver in Hearth or
  the host registry, add facade projection/guarantee tests, and update
  [module-graph.md](module-graph.md). Never infer exactness in the UI
- **A new in-pane action**: add a `matches_single_key` branch to
  `handle_repo_browse_file_input`. The borrow of `self` and the mutable borrow
  of `browse_state` collide, so **decide into a bool first, then take the
  state** — the standard shape the existing code already uses
- **Additional line annotations**: as with blame, keep the fetch lifecycle in a
  `BrowseState` enum, build the render data once when the result arrives as a
  sidecar, and only borrow in `render_content`'s per-frame path.
