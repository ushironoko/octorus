# Symbol Index — Technical Reference

The Repository Browser symbol engine is exposed by `src/symbols.rs`, an
octorus-owned compatibility facade over the exact crates.io dependency
`hearth-graph = "=0.1.1"`.

Read this document when changing the facade, adding a symbol language, or
upgrading Hearth. The browser architecture and task lifecycle remain documented
in [`repo-browse-architecture.md`](repo-browse-architecture.md).

## 1. Ownership and data flow

Hearth owns parsing, tags-query execution, duplicate collapse, nesting,
repository index construction, search scoring, search memoization, and
definition ranking. octorus owns the public API consumed by the browser:

```text
source + path
    │
    ▼
octorus ParserPool ──borrows──> hearth_graph::ParserPool
    │
    ▼
hearth_graph::extract_symbols
    │  CompactString / u32 / u16 + byte ranges
    ▼
src/symbols.rs compatibility conversion
    │  String / usize
    ▼
Vec<octorus::symbols::Symbol>
```

The public symbols-only compatibility build follows the same boundary:

```text
repo root + paths + local CancelSignal
    │
    ├─ FsLoader / BuildOptions / cancellation closure
    ▼
hearth_graph::build_index
    │
    ▼
Hearth SymbolIndex + octorus projection
    │
    ├─ definitions(name)
    ├─ search(query, limit)
    └─ file_symbols(path)
```

The Repository Browser itself uses `src/code_index.rs` instead: one
`hearth_graph::analyze_paths` pass extracts symbols and imports from the same
syntax trees, then feeds `SymbolIndex::from_analyses_cancellable` and the module
graph.
Never replace that path with separate `build_index` + `analyze_paths` calls;
that would parse the repository twice. Import ownership and guarantees are in
[`module-graph.md`](module-graph.md).

The facade deliberately prevents Hearth storage choices from leaking into
browser state, tests, or benchmarks. The previously public
`SupportedLanguage::tags_query` and `ParserPool::get_or_create_tags_query`
methods remain available as compatibility accessors; both resolve through this
same registry and Hearth query cache rather than restoring local query
ownership.

## 2. Compatibility types and conversion rules

The octorus-facing types remain:

```rust
Symbol {
    name: String,
    kind: SymbolKind,
    line: usize,
    column: usize,
    depth: usize,
}

FileSymbols {
    path: String,
    symbols: Vec<Symbol>,
}
```

Hearth stores `CompactString`, `u32` line/column/byte offsets, and `u16` depth.
Conversions from Hearth widen into octorus types. Conversions from synthetic or
host-provided octorus fixtures use `u32::try_from` / `u16::try_from` and panic
with a stable explanation when a value cannot be represented. Never replace
these with `as`: truncation must not differ between debug and release builds.

`SymbolIndex` retains both:

- the Hearth index used for every query;
- an octorus-owned projection used to return `SymbolRef<'_>` with the original
  `&str` / `&Symbol` API.

Hearth query refs point into stable per-file symbol vectors. The facade records
each Hearth symbol address in an `FxHashMap` whose value is the parallel
`(file, symbol)` position, so hits are projected in O(1) without comparing names,
hashing paths, or scanning a file outline. `SymbolIndex::from_files` fixtures
have no source ranges, so their source-order position becomes a coherent
synthetic one-byte range.

## 3. Language registry

`symbol_language_registry()` starts with `LanguageRegistry::bundled()` and then
applies two host registrations through the public API:

```rust
LanguageSpec::new("c", tree_sitter_c::LANGUAGE.into(), ["c", "h"])
    .with_tags_query(tree_sitter_c::TAGS_QUERY)
    .with_merge_adjacent_same_name_definitions(true)

LanguageSpec::new("moonbit", tree_sitter_moonbit::LANGUAGE.into(), ["mbt"])
    .with_tags_query(MOONBIT_TAGS_QUERY)
```

Last registration wins for an extension. The C override preserves the former
octorus behavior for function-like macro definitions whose query captures the
same macro identifier on adjacent definitions; ordinary non-adjacent symbols
remain distinct. MoonBit remains an octorus host grammar without requiring a
private Hearth field or a struct literal. `LanguageSpec` is non-exhaustive;
always use its constructor/builders.

### Bundled by Hearth

| Language | Extensions used for symbols | Query ownership |
|---|---|---|
| Rust | `.rs` | Hearth / grammar crate |
| TypeScript | `.ts`, `.mts`, `.cts` | Hearth combines JavaScript + TypeScript |
| TSX | `.tsx` | Hearth combines JavaScript + TypeScript |
| JavaScript | `.js`, `.mjs`, `.cjs` | Hearth / grammar crate |
| JSX | `.jsx` | Hearth / grammar crate |
| Go | `.go` | Hearth / grammar crate |
| Python | `.py` | Hearth / grammar crate |
| Ruby | `.rb`, `.rake`, `.gemspec` | Hearth / grammar crate |
| C | `.c`, `.h` | grammar-crate query, re-registered by octorus to enable legacy adjacent-definition merging |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx` | Hearth / grammar crate |
| Java | `.java` | Hearth / grammar crate |
| C# | `.cs` | Hearth-bundled query |
| Zig | `.zig` | Hearth-bundled query |
| Bash | `.sh`, `.bash`, `.zsh` | Hearth-bundled query |
| Haskell | `.hs`, `.lhs` | Hearth-bundled query |
| Lua | `.lua` | Hearth / grammar crate |
| PHP | `.php` | Hearth / grammar crate |
| Swift | `.swift` | Hearth / grammar crate |
| Vue | `.vue` | Hearth-bundled injections query — `<script>` blocks are parsed as embedded TypeScript/JavaScript, symbols keep whole-file line/column |
| Markdown | `.md`, `.markdown` | Hearth-bundled query |

### Registered by octorus

| Language | Extensions | Query ownership |
|---|---|---|
| MoonBit | `.mbt` | `src/queries/moonbit/tags.scm` |

Since hearth-graph 0.2.0, Vue is a bundled symbol language: it has no tags
query of its own, so `test_all_registered_symbol_queries_compile` accepts an
injections query in place of one. Svelte, CSS, and MarkdownInline remain
highlighting/injection languages, not standalone symbol languages. Their
handling in `SupportedLanguage` is independent of the Hearth symbol registry.

## 4. Parser and query reuse

The existing `crate::syntax::ParserPool` still owns highlighting parsers and
queries. It now also contains a `hearth_graph::ParserPool<'static>` tied to the
static symbol registry. `extract_symbols` borrows that cache rather than
constructing a registry, parser, or query for each call.

Repository builds use Hearth's worker-local parser pools. Both the public
symbols-only build and the Browser's combined symbol/import build remain
blocking and CPU-bound and must be called from `spawn_blocking`, never from the
draw loop.

## 5. Extraction behavior and grammar quirks

These behaviors are implemented upstream but remain user-visible contracts for
octorus.

### TypeScript inherits JavaScript tags

The TypeScript tags query contains only TypeScript-specific patterns. Hearth
combines it with the JavaScript query; otherwise ordinary classes and functions
would disappear. C++ is the opposite: its tags query is self-contained and must
not be concatenated with C.

### Rust duplicate captures

A function inside an `impl` can match both `@definition.method` and
`@definition.function`. Hearth collapses captures by the name node byte offset,
preferring the more specific kind and then the narrower definition span.
`impl Foo` itself remains an upstream reference rather than an outline
definition, so methods appear at top level.

### Haskell equations

Haskell is registered with
`with_merge_adjacent_same_name_definitions(true)`. Consecutive equations such as
`describe 0 = ...` and `describe value = ...` become one logical symbol.
Ordinary sibling overloads in other languages are preserved.

### Markdown nesting

Markdown headings capture their enclosing `section`, not only the heading node.
That gives `##` and `###` their expected outline depth.

### Character columns

tree-sitter reports byte columns. Hearth converts the prefix to characters, so
CJK and accented identifiers agree with octorus cursor coordinates. The facade
widens the resulting `u32` character column to `usize`.

### Deterministic ordering

Hearth sorts raw tags by definition start, outer definition first, line, name,
and finally name-byte offset. The final key is an intentional difference from
the former `HashMap` iteration corner case and guarantees deterministic output.

## 6. Search and definitions

`SymbolIndex::search(query, limit)` delegates ranking and top-N selection to
Hearth. The observable tiers remain:

| Tier | Example for `parse` |
|---|---|
| Exact | `parse` |
| Prefix | `parse_line` |
| Boundary substring | `do_parse` |
| Other substring | `reparsed` |
| Subsequence | `please_advance_rest_of_set` |

Ties use shorter name, path, line, and stable index position. Hearth performs a
top-N partial selection before sorting and memoizes the latest `(needle, limit)`
pair. The octorus facade only projects the returned refs; it must not rescore or
resort them.

`definitions(name)` remains case-insensitive and prefers types, then callables,
then constants/modules, then fields/properties/headings, followed by depth,
path, and line.

## 7. Repository build and cancellation

The compatibility entry point remains:

```rust
SymbolIndex::build_cancellable(
    repo_root: &Path,
    paths: &[String],
    cancel: &dyn CancelSignal,
) -> IndexBuild
```

The local trait is adapted with a closure, so browser code does not expose
Hearth's cancellation trait. `CancellationToken` continues to implement the
local seam.

Build options retain the octorus limits:

- maximum file size: `MAX_INDEXED_FILE_BYTES = 2 MiB`;
- maximum workers: 8;
- maximum symbols per file: Hearth's `MAX_SYMBOLS_PER_FILE = 10,000`.

Outcomes remain `Completed`, `Cancelled { scanned_files }`, and
`Failed { message }`. Unsupported, oversized, missing, and unreadable individual
files are skipped. Root verification and worker panics are failures.

`CodeIndex::build_cancellable` adapts the same local cancellation seam to
`analyze_paths`. It polls cancellation again while folding completed analyses
into graph nodes. Cancellation at either stage publishes neither index; the UI
channel has no partial-result variant.

## 8. Intentional compatibility differences

These differences are accepted by issue #177 and must remain explicit:

- duplicate input paths are last-wins;
- successfully analyzed files with no symbols remain addressable as `Some(&[])`;
- Hearth uses `parking_lot`, so there is no mutex-poison recovery contract;
- raw symbol ordering includes the name-byte tie breaker;
- `.mjs`, `.cjs`, `.mts`, and `.cts` are symbol-indexable;
- root error wording may refer to a source root rather than a repository root;
- direct extraction of source larger than `u32::MAX` bytes returns no symbols;
- checked facade narrowing rejects synthetic values outside Hearth's integer
  ranges instead of truncating them.

Any additional divergence requires a compatibility test and an update here.

## 9. Benchmarks

`benches/symbol_index.rs` measures the public facade, not Hearth directly:

- `extract_symbols_rust/{10,50,200,1000}`;
- `extract_symbols_language/{rust,typescript,markdown}`;
- `from_files/{100,1000,5000}`;
- `query/{definitions_hit,definitions_miss,search_cached/*,search_cold/*}`.

`search_cached` includes Hearth cache lookup plus octorus ref projection.
`search_cold` alternates the limit to force rescoring. Do not compare these two
as if they measured the same path.

Run:

```bash
cargo bench --bench symbol_index
```

The Browser's combined parse/build boundary and module queries are measured
separately by `benches/module_graph.rs`; see
[`module-graph.md`](module-graph.md#7-performance).

Historic timings from the octorus-owned implementation are not baseline values
for the facade because conversion and projection changed the measured boundary.
Record new numbers only from a reproducible Criterion run and include the exact
Hearth version.

## 10. Adding a symbol language

1. Prefer adding the grammar/query to Hearth when it is generally useful.
2. For an octorus-only grammar, add the grammar dependency and query asset.
3. Register it after `LanguageRegistry::bundled()` with `LanguageSpec` builders.
4. Add extraction and `supports_symbols` compatibility tests.
5. Confirm every registered query compiles through Hearth's `ParserPool`.
6. Update the language tables and run `cargo bench --bench symbol_index`.

Do not restore tags-query ownership to `SupportedLanguage`; highlighting and
symbol registration intentionally have separate owners now.

## 11. Relationship to `src/symbol.rs`

`src/symbol.rs` still provides the lightweight keyword/grep navigation used by
the diff view. `src/symbols.rs` is the repository-wide CST index used by Repo
Browse (`gd`, outline, and symbol search). Replacing the former with the latter
would remove the diff view's instant no-index fallback and is outside this
integration.
