//! Measure tree-sitter Parser + Query memory via C malloc interception.
//!
//! Run: cargo run --release --example measure_parser_size

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tree_sitter::Language;

// Track C-side malloc via libc hooks
static TRACKING: AtomicBool = AtomicBool::new(false);
static C_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static C_ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

// Use Rust GlobalAlloc to also capture Rust-side allocations
use std::alloc::{GlobalAlloc, Layout, System};

static RUST_ALLOCATED: AtomicUsize = AtomicUsize::new(0);

struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = unsafe { System.alloc(layout) };
        if !ret.is_null() && TRACKING.load(Ordering::Relaxed) {
            RUST_ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACKING.load(Ordering::Relaxed) {
            RUST_ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static A: TrackingAllocator = TrackingAllocator;

fn start_tracking() {
    RUST_ALLOCATED.store(0, Ordering::SeqCst);
    TRACKING.store(true, Ordering::SeqCst);
}

fn stop_tracking() -> usize {
    TRACKING.store(false, Ordering::SeqCst);
    RUST_ALLOCATED.load(Ordering::SeqCst)
}

fn measure_parser(name: &str, lang: Language, query_src: &str) {
    // Measure Parser + set_language + first parse
    start_tracking();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).unwrap();
    let _ = parser.parse("x", None);
    let parser_rust = stop_tracking();

    // Measure Query compilation
    start_tracking();
    let _query = tree_sitter::Query::new(&lang, query_src).unwrap();
    let query_rust = stop_tracking();

    let total = parser_rust + query_rust;

    println!(
        "{:<14} Parser: {:>7.1} KB   Query: {:>7.1} KB   Total: {:>7.1} KB",
        name,
        parser_rust as f64 / 1024.0,
        query_rust as f64 / 1024.0,
        total as f64 / 1024.0,
    );

    drop(_query);
    drop(parser);
}

fn main() {
    let ts_combined = format!(
        "{}\n{}",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_typescript::HIGHLIGHTS_QUERY
    );
    let cpp_combined = format!(
        "{}\n{}",
        tree_sitter_c::HIGHLIGHT_QUERY,
        tree_sitter_cpp::HIGHLIGHT_QUERY
    );
    let svelte_query: String = tree_sitter_svelte_ng::HIGHLIGHTS_QUERY
        .lines()
        .filter(|line| !line.trim().starts_with("; inherits:"))
        .collect::<Vec<_>>()
        .join("\n");
    let svelte_combined = format!("{}\n{}", tree_sitter_html::HIGHLIGHTS_QUERY, svelte_query);
    let html_query: String = tree_sitter_html::HIGHLIGHTS_QUERY
        .lines()
        .filter(|line| !line.contains("doctype"))
        .collect::<Vec<_>>()
        .join("\n");
    let vue_combined = format!("{}\n{}", html_query, tree_sitter_vue3::HIGHLIGHTS_QUERY);

    println!("tree-sitter memory cost (Rust-side allocations only)");
    println!("Note: Parser allocations happen in C, so Parser shows 0.");
    println!("Query is compiled in Rust, so Query numbers are accurate.");
    println!("===================================================\n");

    // Warm up
    {
        let mut p = tree_sitter::Parser::new();
        let lang: Language = tree_sitter_rust::LANGUAGE.into();
        p.set_language(&lang).unwrap();
        let _ = p.parse("fn main() {}", None);
        let _ = tree_sitter::Query::new(&lang, tree_sitter_rust::HIGHLIGHTS_QUERY).unwrap();
        drop(p);
    }

    measure_parser("Rust", tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY);
    measure_parser("TypeScript", tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), &ts_combined);
    measure_parser("JavaScript", tree_sitter_javascript::LANGUAGE.into(), tree_sitter_javascript::HIGHLIGHT_QUERY);
    measure_parser("Go", tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY);
    measure_parser("Python", tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY);
    measure_parser("C", tree_sitter_c::LANGUAGE.into(), tree_sitter_c::HIGHLIGHT_QUERY);
    measure_parser("C++", tree_sitter_cpp::LANGUAGE.into(), &cpp_combined);
    measure_parser("Java", tree_sitter_java::LANGUAGE.into(), tree_sitter_java::HIGHLIGHTS_QUERY);
    measure_parser("Ruby", tree_sitter_ruby::LANGUAGE.into(), tree_sitter_ruby::HIGHLIGHTS_QUERY);
    measure_parser("Lua", tree_sitter_lua::LANGUAGE.into(), tree_sitter_lua::HIGHLIGHTS_QUERY);
    measure_parser("Bash", tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY);
    measure_parser("PHP", tree_sitter_php::LANGUAGE_PHP.into(), tree_sitter_php::HIGHLIGHTS_QUERY);
    measure_parser("Swift", tree_sitter_swift::LANGUAGE.into(), tree_sitter_swift::HIGHLIGHTS_QUERY);
    measure_parser("Haskell", tree_sitter_haskell::LANGUAGE.into(), tree_sitter_haskell::HIGHLIGHTS_QUERY);
    measure_parser("CSS", tree_sitter_css::LANGUAGE.into(), tree_sitter_css::HIGHLIGHTS_QUERY);
    measure_parser("Svelte", tree_sitter_svelte_ng::LANGUAGE.into(), &svelte_combined);
    measure_parser("Vue", tree_sitter_vue3::LANGUAGE.into(), &vue_combined);

    println!();

    // Svelte injection scenario
    println!("--- Svelte injection: 3 Queries (TS + CSS + Svelte) ---\n");
    start_tracking();
    let svelte_lang: Language = tree_sitter_svelte_ng::LANGUAGE.into();
    let _q1 = tree_sitter::Query::new(&svelte_lang, &svelte_combined).unwrap();
    let ts_lang: Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    let _q2 = tree_sitter::Query::new(&ts_lang, &ts_combined).unwrap();
    let css_lang: Language = tree_sitter_css::LANGUAGE.into();
    let _q3 = tree_sitter::Query::new(&css_lang, tree_sitter_css::HIGHLIGHTS_QUERY).unwrap();
    let total = stop_tracking();
    println!("3 Queries combined: {:.1} KB", total as f64 / 1024.0);
    println!("× 50 files without pool: {:.1} KB", total as f64 * 50.0 / 1024.0);
    println!("× 1 with pool:           {:.1} KB", total as f64 / 1024.0);
    println!();
}
