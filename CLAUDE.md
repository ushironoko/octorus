# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Plz read README.md.

Design notes for the Repository Browser live in `docs/`. Read
`docs/repo-browse-architecture.md` before changing the browser screen, and
`docs/symbol-index.md` when the work touches symbol extraction or adds a
language — it records the per-grammar quirks you will otherwise rediscover the
hard way. `docs/README.md` indexes both.

## Build & Test Commands

```bash
cargo build --release        # Build
cargo test                   # Run all tests
cargo clippy                 # Lint
cargo fmt --check            # Format check
cargo bench                  # Run all benchmarks (Criterion)
cargo bench --bench ui_rendering
cargo bench --bench diff_parsing
cargo bench --bench symbol_search
cargo bench --bench symbol_index
cccc --table --top-cognitive 20 src # Top 20 functions by cognitive complexity
```

`.github/workflows/benchmark.yml` runs only `ui_rendering` and `diff_parsing`, and only on
`workflow_dispatch` or the weekly cron — with `fail-on-alert: false`. A benchmark regression
never blocks a PR, so run the relevant bench locally when you touch a hot path.

## Principles

This project is Building/Development for 5 Principles.

## 1. Always Integration/Snapshot/Scenario based TDD

- Write tests **before** implementation. No feature is complete without passing tests.
- Use `insta` for snapshot testing. always use inline-snapshot.
- Use `assert_cmd` + `predicates` for integration tests that exercise the binary end-to-end.
- Prefer scenario-based tests that simulate real user workflows (e.g., "load PR → navigate to file → jump to comment") over isolated unit tests of internal helpers.
- Snapshot tests are the primary regression safety net. When changing rendering or API response parsing, always update or add snapshots.
- Use `serial_test` (`#[serial]`) when tests share mutable global state or filesystem resources.

## 2. Always without exception needs recover Clippy,Test,Check Errors

- Every change must pass all three gates before being considered complete:
  1. `cargo clippy -- -D warnings` — zero warnings, treated as errors
  2. `cargo test` — all tests green
  3. `cargo check` — no compilation errors
- If a change introduces a Clippy warning, fix it immediately in the same change — never suppress with `#[allow(...)]` unless there is a documented, unavoidable reason.
- CI failures are blocking. Do not proceed with further work until all three gates pass locally.
- When refactoring, run all three gates after each logical step, not just at the end.

## 3. Focus on performance for Rust code

- This app handles PRs with 6,000+ files and 300,000+ lines. Performance is a user-facing feature.
- Use string interning (`lasso::Rodeo`) for repeated diff line strings to reduce allocations.
- Use `smallvec` for stack-allocated small collections where heap allocation is avoidable.
- Use compile-time perfect hash maps (`phf`) for static lookup tables (e.g., language detection, capture-to-scope mapping).
- Pre-compute and cache syntax-highlighted diffs in `DiffCache` — never re-highlight on every render.
- Benchmark before and after performance-sensitive changes using Criterion (`benches/`). The CI alerts on 150%+ regression.
- Prefer `&str` borrows over `String` clones. Avoid unnecessary `.clone()` and `.to_string()`.
- Use `tokio` async tasks with cancellation tokens for background data loading — never block the UI thread.

## 4. Use a state machine for consistency instead of individual conditional branches.

- All screen/mode transitions go through `AppState` enum (19 states). Never use ad-hoc boolean flags to track "which screen am I on."
- Data loading lifecycle is modeled as `DataState` enum (`Loading` → `Loaded` → `Error`). Never use `Option<Data>` + `is_loading: bool` separately.
- AI Rally transitions flow through `RallyState` enum with `RallyEvent`-driven transitions. Each state has explicit allowed transitions — invalid transitions are compile-time or runtime errors.
- Input modes are modeled as `InputMode` enum variants with associated data (context, original code, etc.).
- When adding a new feature that introduces a new mode or screen, add a variant to the appropriate state enum and handle it exhaustively in match arms. The compiler enforces completeness.
- Pause/resume is `PauseState` enum (`Running` → `PauseRequested` → `Paused` → `Running`), not a boolean toggle.

## 5. Consider all edge cases as use cases

- Empty states are first-class: no PRs, no files, no comments, no diff content — all must render gracefully.
- Unicode and CJK text: use `unicode-width` for display width calculation. Never assume 1 byte = 1 character = 1 column.
- Large inputs: test with 1,000+ line diffs and 5,000+ line patches. Benchmark suites cover 100/500/1000/5000 line scenarios.
- Network failures: `DataState::Error` must display actionable messages. Retry mechanism uses atomic flags.
- Concurrent state: file watcher events can arrive during any state. AI Rally commands can arrive while rendering. Handle via channels with non-blocking receives.
- Platform differences: binary runs on Linux, macOS, Windows. Terminal behavior varies — use crossterm abstractions, never raw ANSI escapes.
