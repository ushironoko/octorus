//! Compatibility facade over the `hearth-graph` symbol engine.
//!
//! octorus keeps its original `String` / `usize` public types so the browser,
//! tests, and benchmarks do not depend on Hearth's compact storage choices.
//! Extraction, indexing, search, definition lookup, and repository walking are
//! delegated to Hearth; all representation conversion stays in this module.

use std::path::Path;
use std::sync::LazyLock;

use hearth_graph::{
    BuildOptions, FileAnalysis as HearthFileAnalysis, FileSymbols as HearthFileSymbols, FsLoader,
    IndexBuild as HearthIndexBuild, LanguageId, LanguageRegistry, LanguageSpec,
    Symbol as HearthSymbol, SymbolIndex as HearthSymbolIndex, SymbolKind as HearthSymbolKind,
    SymbolRef as HearthSymbolRef,
};
use rustc_hash::FxHashMap;

use crate::syntax::ParserPool;

/// Files larger than this are skipped when indexing.
pub const MAX_INDEXED_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum number of symbols retained per file.
pub const MAX_SYMBOLS_PER_FILE: usize = hearth_graph::MAX_SYMBOLS_PER_FILE;

const MOONBIT_TAGS_QUERY: &str = include_str!("queries/moonbit/tags.scm");

/// Hearth's bundled registry plus the host-owned MoonBit grammar.
static SYMBOL_LANGUAGES: LazyLock<LanguageRegistry> = LazyLock::new(|| {
    let mut registry = LanguageRegistry::bundled();

    // The former octorus extractor merged adjacent same-name definitions for
    // every language. C needs that compatibility behavior for function-like
    // macro declarations such as tree_sitter_external_scanner(reset), whose
    // tags query captures the macro identifier for each generated function.
    registry.register(
        LanguageSpec::new("c", tree_sitter_c::LANGUAGE.into(), ["c", "h"])
            .with_tags_query(tree_sitter_c::TAGS_QUERY)
            .with_merge_adjacent_same_name_definitions(true),
    );
    registry.register(
        LanguageSpec::new("moonbit", tree_sitter_moonbit::LANGUAGE.into(), ["mbt"])
            .with_tags_query(MOONBIT_TAGS_QUERY),
    );
    registry
});

/// Registry shared by the facade and the symbol parser cache.
pub(crate) fn symbol_language_registry() -> &'static LanguageRegistry {
    &SYMBOL_LANGUAGES
}

/// Resolve an extension to its last-registered symbol language.
///
/// Iterating rather than constructing a synthetic path keeps compatibility
/// accessors allocation-free while preserving the registry's last-wins rule.
pub(crate) fn symbol_language_id(extension: &str) -> Option<LanguageId> {
    let mut found = None;
    for (id, spec) in SYMBOL_LANGUAGES.iter() {
        if spec
            .extensions
            .iter()
            .any(|candidate| candidate.as_str() == extension)
        {
            found = Some(id);
        }
    }
    found
}

/// A cooperative cancellation signal used by octorus background work.
pub trait CancelSignal: Sync {
    fn is_cancelled(&self) -> bool;
}

impl CancelSignal for tokio_util::sync::CancellationToken {
    fn is_cancelled(&self) -> bool {
        tokio_util::sync::CancellationToken::is_cancelled(self)
    }
}

/// The kind of a named entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Interface,
    Module,
    Macro,
    Constant,
    Type,
    Field,
    Property,
    /// Markdown heading — the outline of a prose document.
    Heading,
}

impl SymbolKind {
    /// Single-character glyph used in outline and search rows.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Function => "ƒ",
            Self::Method => "m",
            Self::Class => "C",
            Self::Interface => "I",
            Self::Module => "M",
            Self::Macro => "!",
            Self::Constant => "c",
            Self::Type => "T",
            Self::Field => "f",
            Self::Property => "p",
            Self::Heading => "#",
        }
    }
}

impl From<HearthSymbolKind> for SymbolKind {
    fn from(kind: HearthSymbolKind) -> Self {
        match kind {
            HearthSymbolKind::Function => Self::Function,
            HearthSymbolKind::Method => Self::Method,
            HearthSymbolKind::Class => Self::Class,
            HearthSymbolKind::Interface => Self::Interface,
            HearthSymbolKind::Module => Self::Module,
            HearthSymbolKind::Macro => Self::Macro,
            HearthSymbolKind::Constant => Self::Constant,
            HearthSymbolKind::Type => Self::Type,
            HearthSymbolKind::Field => Self::Field,
            HearthSymbolKind::Property => Self::Property,
            HearthSymbolKind::Heading => Self::Heading,
        }
    }
}

impl From<SymbolKind> for HearthSymbolKind {
    fn from(kind: SymbolKind) -> Self {
        match kind {
            SymbolKind::Function => Self::Function,
            SymbolKind::Method => Self::Method,
            SymbolKind::Class => Self::Class,
            SymbolKind::Interface => Self::Interface,
            SymbolKind::Module => Self::Module,
            SymbolKind::Macro => Self::Macro,
            SymbolKind::Constant => Self::Constant,
            SymbolKind::Type => Self::Type,
            SymbolKind::Field => Self::Field,
            SymbolKind::Property => Self::Property,
            SymbolKind::Heading => Self::Heading,
        }
    }
}

/// A named entity in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// 1-based line of the symbol's name.
    pub line: usize,
    /// 0-based column (in characters) of the symbol's name.
    pub column: usize,
    /// Nesting depth of the enclosing definitions — 0 for top level.
    pub depth: usize,
}

/// Symbols of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSymbols {
    pub path: String,
    pub symbols: Vec<Symbol>,
}

/// A symbol together with the file it lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolRef<'a> {
    pub path: &'a str,
    pub symbol: &'a Symbol,
}

impl SymbolRef<'_> {
    /// Single source of truth for a symbol-search result row.
    pub fn search_label(&self) -> String {
        format!(
            "{} {}  {}:{}",
            self.symbol.kind.glyph(),
            self.symbol.name,
            self.path,
            self.symbol.line
        )
    }
}

/// Whether symbol extraction is possible for the given filename.
pub fn supports_symbols(filename: &str) -> bool {
    symbol_language_registry().supports_symbols(Path::new(filename))
}

/// Extract symbols through Hearth while preserving the octorus return type.
pub fn extract_symbols(source: &str, filename: &str, pool: &mut ParserPool) -> Vec<Symbol> {
    hearth_graph::extract_symbols(source, filename, pool.symbol_parser_pool())
        .iter()
        .map(symbol_from_hearth)
        .collect()
}

fn symbol_from_hearth(symbol: &HearthSymbol) -> Symbol {
    Symbol {
        name: symbol.name.to_string(),
        kind: symbol.kind.into(),
        line: usize::try_from(symbol.line)
            .expect("hearth-graph symbol line does not fit octorus usize"),
        column: usize::try_from(symbol.column)
            .expect("hearth-graph symbol column does not fit octorus usize"),
        depth: usize::from(symbol.depth),
    }
}

fn symbol_to_hearth(symbol: Symbol, position: usize) -> HearthSymbol {
    let name_start =
        u32::try_from(position).expect("symbol position exceeds hearth-graph u32 range");
    let def_end = name_start
        .checked_add(1)
        .expect("symbol position exceeds hearth-graph u32 range");

    HearthSymbol {
        name: symbol.name.into(),
        kind: symbol.kind.into(),
        line: u32::try_from(symbol.line).expect("symbol line exceeds hearth-graph u32 range"),
        column: u32::try_from(symbol.column).expect("symbol column exceeds hearth-graph u32 range"),
        depth: u16::try_from(symbol.depth).expect("symbol depth exceeds hearth-graph u16 range"),
        // `from_files` fixtures have no source byte ranges. Hearth's index
        // only needs a stable per-file identity here, so the source-order
        // position provides a coherent synthetic one-byte range.
        name_start,
        def_start: name_start,
        def_end,
    }
}

fn file_to_hearth(file: FileSymbols, position: usize) -> HearthFileSymbols {
    let content_hash = u64::try_from(position)
        .expect("file position exceeds hearth-graph content hash range")
        .checked_add(1)
        .expect("file position exceeds hearth-graph content hash range");
    HearthFileSymbols {
        path: file.path.into(),
        content_hash,
        symbols: file
            .symbols
            .into_iter()
            .enumerate()
            .map(|(position, symbol)| symbol_to_hearth(symbol, position))
            .collect(),
    }
}

/// Outcome of a cancellable repository-wide index build.
#[derive(Debug)]
pub enum IndexBuild {
    Completed(SymbolIndex),
    Cancelled { scanned_files: usize },
    Failed { message: String },
}

#[derive(Debug)]
struct ProjectedFile {
    file: FileSymbols,
}

/// A repository-wide symbol index backed by Hearth.
#[derive(Debug)]
pub struct SymbolIndex {
    // Keep the compatibility wrapper small enough to move through the existing
    // `IndexBuild` / `IndexDelivery` state-machine variants without inflating
    // every cancellation and failure value to Hearth's full index size.
    inner: Box<HearthSymbolIndex>,
    files: Vec<ProjectedFile>,
    file_by_path: FxHashMap<String, usize>,
    /// Hearth symbol address -> (projected file, projected symbol).
    ///
    /// Hearth refs point into stable per-file vectors. One fast integer lookup
    /// avoids hashing both path strings and name offsets for every one of the
    /// 200 rows rebuilt by a cached search on each render frame.
    symbol_projection: FxHashMap<usize, (usize, usize)>,
}

impl Default for SymbolIndex {
    fn default() -> Self {
        Self::from_hearth(HearthSymbolIndex::new())
    }
}

impl SymbolIndex {
    /// Build an index from already-extracted octorus symbols.
    pub fn from_files(files: Vec<FileSymbols>) -> Self {
        let files = files
            .into_iter()
            .enumerate()
            .map(|(position, file)| file_to_hearth(file, position))
            .collect();
        Self::from_hearth(HearthSymbolIndex::from_files(
            files,
            symbol_language_registry().generation(),
        ))
    }

    /// Consume the symbol half of Hearth's shared symbol/import analyses.
    pub(crate) fn from_analyses_cancellable(
        files: Vec<HearthFileAnalysis>,
        cancel: &dyn CancelSignal,
    ) -> Option<Self> {
        let mut symbol_files = Vec::with_capacity(files.len());
        for (index, analysis) in files.into_iter().enumerate() {
            if index.is_multiple_of(1_024) && cancel.is_cancelled() {
                return None;
            }
            symbol_files.push(HearthFileSymbols {
                path: analysis.path,
                content_hash: analysis.content_hash,
                symbols: analysis.symbols,
            });
        }
        if cancel.is_cancelled() {
            return None;
        }
        let inner =
            HearthSymbolIndex::from_files(symbol_files, symbol_language_registry().generation());
        Self::project_hearth(inner, Some(cancel))
    }

    /// Build an index by reading and parsing repository-relative paths.
    ///
    /// Blocking and CPU-bound — call from `spawn_blocking`.
    pub fn build_cancellable(
        repo_root: &Path,
        paths: &[String],
        cancel: &dyn CancelSignal,
    ) -> IndexBuild {
        let loader = FsLoader::new(repo_root);
        let cancellation = || cancel.is_cancelled();
        let options = BuildOptions {
            max_file_bytes: MAX_INDEXED_FILE_BYTES,
            max_workers: 8,
        };

        match hearth_graph::build_index(
            symbol_language_registry(),
            &loader,
            paths,
            &cancellation,
            &options,
        ) {
            HearthIndexBuild::Completed(index) => IndexBuild::Completed(Self::from_hearth(index)),
            HearthIndexBuild::Cancelled { scanned_files } => {
                IndexBuild::Cancelled { scanned_files }
            }
            HearthIndexBuild::Failed { message } => IndexBuild::Failed { message },
        }
    }

    fn from_hearth(inner: HearthSymbolIndex) -> Self {
        Self::project_hearth(inner, None).expect("uncancellable symbol projection was cancelled")
    }

    fn project_hearth(inner: HearthSymbolIndex, cancel: Option<&dyn CancelSignal>) -> Option<Self> {
        let mut files = Vec::with_capacity(inner.file_count());
        let mut file_by_path = FxHashMap::default();
        file_by_path.reserve(inner.file_count());
        let mut symbol_projection = FxHashMap::default();
        symbol_projection.reserve(inner.symbol_count());
        let mut projected_symbols = 0_usize;

        for (path_position, path) in inner.paths().enumerate() {
            if path_position.is_multiple_of(1_024) && cancel.is_some_and(CancelSignal::is_cancelled)
            {
                return None;
            }
            let hearth_symbols = inner
                .file_symbols(path)
                .expect("hearth-graph path iterator returned a missing file");
            let file_position = files.len();
            let mut symbols = Vec::with_capacity(hearth_symbols.len());

            for (symbol_position, symbol) in hearth_symbols.iter().enumerate() {
                if projected_symbols.is_multiple_of(1_024)
                    && cancel.is_some_and(CancelSignal::is_cancelled)
                {
                    return None;
                }
                projected_symbols += 1;
                let key = std::ptr::from_ref(symbol) as usize;
                let previous = symbol_projection.insert(key, (file_position, symbol_position));
                assert!(
                    previous.is_none(),
                    "hearth-graph returned aliased symbol storage for {path}"
                );
                symbols.push(symbol_from_hearth(symbol));
            }

            let path = path.to_owned();
            let previous = file_by_path.insert(path.clone(), file_position);
            assert!(
                previous.is_none(),
                "hearth-graph returned duplicate indexed path {path}"
            );
            files.push(ProjectedFile {
                file: FileSymbols { path, symbols },
            });
        }

        if cancel.is_some_and(CancelSignal::is_cancelled) {
            return None;
        }
        Some(Self {
            inner: Box::new(inner),
            files,
            file_by_path,
            symbol_projection,
        })
    }

    /// Total number of indexed symbols.
    pub fn symbol_count(&self) -> usize {
        self.inner.symbol_count()
    }

    /// Number of successfully indexed files, including files with no symbols.
    #[cfg(test)]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Number of indexable inputs whose load was attempted.
    #[cfg(test)]
    pub fn scanned_file_count(&self) -> usize {
        self.inner.scanned_file_count()
    }

    /// Test-only empty-state probe.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.symbol_count() == 0
    }

    /// Symbols of a single indexed file, including `Some(&[])` for an empty file.
    pub fn file_symbols(&self, path: &str) -> Option<&[Symbol]> {
        let position = *self.file_by_path.get(path)?;
        Some(self.files[position].file.symbols.as_slice())
    }

    /// All definitions with exactly this name, case-insensitively.
    pub fn definitions(&self, name: &str) -> Vec<SymbolRef<'_>> {
        self.inner
            .definitions(name)
            .into_iter()
            .map(|hit| self.project_ref(hit))
            .collect()
    }

    /// Fuzzy-search symbol names, best match first, capped at `limit`.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SymbolRef<'_>> {
        self.inner
            .search(query, limit)
            .into_iter()
            .map(|hit| self.project_ref(hit))
            .collect()
    }

    fn project_ref(&self, hit: HearthSymbolRef<'_>) -> SymbolRef<'_> {
        let key = std::ptr::from_ref(hit.symbol) as usize;
        let &(file_position, symbol_position) = self
            .symbol_projection
            .get(&key)
            .expect("Hearth search returned an unprojected symbol");
        let projected = &self.files[file_position];
        let symbol = &projected.file.symbols[symbol_position];
        debug_assert_eq!(symbol.name, hit.symbol.name.as_str());
        debug_assert_eq!(
            symbol.line,
            usize::try_from(hit.symbol.line)
                .expect("hearth-graph symbol line does not fit octorus usize")
        );

        SymbolRef {
            path: &projected.file.path,
            symbol,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use tokio_util::sync::CancellationToken;

    struct PollCountCancel {
        limit: usize,
        polls: AtomicUsize,
    }

    impl CancelSignal for PollCountCancel {
        fn is_cancelled(&self) -> bool {
            self.polls.fetch_add(1, Ordering::SeqCst) >= self.limit
        }
    }

    #[test]
    fn test_combined_symbol_projection_observes_cancellation_between_files() {
        let files = (0..2_048)
            .map(|index| HearthFileAnalysis {
                path: format!("src/file_{index:04}.ts").into(),
                content_hash: index as u64,
                language: Some("typescript".into()),
                symbols: Vec::new(),
                imports: Vec::new(),
                has_opaque_imports: false,
            })
            .collect();
        let cancel = PollCountCancel {
            limit: 1,
            polls: AtomicUsize::new(0),
        };

        assert!(SymbolIndex::from_analyses_cancellable(files, &cancel).is_none());
        assert!(cancel.polls.load(Ordering::SeqCst) >= 2);
    }

    fn outline(source: &str, filename: &str) -> Vec<(String, SymbolKind, usize, usize)> {
        let mut pool = ParserPool::new();
        extract_symbols(source, filename, &mut pool)
            .into_iter()
            .map(|symbol| (symbol.name, symbol.kind, symbol.line, symbol.depth))
            .collect()
    }

    fn names(source: &str, filename: &str) -> Vec<String> {
        let mut pool = ParserPool::new();
        extract_symbols(source, filename, &mut pool)
            .into_iter()
            .map(|symbol| symbol.name)
            .collect()
    }

    #[test]
    fn test_all_registered_symbol_queries_compile() {
        let registry = symbol_language_registry();
        let mut pool = hearth_graph::ParserPool::new(registry);

        for (id, spec) in registry.iter() {
            if spec.tags_query.is_some() {
                assert!(
                    pool.tags_query(id).is_some(),
                    "tags query for {} failed to compile",
                    spec.name
                );
            } else {
                assert!(
                    spec.injections_query.is_some(),
                    "{} has neither a tags query nor an injections query",
                    spec.name
                );
                assert!(
                    pool.injections_query(id).is_some(),
                    "injections query for {} failed to compile",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn test_extract_rust_outline() {
        let source = "\
pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new() -> Self {
        Self { name: String::new() }
    }
}

fn helper() {}
";
        insta::assert_debug_snapshot!(outline(source, "src/config.rs"), @r#"
        [
            (
                "Config",
                Class,
                1,
                0,
            ),
            (
                "new",
                Method,
                6,
                0,
            ),
            (
                "helper",
                Function,
                11,
                0,
            ),
        ]
        "#);
    }

    #[test]
    fn test_extract_vue_script_outline() {
        let source = "\
<template>
  <p>{{ label }}</p>
</template>
<script setup lang=\"ts\">
export interface Props {
  label: string
}

export function useCounter() {}
</script>
";
        insta::assert_debug_snapshot!(outline(source, "src/Counter.vue"), @r#"
        [
            (
                "Props",
                Interface,
                5,
                0,
            ),
            (
                "useCounter",
                Function,
                9,
                0,
            ),
        ]
        "#);
    }

    #[test]
    fn test_extract_typescript() {
        let source = "\
export interface Props { a: number }
export class Widget {
  render(): void {}
}
export function setup() {}
";
        assert_eq!(
            names(source, "src/widget.ts"),
            ["Props", "Widget", "render", "setup"]
        );
    }

    #[test]
    fn test_extract_python() {
        let source = "\
class Loader:
    def load(self):
        pass

def main():
    pass
";
        insta::assert_debug_snapshot!(outline(source, "loader.py"), @r#"
        [
            (
                "Loader",
                Class,
                1,
                0,
            ),
            (
                "load",
                Function,
                2,
                1,
            ),
            (
                "main",
                Function,
                5,
                0,
            ),
        ]
        "#);
    }

    #[test]
    fn test_extract_go() {
        let source = "\
package main

type Config struct{}

func (c *Config) Load() {}

func main() {}
";
        assert_eq!(names(source, "main.go"), ["Config", "Load", "main"]);
    }

    #[test]
    fn test_extract_c_merges_macro_generated_function_names() {
        let scanner = include_str!("../crates/tree-sitter-moonbit/src/scanner.c");
        let generated: Vec<_> = outline(scanner, "scanner.c")
            .into_iter()
            .filter(|(name, ..)| name == "tree_sitter_external_scanner")
            .collect();

        insta::assert_debug_snapshot!(generated, @r#"
        [
            (
                "tree_sitter_external_scanner",
                Function,
                63,
                0,
            ),
            (
                "tree_sitter_external_scanner",
                Function,
                396,
                0,
            ),
        ]
        "#);
    }

    #[test]
    fn test_extract_c_sharp_preserves_overloads() {
        let source = "\
class Service {
    void Run(int x) {}
    void Run(string x) {}
}
";
        let symbols = outline(source, "Service.cs");
        let runs: Vec<_> = symbols
            .iter()
            .filter(|(name, ..)| name == "Run")
            .cloned()
            .collect();
        assert_eq!(
            runs,
            [
                ("Run".to_owned(), SymbolKind::Method, 2, 1),
                ("Run".to_owned(), SymbolKind::Method, 3, 1),
            ]
        );
    }

    #[test]
    fn test_extract_zig_uses_bundled_query() {
        let source =
            "const Point = struct { x: i32 };\npub fn add(a: i32, b: i32) i32 { return a + b; }\n";
        let names = names(source, "point.zig");
        assert!(names.contains(&"Point".to_owned()), "{names:?}");
        assert!(names.contains(&"add".to_owned()), "{names:?}");
    }

    #[test]
    fn test_extract_bash_skips_function_local_assignments() {
        let source = "TOP_LEVEL=1\n\ndeploy() {\n  local_var=2\n  echo hi\n}\n";
        assert_eq!(names(source, "deploy.sh"), ["TOP_LEVEL", "deploy"]);
    }

    #[test]
    fn test_extract_haskell_merges_adjacent_equations() {
        let source = "describe 0 = \"zero\"\ndescribe value = show value\n";
        assert_eq!(names(source, "Fixture.hs"), ["describe"]);
    }

    #[test]
    fn test_extract_markdown_headings() {
        let source = "# Title\n\nintro\n\n## Usage\n\n### Options\n";
        insta::assert_debug_snapshot!(outline(source, "README.md"), @r#"
        [
            (
                "Title",
                Heading,
                1,
                0,
            ),
            (
                "Usage",
                Heading,
                5,
                1,
            ),
            (
                "Options",
                Heading,
                7,
                2,
            ),
        ]
        "#);
    }

    #[test]
    fn test_moonbit_is_injected_through_the_host_registry() {
        let moonbit = symbol_language_registry()
            .for_path(Path::new("src/main.mbt"))
            .and_then(|id| symbol_language_registry().get(id))
            .expect("MoonBit host registration");
        assert_eq!(moonbit.name, "moonbit");
        assert!(moonbit.tags_query.is_some());
        assert_eq!(
            names("fn main {\n  println(\"hi\")\n}\n", "src/main.mbt"),
            ["main"]
        );
    }

    #[test]
    fn test_empty_and_unsupported_sources_yield_no_symbols() {
        assert!(names("", "src/lib.rs").is_empty());
        assert!(names("fn main() {}", "notes.txt").is_empty());
        assert!(names("body { color: red }", "site.css").is_empty());
        assert!(names("fn main() {}", "Makefile").is_empty());
    }

    #[test]
    fn test_syntax_errors_still_yield_recovered_symbols() {
        let names = names("fn ok() {}\nfn broken( {\n", "src/broken.rs");
        assert!(names.contains(&"ok".to_owned()), "{names:?}");
    }

    #[test]
    fn test_cjk_locations_use_character_columns() {
        let source = "class 構造体 { メソッド() {} }\n";
        let mut pool = ParserPool::new();
        let symbols = extract_symbols(source, "src/cjk.ts", &mut pool);
        let method = symbols
            .iter()
            .find(|symbol| symbol.name == "メソッド")
            .expect("method symbol");
        assert_eq!(method.column, "class 構造体 { ".chars().count());
        assert_eq!(method.column, 12);
        assert_eq!("class 構造体 { ".len(), 18);
    }

    #[test]
    fn test_supports_symbols() {
        for path in [
            "src/main.rs",
            "README.md",
            "src/module.mjs",
            "src/module.cjs",
            "src/module.mts",
            "src/module.cts",
            "src/main.mbt",
        ] {
            assert!(supports_symbols(path), "{path}");
        }
        assert!(!supports_symbols("style.css"));
        assert!(!supports_symbols("data.json"));
    }

    fn test_symbol(name: &str, kind: SymbolKind, line: usize, column: usize) -> Symbol {
        Symbol {
            name: name.to_owned(),
            kind,
            line,
            column,
            depth: 0,
        }
    }

    fn sample_index() -> SymbolIndex {
        SymbolIndex::from_files(vec![
            FileSymbols {
                path: "src/app.rs".to_owned(),
                symbols: vec![
                    test_symbol("App", SymbolKind::Class, 10, 0),
                    test_symbol("render_app", SymbolKind::Function, 20, 0),
                ],
            },
            FileSymbols {
                path: "src/ui.rs".to_owned(),
                symbols: vec![test_symbol("app", SymbolKind::Constant, 5, 0)],
            },
        ])
    }

    fn completed_index(build: IndexBuild) -> SymbolIndex {
        match build {
            IndexBuild::Completed(index) => index,
            IndexBuild::Cancelled { scanned_files } => {
                panic!("build was cancelled after scanning {scanned_files} files")
            }
            IndexBuild::Failed { message } => panic!("build failed: {message}"),
        }
    }

    #[test]
    fn test_index_counts_and_empty_state() {
        let index = sample_index();
        assert_eq!(index.file_count(), 2);
        assert_eq!(index.symbol_count(), 3);
        assert!(!index.is_empty());

        let empty = SymbolIndex::default();
        assert!(empty.is_empty());
        assert!(empty.definitions("anything").is_empty());
        assert!(empty.search("anything", 10).is_empty());
    }

    #[test]
    fn test_definitions_are_case_insensitive_and_ranked() {
        let index = sample_index();
        let hits = index.definitions("APP");
        let rendered: Vec<_> = hits
            .iter()
            .map(|hit| (hit.path, hit.symbol.name.as_str(), hit.symbol.kind))
            .collect();
        assert_eq!(
            rendered,
            [
                ("src/app.rs", "App", SymbolKind::Class),
                ("src/ui.rs", "app", SymbolKind::Constant),
            ]
        );
    }

    #[test]
    fn test_file_symbols_lookup() {
        let index = sample_index();
        assert_eq!(
            index.file_symbols("src/ui.rs").map(<[Symbol]>::len),
            Some(1)
        );
        assert!(index.file_symbols("src/missing.rs").is_none());
    }

    #[test]
    fn test_projection_preserves_symbols_with_identical_public_fields() {
        let symbol = test_symbol("overload", SymbolKind::Method, 7, 4);
        let index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/overloads.rs".to_owned(),
            symbols: vec![symbol.clone(), symbol],
        }]);

        let hits = index.definitions("overload");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].symbol, hits[1].symbol);
        assert!(!std::ptr::eq(hits[0].symbol, hits[1].symbol));
    }

    #[test]
    fn test_from_files_uses_last_duplicate_path_and_retains_empty_files() {
        let index = SymbolIndex::from_files(vec![
            FileSymbols {
                path: "src/duplicate.rs".to_owned(),
                symbols: vec![test_symbol("first", SymbolKind::Function, 1, 0)],
            },
            FileSymbols {
                path: "src/duplicate.rs".to_owned(),
                symbols: vec![test_symbol("last", SymbolKind::Function, 2, 0)],
            },
            FileSymbols {
                path: "src/empty.rs".to_owned(),
                symbols: Vec::new(),
            },
        ]);

        assert_eq!(index.scanned_file_count(), 3);
        assert_eq!(index.file_count(), 2);
        assert!(index.definitions("first").is_empty());
        assert_eq!(index.definitions("last")[0].path, "src/duplicate.rs");
        assert_eq!(index.file_symbols("src/empty.rs"), Some([].as_slice()));
    }

    #[test]
    #[should_panic(expected = "symbol line exceeds hearth-graph u32 range")]
    fn test_from_files_rejects_lines_that_hearth_cannot_represent() {
        SymbolIndex::from_files(vec![FileSymbols {
            path: "src/overflow.rs".to_owned(),
            symbols: vec![test_symbol(
                "overflow",
                SymbolKind::Function,
                usize::try_from(u64::from(u32::MAX) + 1).expect("64-bit test host"),
                0,
            )],
        }]);
    }

    #[test]
    #[should_panic(expected = "symbol depth exceeds hearth-graph u16 range")]
    fn test_from_files_rejects_depths_that_hearth_cannot_represent() {
        let mut symbol = test_symbol("overflow", SymbolKind::Function, 1, 0);
        symbol.depth = usize::from(u16::MAX) + 1;
        SymbolIndex::from_files(vec![FileSymbols {
            path: "src/overflow.rs".to_owned(),
            symbols: vec![symbol],
        }]);
    }

    #[test]
    fn test_search_contracts_and_fuzzy_tiers() {
        let index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/search.rs".to_owned(),
            symbols: vec![
                test_symbol("parse", SymbolKind::Function, 1, 0),
                test_symbol("parse_line", SymbolKind::Function, 2, 0),
                test_symbol("do_parse", SymbolKind::Function, 3, 0),
                test_symbol("reparsed", SymbolKind::Function, 4, 0),
                test_symbol("please_advance_rest_of_set", SymbolKind::Function, 5, 0),
            ],
        }]);

        let names: Vec<_> = index
            .search("parse", usize::MAX)
            .iter()
            .map(|hit| hit.symbol.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "parse",
                "parse_line",
                "do_parse",
                "reparsed",
                "please_advance_rest_of_set",
            ]
        );
        assert_eq!(index.search("parse", 2).len(), 2);
        assert!(index.search("", 10).is_empty());
        assert!(index.search("parse", 0).is_empty());
        assert!(index.search("xyz", 10).is_empty());
    }

    #[test]
    fn test_search_subsequence_and_case_insensitivity() {
        let index = sample_index();
        assert_eq!(index.search("rndap", 10)[0].symbol.name, "render_app");
        assert_eq!(index.search("APP", 10), index.search("app", 10));
    }

    #[test]
    fn test_candidates_equal_on_visible_keys_have_deterministic_order() {
        const TOTAL: usize = 300;
        const LIMIT: usize = 200;
        let symbols = (0..TOTAL)
            .map(|index| test_symbol(&format!("tie_{index:04}"), SymbolKind::Function, 1, 0))
            .collect();
        let index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/generated.rs".to_owned(),
            symbols,
        }]);

        let names: Vec<_> = index
            .search("tie", LIMIT)
            .iter()
            .map(|hit| hit.symbol.name.as_str())
            .collect();
        let expected: Vec<_> = (0..LIMIT).map(|index| format!("tie_{index:04}")).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn test_search_is_case_insensitive_for_unicode_names() {
        let names = ["MixedCase", "UPPERCASE", "名前", "İ", "ß", "Éclair"];
        let symbols = names
            .iter()
            .enumerate()
            .map(|(index, name)| test_symbol(name, SymbolKind::Function, index + 1, 0))
            .collect();
        let index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/unicode.rs".to_owned(),
            symbols,
        }]);

        for query in ["MiXeDcAsE", "uPpErCaSe", "名前", "İ", "ß", "ÉcLaIr"] {
            let result = index.search(query, names.len());
            assert_eq!(result, index.search(&query.to_lowercase(), names.len()));
            assert!(!result.is_empty(), "{query:?}");
        }
    }

    #[test]
    fn test_symbol_index_is_send_and_sync() {
        fn assert_send_and_sync<T: Send + Sync>() {}
        assert_send_and_sync::<SymbolIndex>();
    }

    #[test]
    fn test_symbol_ref_search_label() {
        let cases = [
            (
                test_symbol("alpha", SymbolKind::Function, 3, 0),
                "src/a.rs",
                "ƒ alpha  src/a.rs:3",
            ),
            (
                test_symbol("名前", SymbolKind::Method, 27, 4),
                "src/parser/names.rs",
                "m 名前  src/parser/names.rs:27",
            ),
        ];
        for (symbol, path, expected) in &cases {
            assert_eq!(SymbolRef { path, symbol }.search_label(), *expected);
        }
    }

    #[test]
    fn test_browse_symbol_search_results_match_symbol_ref_search_label() {
        let index = std::sync::Arc::new(sample_index());
        let mut state = crate::app::browse::BrowseState::new(
            std::path::PathBuf::from("/repo"),
            crate::app::AppState::FileList,
        );
        state.index = crate::app::browse::IndexState::Ready(std::sync::Arc::clone(&index));

        let expected = index.search("app", usize::MAX);
        let actual = state.symbol_search_results("app");
        assert_eq!(actual.len(), expected.len());
        for ((path, line, label), hit) in actual.iter().zip(expected) {
            assert_eq!(label, &hit.search_label());
            assert_eq!(path, hit.path);
            assert_eq!(*line, hit.symbol.line);
        }
    }

    #[test]
    fn test_cancelled_build_stops_scanning_early() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let paths: Vec<_> = (0..1_000)
            .map(|index| {
                let path = format!("src/file_{index:04}.rs");
                std::fs::write(dir.path().join(&path), format!("pub fn f{index}() {{}}\n"))
                    .unwrap();
                path
            })
            .collect();

        let control = completed_index(SymbolIndex::build_cancellable(
            dir.path(),
            &paths,
            &CancellationToken::new(),
        ));
        assert_eq!(control.scanned_file_count(), 1_000);

        let cancel = PollCountCancel {
            limit: 50,
            polls: AtomicUsize::new(0),
        };
        match SymbolIndex::build_cancellable(dir.path(), &paths, &cancel) {
            IndexBuild::Cancelled { scanned_files } => {
                assert!(scanned_files <= 64, "{scanned_files}");
                assert!(scanned_files < 1_000);
            }
            other => panic!("cancelled build did not cancel: {other:?}"),
        }
    }

    #[test]
    fn test_cancelled_build_stops_metadata_prefilter() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..5_000)
            .map(|index| format!("notes/file_{index:04}.txt"))
            .collect();
        let cancel = PollCountCancel {
            limit: 1,
            polls: AtomicUsize::new(0),
        };

        match SymbolIndex::build_cancellable(dir.path(), &paths, &cancel) {
            IndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
            other => panic!("metadata prefilter did not cancel: {other:?}"),
        }
    }

    #[test]
    fn test_precancelled_build_scans_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.rs"), "pub fn present() {}\n").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        match SymbolIndex::build_cancellable(dir.path(), &["file.rs".to_owned()], &cancel) {
            IndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
            other => panic!("precancelled build did not cancel: {other:?}"),
        }
    }

    #[test]
    fn test_missing_or_non_directory_root_fails() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        match SymbolIndex::build_cancellable(&missing, &[], &CancellationToken::new()) {
            IndexBuild::Failed { message } => {
                assert!(
                    message.contains(&missing.display().to_string()),
                    "{message}"
                );
            }
            other => panic!("missing root did not fail: {other:?}"),
        }

        let file = dir.path().join("root.rs");
        std::fs::write(&file, "pub fn root() {}\n").unwrap();
        match SymbolIndex::build_cancellable(
            &file,
            &["src/lib.rs".to_owned()],
            &CancellationToken::new(),
        ) {
            IndexBuild::Failed { message } => {
                assert!(message.contains("is not a directory"), "{message}");
            }
            other => panic!("file root did not fail: {other:?}"),
        }
    }

    #[test]
    fn test_completed_build_retains_successfully_loaded_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("symbols.rs"), "pub fn present() {}\n").unwrap();
        std::fs::write(dir.path().join("comments.rs"), "// no symbols here\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "unsupported\n").unwrap();
        let paths = vec![
            "symbols.rs".to_owned(),
            "comments.rs".to_owned(),
            "notes.txt".to_owned(),
            "missing.rs".to_owned(),
        ];

        let index = completed_index(SymbolIndex::build_cancellable(
            dir.path(),
            &paths,
            &CancellationToken::new(),
        ));
        assert_eq!(index.file_count(), 2);
        assert_eq!(index.scanned_file_count(), 2);
        assert_eq!(index.file_symbols("comments.rs"), Some([].as_slice()));
    }

    #[test]
    fn test_build_indexes_supported_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("src/b.ts"), "export function beta() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not code\n").unwrap();

        let index = completed_index(SymbolIndex::build_cancellable(
            dir.path(),
            &[
                "src/a.rs".to_owned(),
                "src/b.ts".to_owned(),
                "notes.txt".to_owned(),
                "src/missing.rs".to_owned(),
            ],
            &CancellationToken::new(),
        ));
        assert_eq!(index.file_count(), 2);
        assert_eq!(index.symbol_count(), 2);
        assert_eq!(index.definitions("alpha").len(), 1);
        assert_eq!(index.definitions("beta")[0].path, "src/b.ts");
    }

    #[test]
    fn test_empty_build_and_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let empty = completed_index(SymbolIndex::build_cancellable(
            dir.path(),
            &[],
            &CancellationToken::new(),
        ));
        assert!(empty.is_empty());

        let huge = format!(
            "pub fn big() {{}}\n{}",
            "// filler filler filler filler\n".repeat(80_000)
        );
        assert!(huge.len() as u64 > MAX_INDEXED_FILE_BYTES);
        std::fs::write(dir.path().join("big.rs"), huge).unwrap();
        std::fs::write(dir.path().join("small.rs"), "pub fn small() {}\n").unwrap();
        let index = completed_index(SymbolIndex::build_cancellable(
            dir.path(),
            &["big.rs".to_owned(), "small.rs".to_owned()],
            &CancellationToken::new(),
        ));
        assert_eq!(index.definitions("small").len(), 1);
        assert!(index.file_symbols("big.rs").is_none());
    }
}
