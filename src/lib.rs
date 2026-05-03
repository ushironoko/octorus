//! octorus - A TUI tool for GitHub PR review
//!
//! This library exposes modules for benchmarking purposes.

pub mod ai;
pub mod app;
pub mod cache;
pub mod config;
pub mod diff;
pub mod diff_store;
pub mod editor;
pub mod filter;
pub mod gitfilm;
pub mod github;
pub mod headless;
pub mod keybinding;
pub mod language;
pub mod loader;
pub mod symbol;
pub mod syntax;
pub mod ui;
pub mod url_parse;

// Re-export commonly used types for benchmarks
pub use app::{CachedDiffLine, DiffCache, InternedSpan};
pub use diff::{classify_line, get_line_info, LineType, PatchIndex, PatchLineInfo};
pub use syntax::ParserPool;
pub use ui::diff_view::{build_diff_cache, render_cached_lines};
