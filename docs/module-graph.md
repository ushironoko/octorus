# Module Graph — Technical Reference

The Repository Browser import engine is an octorus-owned compatibility facade
over `hearth-graph = "=0.3.0"`. Read this document when changing import
extraction, resolver setup, graph guarantees, or the `i` dependency graph pane.
Browser task/state/rendering architecture remains in
[`repo-browse-architecture.md`](repo-browse-architecture.md); symbol-specific
behavior remains in [`symbol-index.md`](symbol-index.md).

## 1. Ownership and one-pass data flow

Hearth owns parsing, import extraction, module resolution, graph storage,
reverse-dependency maintenance, sorting, and exactness calculations. Octorus
owns repository enumeration, cancellation, resolver configuration, path
projection, browser state, and UI labels.

```text
git ls-files paths + repository root + CancelSignal
    │
    ▼
hearth_graph::analyze_paths                 src/code_index.rs
    │  one tree-sitter parse per source
    ├─ symbols ────────────────► octorus SymbolIndex projection
    └─ imports/language/hash ──► octorus ModuleGraph facade
                                      │
                                      ├─ dependencies(path)
                                      └─ dependents(path)
```

The browser must use `CodeIndex::build_cancellable`; starting an independent
`build_index` and `analyze_paths` would parse every supported file twice.
`SymbolIndex::build_cancellable` remains as the public symbols-only
compatibility API for existing external callers and benchmarks.

The shared build keeps the existing limits:

- maximum source size: 2 MiB;
- maximum workers: 8;
- maximum symbols per file: Hearth's 10,000 cap.

Unsupported, oversized, sparse/missing, unreadable, and non-UTF-8 source files
are skipped by Hearth. A root verification error or worker panic fails the
whole build. A cancelled build publishes neither a partial symbol index nor a
partial graph. Cancellation is checked before resolver setup, during repository
completeness scans, between graph insertions, and during octorus symbol
projection. Once an analysis's imports have been copied into compact graph
edges, its original import vector is dropped before symbol projection to bound
peak retained data.

## 2. Import language matrix

| Language | Extensions | Extraction | Resolution |
|---|---|---|---|
| Rust | `.rs` | custom Hearth use-tree / `mod` walker | Hearth best-effort Rust resolver |
| TypeScript | `.ts`, `.mts`, `.cts` | JavaScript + TypeScript import queries | `oxc_resolver` |
| TSX | `.tsx` | JavaScript + TypeScript import queries | `oxc_resolver` |
| JavaScript | `.js`, `.mjs`, `.cjs` | JavaScript import query | `oxc_resolver` |
| JSX | `.jsx` | JavaScript import query | `oxc_resolver` |

All other symbol languages remain symbol-only. They are deliberately not
inserted as analyzed graph nodes: an import-unsupported Python or Markdown file
must not make every JavaScript reverse-dependency answer approximate.

Extracted JavaScript forms are static imports, re-exports, literal dynamic
imports, CommonJS `require`, and TypeScript `import = require`. A non-literal
dynamic/CommonJS argument marks its file opaque and weakens guarantees.

Rust grouped use-trees are flattened to one normalized leaf per edge. Rust
resolution does not model `cfg`, `#[path]`, macros, inline-module trees, or
Cargo targets completely, so its baseline completeness is always partial.

## 3. Resolver setup

`hearth-graph` is built with `bundled-languages`, `fs`, `resolve-js`, and
`resolve-rust` and without default features.

### JavaScript and TypeScript

Octorus selects `<root>/tsconfig.json`; if it is absent, it selects
`<root>/jsconfig.json`; if both are absent, no manual config is supplied.
`oxc_resolver` handles extension probing, package imports/exports, CommonJS
conditions, and configured path aliases. Hearth tracks config/package paths
consulted by resolution, but octorus v1 has no watcher-driven re-resolution.

Only the root config is selected explicitly. Package-local config behavior is
whatever `oxc_resolver` derives from that setup; octorus does not enumerate and
merge every workspace config.

### Rust

Listed paths ending in `src/lib.rs` or `src/main.rs` become explicit crate
roots. Hearth recognizes files under `examples`, `tests`, and `src/bin` as
implicit roots. The nearest applicable root wins, but every Rust outcome stays
partial by design.

## 4. Relative graph keys and absolute resolvers

Git and Browser state use UTF-8 repository-relative paths with `/` separators.
Both Hearth filesystem resolvers require an absolute referrer. The
`RootedResolver` adapter is the only boundary between these forms:

```text
Graph key:       packages/app/src/main.ts
Resolver input:  /canonical/root/packages/app/src/main.ts
Resolver output: /canonical/root/packages/lib/src/index.ts
Graph target:    packages/lib/src/index.ts
```

Resolved paths under the canonical repository root are stripped and rebuilt
from path components with `/` separators. Paths outside the root stay absolute.
The UI opens only targets present in the original Git listing; outside-root,
ignored/unlisted, external, and unresolved targets remain visible but are not
navigable. Membership includes listed non-source targets such as JSON and CSS,
while reverse-dependency completeness still considers only import-supported
source files.

If the repository root cannot be represented as UTF-8, the rooted resolver
returns a partial invalid-specifier failure instead of using a lossy path that
could resolve the wrong file.

## 5. Graph guarantees

Every query returns `DependencyGuarantee::Exact` or `Approximate`, projected
from Hearth's `Guarantee`.

### Direct dependencies

A file's `dependencies(path)` result is exact only when all of these hold:

- import extraction supports the file;
- no non-literal import made the file opaque;
- the matching resolver is live;
- every resolution is complete;
- edges were resolved at the current resolver generation.

A normal JS/TS file with no opaque imports can therefore be exact. Rust is
always approximate because its resolver baseline is partial.

### Reverse dependencies

`dependents(path)` additionally needs a complete source universe and every
graph node to be exact. Octorus marks the source universe complete only when:

- the Git listing was not truncated at 200,000 entries;
- no non-UTF-8 path was omitted; and
- every listed import-supported path produced a `FileAnalysis`.

An oversized, missing, unreadable, or sparse import-supporting file makes the
universe partial. A resolved target absent from analysis remains a stub and also
prevents exact reverse answers. Octorus does not expose a stub's empty outgoing
edge vector as “No imports”: both query directions return unavailable for that
path because its source was never analyzed. Any Rust analyzed node makes
root-wide reverse answers approximate under Hearth 0.1.1.

Guarantees describe structural completeness, not runtime freshness. The browser
builds once per session and does not yet refresh after filesystem or resolver
configuration changes.

## 6. Browser state and interaction

The repository graph lifecycle is independent state, although the same worker
transitions it together with symbols:

```text
ModuleGraphState
    Idle → Building → Ready(Arc<ModuleGraph>) | Failed
```

Visibility and request progress for the right-side pane are a second typed
lifecycle. They are not modal overlays:

```text
ModuleGraphPaneState
    Closed ── i ──→ Loading { request_id, path } → Ready(ModuleGraphPanel)
       ▲                   ▲                            │
       │                   └─ Waiting { path } ← path ─┘
       └──────────── close/error from every visible state
```

`Waiting` coalesces rapid file navigation. It cancels the old graph request but
does not start another until the latest background file load succeeds. The
existing superseded-file cancellation therefore prevents intermediate paths
from launching Hearth queries whose internal materialize/sort phase cannot be
interrupted in 0.1.1.

`AppState::RepoBrowseGraph` represents graph-pane focus. Tree and code remain
visible while the pane is loading or ready. With a closed pane, the `i` action
checks conditions in this order:

1. open file still loading → `Still opening this file`;
2. graph idle/building → `Module graph is still building`;
3. graph failed → `Module graph is unavailable`;
4. unsupported file → `Import analysis is not supported for this file`;
5. supported but skipped file → `Import analysis is unavailable for this file`;
6. otherwise enter `ModuleGraphPaneState::Loading`, focus
   `AppState::RepoBrowseGraph`, and start a request-scoped `spawn_blocking`
   query;
7. install `ModuleGraphPaneState::Ready` only when request id, requested path,
   and current open path still match.

Pressing `i` while the pane is already visible only focuses it; pressing `i`
from graph focus closes it and cancels any request. `Esc`/`q` returns focus to
code without hiding or cancelling the pane. Opening another file while the pane is visible cancels the old query and enters
`Waiting`; only the final successfully loaded file starts a new query. A file
load failure, unsupported path, or unavailable analysis closes the pane.
Closing the browser cancels the browser session token, so a late delivery cannot
be installed.

Direct and reverse nodes are materialized on the blocking worker, never on the
input or draw thread. Octorus retains at most 200 nodes per direction and keeps
the pre-truncation total so the title can report `3/5000 exact`. Each source
component is capped at 512 Unicode scalars. The precomputed node and edge labels
are each capped at 240 terminal cells before retention.

The pane visualizes one direction at a time. The current module uses a
UML-style double-line box; each direct relationship uses a single-line module
box and a directional connector (`├─▶` for imports, `▲─┤` for importers).
Package, unresolved, and unlisted nodes remain in the diagram but are not
navigable.

| Key | Behavior |
|---|---|
| `i` from file | Open or focus the graph pane |
| `l` / `→` from file | Focus an already-visible graph pane |
| `Tab`, `h/l`, `←/→` | Switch Imports / Imported by |
| `j/k`, `↑/↓` | Select a module node |
| `Enter` | Open a listed local target/importer |
| `Esc` / `q` | Return focus to code, keeping the pane visible |
| `i` from graph | Close the pane and return to code |

Outgoing jumps open the target at its first line. Incoming jumps open the
importer at the extracted import line. Both push the existing browser jump
stack, so `Ctrl-o` restores the exact source line and scroll position.

## 7. Performance

`benches/module_graph.rs` measures the octorus boundary:

- combined symbol/import/graph build for 100 and 1,000 TypeScript files;
- direct dependency query on a 5,000-file chain;
- reverse dependency query on the same graph;
- reverse dependency query for a 5,000-importer high-fan-in module.

Run:

```bash
cargo bench --bench module_graph
```

The renderer is protected structurally rather than by this graph benchmark:
`ModuleGraphPanel` owns at most 200 precomputed nodes per direction, with
separately bounded 240-cell node and edge labels. `render_module_graph_ready_pane`
applies `skip(offset).take(visible_nodes)` and allocates only the three UML text
lines for each visible node; it never walks Hearth or formats hidden rows.

Hearth 0.1.1 has no bounded direct/reverse query API. It clones and sorts the
full edge result before octorus can truncate it. Running that work in
`spawn_blocking` prevents input/render stalls, and post-query truncation bounds
labels and retained panel memory, but background CPU and temporary Hearth query
memory still scale with actual fan-in. The high-fan-in benchmark tracks this
upstream boundary explicitly.

## 8. Tests

| Location | Coverage |
|---|---|
| `src/module_graph.rs` | import kinds/spans, JS/TS aliases/packages, Rust resolution, direct/reverse guarantees, stub/listed non-source availability, high fan-in bounds, observable cancellation, import-vector release |
| `src/code_index.rs` / `src/symbols.rs` | one completed result containing symbols and graph edges, empty files, cancellation through post-analysis projection |
| `src/app/browse.rs` | listing completeness, combined/query failure lifecycles, actual 250-importer panel limits, Unicode label bounds |
| `src/app/input_browse.rs` | waiting/loading/open/switch/jump/back/CJK JSON/non-navigable/empty/pending/cancel/stale/file-failure scenarios, single and sequence bindings |
| `src/ui/browse.rs` | waiting/loading/outgoing/truncated/incoming/long-CJK/approximate/empty/tiny-terminal inline snapshots |
| `src/config/mod.rs` | default, serialization, overlap validation, one-diagnostic ownership, and round-trip |

## 9. Known limitations

- the UML pane shows only the current module and one direct direction at a time; there is no transitive neighborhood, pan, zoom, or automatic graph layout;
- no import-aware symbol disambiguation for `gd`;
- no alias or re-export chain mapped to an exact destination symbol;
- no incremental upsert/removal from filesystem watcher events;
- no resolver-cache clearing or graph re-resolution after config changes;
- Rust resolution remains approximate until Hearth gains Cargo target/module-tree metadata;
- only a root `tsconfig.json` / `jsconfig.json` is selected explicitly;
- outside-root and unlisted resolved paths are displayed but cannot be opened;
- Hearth 0.1.1 queries materialize and sort every matching edge before octorus can retain only the first 200; this work is off the UI thread but its temporary cost is not bounded;
- the header still reports only symbol-index lifecycle; graph failures are exposed through the footer/action path.
