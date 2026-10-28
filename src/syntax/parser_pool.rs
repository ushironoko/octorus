//! Parser pool for tree-sitter parsers.
//!
//! Parsers are relatively heavy objects (~200KB each), so we maintain a pool
//! to reuse them across multiple files rather than creating new ones for each file.
//!
//! Highlight queries are compiled once per [`SupportedLanguage`] for the whole
//! process. Jobs each own a pool, and sharing the `Query` avoids compiling it
//! again on every job. A `Parser` stays in the pool that created it.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::OnceLock;
use tree_sitter::{Parser, Query};

use crate::language::SupportedLanguage;

/// Pool of tree-sitter parsers, one per language, for a single highlight job.
///
/// Parsers are created on first use and reused for later files in that job.
/// Highlight queries live in a process-wide slot. See [`Self::get_or_create_query`].
pub struct ParserPool {
    parsers: HashMap<SupportedLanguage, Parser>,
    /// Hearth-owned parser and tags-query cache for symbol extraction.
    symbol_parsers: hearth_graph::ParserPool<'static>,
}

impl Default for ParserPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ParserPool {
    /// Create a new empty parser pool.
    pub fn new() -> Self {
        Self {
            parsers: HashMap::new(),
            symbol_parsers: hearth_graph::ParserPool::new(
                crate::symbols::symbol_language_registry(),
            ),
        }
    }

    /// Return the Hearth parser/query cache used by the symbol facade.
    pub(crate) fn symbol_parser_pool(&mut self) -> &mut hearth_graph::ParserPool<'static> {
        &mut self.symbol_parsers
    }

    /// Get or create the compiled tags query for a supported symbol language.
    ///
    /// This preserves the public octorus API while delegating query ownership
    /// and caching to Hearth. Languages without symbol support return `None`.
    pub fn get_or_create_tags_query(&mut self, lang: SupportedLanguage) -> Option<&Query> {
        let id = crate::symbols::symbol_language_id(lang.default_extension())?;
        self.symbol_parsers.tags_query(id)
    }

    /// Get or create a compiled highlight query for the given language.
    ///
    /// The compiled `Query` is shared by every pool in the process. The pool
    /// does not store it. Callers keep using this method so a highlight job
    /// does not need a different API.
    ///
    /// A failed compile returns `None` and is not stored, so the next call
    /// tries again.
    pub fn get_or_create_query(&self, lang: SupportedLanguage) -> Option<&'static Query> {
        shared_highlight_query(lang)
    }

    /// Get or create a parser for the given file extension.
    ///
    /// Returns `None` if the extension is not supported by tree-sitter.
    pub fn get_or_create(&mut self, ext: &str) -> Option<&mut Parser> {
        let lang = SupportedLanguage::from_extension(ext)?;

        if let Entry::Vacant(e) = self.parsers.entry(lang) {
            let ts_language = lang.ts_language();
            let mut parser = Parser::new();
            if parser.set_language(&ts_language).is_err() {
                return None;
            }
            e.insert(parser);
        }

        self.parsers.get_mut(&lang)
    }

    /// Check if tree-sitter supports the given file extension.
    pub fn supports_extension(ext: &str) -> bool {
        SupportedLanguage::is_supported(ext)
    }
}

const HIGHLIGHT_QUERY_SLOT_COUNT: usize = 24;

fn shared_highlight_query(lang: SupportedLanguage) -> Option<&'static Query> {
    let slot = highlight_query_slot(lang);
    if let Some(query) = slot.get() {
        return Some(query);
    }
    let query = Query::new(&lang.ts_language(), lang.highlights_query()).ok()?;
    let _ = slot.set(query);
    slot.get()
}

fn highlight_query_slot(lang: SupportedLanguage) -> &'static OnceLock<Query> {
    static SLOTS: [OnceLock<Query>; HIGHLIGHT_QUERY_SLOT_COUNT] =
        [const { OnceLock::new() }; HIGHLIGHT_QUERY_SLOT_COUNT];
    &SLOTS[highlight_query_slot_index(lang)]
}

fn highlight_query_slot_index(lang: SupportedLanguage) -> usize {
    use SupportedLanguage::*;
    match lang {
        Rust => 0,
        TypeScript => 1,
        TypeScriptReact => 2,
        JavaScript => 3,
        JavaScriptReact => 4,
        Go => 5,
        Python => 6,
        Ruby => 7,
        Zig => 8,
        C => 9,
        Cpp => 10,
        Java => 11,
        CSharp => 12,
        Lua => 13,
        Bash => 14,
        Php => 15,
        Swift => 16,
        Haskell => 17,
        MoonBit => 18,
        Svelte => 19,
        Vue => 20,
        Css => 21,
        Markdown => 22,
        MarkdownInline => 23,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_pool_rust() {
        let mut pool = ParserPool::new();
        let parser = pool.get_or_create("rs");
        assert!(parser.is_some(), "Should create Rust parser");

        // Second call should return the same parser
        let parser2 = pool.get_or_create("rs");
        assert!(parser2.is_some(), "Should reuse Rust parser");
    }

    #[test]
    fn test_parser_pool_typescript() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("ts").is_some());
        assert!(pool.get_or_create("tsx").is_some());
    }

    #[test]
    fn test_parser_pool_javascript() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("js").is_some());
        assert!(pool.get_or_create("jsx").is_some());
    }

    #[test]
    fn test_parser_pool_go() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("go").is_some());
    }

    #[test]
    fn test_parser_pool_python() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("py").is_some());
    }

    #[test]
    fn test_parser_pool_ruby() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("rb").is_some());
        assert!(pool.get_or_create("rake").is_some());
        assert!(pool.get_or_create("gemspec").is_some());
    }

    #[test]
    fn test_parser_pool_zig() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("zig").is_some());
    }

    #[test]
    fn test_parser_pool_c() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("c").is_some());
        // .h files are treated as C
        assert!(pool.get_or_create("h").is_some());
    }

    #[test]
    fn test_parser_pool_cpp() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("cpp").is_some());
        assert!(pool.get_or_create("cc").is_some());
        assert!(pool.get_or_create("cxx").is_some());
        assert!(pool.get_or_create("hpp").is_some());
        assert!(pool.get_or_create("hxx").is_some());
    }

    #[test]
    fn test_parser_pool_java() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("java").is_some());
    }

    #[test]
    fn test_parser_pool_csharp() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("cs").is_some());
    }

    #[test]
    fn test_parser_pool_lua() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("lua").is_some());
    }

    #[test]
    fn test_parser_pool_bash() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("sh").is_some());
        assert!(pool.get_or_create("bash").is_some());
        assert!(pool.get_or_create("zsh").is_some());
    }

    #[test]
    fn test_parser_pool_php() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("php").is_some());
    }

    #[test]
    fn test_parser_pool_swift() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("swift").is_some());
    }

    #[test]
    fn test_parser_pool_haskell() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("hs").is_some());
        assert!(pool.get_or_create("lhs").is_some());
    }

    // Svelte falls back to syntect (tree-sitter-svelte-ng requires injection)

    #[test]
    fn test_parser_pool_moonbit() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("mbt").is_some());
    }

    #[test]
    fn test_parser_pool_svelte() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("svelte").is_some());
    }

    #[test]
    fn test_parser_pool_vue() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("vue").is_some());
    }

    #[test]
    fn test_parser_pool_markdown() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("md").is_some());
        assert!(pool.get_or_create("markdown").is_some());
    }

    #[test]
    fn test_parser_pool_markdown_inline() {
        let mut pool = ParserPool::new();
        assert!(pool.get_or_create("md_inline").is_some());
    }

    #[test]
    fn test_parser_pool_unsupported() {
        let mut pool = ParserPool::new();
        // Vue is now supported in Phase 3c
        assert!(pool.get_or_create("yaml").is_none());
        assert!(pool.get_or_create("toml").is_none());
    }

    #[test]
    fn test_supports_extension() {
        // Original languages
        assert!(ParserPool::supports_extension("rs"));
        assert!(ParserPool::supports_extension("ts"));
        assert!(ParserPool::supports_extension("tsx"));
        assert!(ParserPool::supports_extension("js"));
        assert!(ParserPool::supports_extension("jsx"));
        assert!(ParserPool::supports_extension("go"));
        assert!(ParserPool::supports_extension("py"));

        // Phase 1 languages
        assert!(ParserPool::supports_extension("lua"));
        assert!(ParserPool::supports_extension("sh"));
        assert!(ParserPool::supports_extension("php"));
        assert!(ParserPool::supports_extension("swift"));
        assert!(ParserPool::supports_extension("hs"));

        // Phase 3: Svelte is now supported
        assert!(ParserPool::supports_extension("svelte"));
        // Phase 3c: Vue is now supported
        assert!(ParserPool::supports_extension("vue"));

        // Phase 4: Markdown
        assert!(ParserPool::supports_extension("md"));

        assert!(!ParserPool::supports_extension("yaml"));
    }

    #[test]
    fn test_parser_can_parse() {
        let mut pool = ParserPool::new();
        let parser = pool.get_or_create("rs").unwrap();

        let code = "fn main() { println!(\"Hello\"); }";
        let tree = parser.parse(code, None);
        assert!(tree.is_some(), "Should parse Rust code");
    }

    #[test]
    fn test_query_cache_creates_query() {
        let pool = ParserPool::new();
        let query = pool.get_or_create_query(SupportedLanguage::Rust);
        assert!(query.is_some(), "Should create Rust highlight query");
    }

    #[test]
    fn test_query_cache_reuses_query() {
        let pool = ParserPool::new();

        let query1 = pool
            .get_or_create_query(SupportedLanguage::TypeScript)
            .expect("TypeScript highlight query should compile");
        let query2 = pool
            .get_or_create_query(SupportedLanguage::TypeScript)
            .expect("TypeScript highlight query should compile");
        assert!(std::ptr::eq(query1, query2));
    }

    #[test]
    fn second_pool_reuses_compiled_highlight_query() {
        let first = ParserPool::new();
        let second = ParserPool::new();
        let languages = [
            SupportedLanguage::Rust,
            SupportedLanguage::Haskell,
            SupportedLanguage::Markdown,
            SupportedLanguage::Vue,
            SupportedLanguage::TypeScript,
            SupportedLanguage::TypeScriptReact,
            SupportedLanguage::JavaScript,
            SupportedLanguage::JavaScriptReact,
        ];

        for lang in languages {
            let left = first.get_or_create_query(lang).unwrap_or_else(|| {
                panic!("{lang:?} highlight query should compile");
            });
            let right = second.get_or_create_query(lang).unwrap_or_else(|| {
                panic!("{lang:?} highlight query should compile");
            });
            assert!(
                std::ptr::eq(left, right),
                "{lang:?} compiled a second highlight query"
            );
        }

        let rust = first
            .get_or_create_query(SupportedLanguage::Rust)
            .expect("Rust highlight query should compile");
        let haskell = first
            .get_or_create_query(SupportedLanguage::Haskell)
            .expect("Haskell highlight query should compile");
        let typescript = first
            .get_or_create_query(SupportedLanguage::TypeScript)
            .expect("TypeScript highlight query should compile");
        let tsx = first
            .get_or_create_query(SupportedLanguage::TypeScriptReact)
            .expect("TSX highlight query should compile");
        assert!(!std::ptr::eq(rust, haskell));
        assert!(!std::ptr::eq(typescript, tsx));
    }

    #[test]
    fn highlight_query_slots_are_unique_per_language() {
        let mut seen = [false; HIGHLIGHT_QUERY_SLOT_COUNT];
        let mut count = 0;
        for lang in SupportedLanguage::all() {
            let index = highlight_query_slot_index(lang);
            assert!(
                index < seen.len(),
                "{lang:?} highlight query slot {index} is out of range"
            );
            assert!(!seen[index], "{lang:?} shares highlight query slot {index}");
            seen[index] = true;
            count += 1;
        }
        assert_eq!(count, HIGHLIGHT_QUERY_SLOT_COUNT);
    }

    #[test]
    fn test_query_cache_multiple_languages() {
        let pool = ParserPool::new();
        let rust = pool.get_or_create_query(SupportedLanguage::Rust);
        let typescript = pool.get_or_create_query(SupportedLanguage::TypeScript);
        let javascript = pool.get_or_create_query(SupportedLanguage::JavaScript);
        let css = pool.get_or_create_query(SupportedLanguage::Css);
        assert!(rust.is_some());
        assert!(typescript.is_some());
        assert!(javascript.is_some());
        assert!(css.is_some());
        assert!(!std::ptr::eq(rust.unwrap(), css.unwrap()));
    }

    #[test]
    fn test_query_has_capture_names() {
        let pool = ParserPool::new();
        let query = pool.get_or_create_query(SupportedLanguage::Rust).unwrap();

        // Query should have capture names for highlighting
        let capture_names = query.capture_names();
        assert!(!capture_names.is_empty(), "Query should have capture names");
    }

    #[test]
    fn test_query_cache_markdown() {
        let pool = ParserPool::new();
        let query = pool.get_or_create_query(SupportedLanguage::Markdown);
        assert!(
            query.is_some(),
            "Should compile Markdown block highlight query"
        );
    }

    #[test]
    fn test_query_cache_markdown_inline() {
        let pool = ParserPool::new();
        let query = pool.get_or_create_query(SupportedLanguage::MarkdownInline);
        assert!(
            query.is_some(),
            "Should compile MarkdownInline highlight query"
        );
    }

    #[test]
    fn test_markdown_parser_can_parse() {
        let mut pool = ParserPool::new();
        let parser = pool.get_or_create("md").unwrap();

        let code = "# Hello\n\nSome **bold** text.\n";
        let tree = parser.parse(code, None);
        assert!(tree.is_some(), "Should parse Markdown code");
    }

    #[test]
    fn test_supports_extension_markdown() {
        assert!(ParserPool::supports_extension("md"));
        assert!(ParserPool::supports_extension("markdown"));
        assert!(ParserPool::supports_extension("md_inline"));
    }
}
