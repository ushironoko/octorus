//! Symbol search benchmarks for octorus.
//!
//! These benchmarks measure the performance of:
//! - Identifier extraction (extract_all_identifiers)
//! - Definition line matching (is_definition_line)
//! - Import line matching (is_import_line)
//! - Definition search within patches (find_definition_in_patches)
//! - Definition search in repository (find_definition_in_repo) — I/O bound

mod common;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use common::generate_diff_patch;
use octorus::github::ChangedFile;
use octorus::symbol::{
    extract_all_identifiers, find_definition_in_patches, find_definition_in_repo,
    is_definition_line, is_import_line,
};

// ---------------------------------------------------------------------------
// Pure function benchmarks
// ---------------------------------------------------------------------------

/// Benchmark extract_all_identifiers on various line complexities.
fn bench_extract_all_identifiers(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_search/extract_all_identifiers");

    let test_lines = [
        ("simple", "let x = 42;"),
        (
            "medium",
            "pub fn calculate(input: &str) -> Result<String> {",
        ),
        (
            "complex",
            "self.session_cache.get_pr_data(&PrCacheKey { repo: repo.clone(), pr_number })",
        ),
        (
            "long_chain",
            "let result = items.iter().filter(|x| x.is_valid()).map(|x| x.transform()).collect::<Vec<_>>();",
        ),
    ];

    for (name, line) in test_lines {
        group.bench_with_input(BenchmarkId::from_parameter(name), line, |b, line| {
            b.iter(|| black_box(extract_all_identifiers(black_box(line))));
        });
    }

    group.finish();
}

/// Benchmark is_definition_line for various languages.
fn bench_is_definition_line(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_search/is_definition_line");

    let test_cases = [
        ("rust_fn", "pub fn calculate(x: i32) -> i32 {", "calculate"),
        ("rust_struct", "pub struct Config {", "Config"),
        ("rust_impl", "impl<T> Vec<T> {", "Vec"),
        ("ts_function", "export function setup() {", "setup"),
        ("ts_class", "class Component {", "Component"),
        ("python_def", "def process(data):", "process"),
        ("go_func", "func main() {", "main"),
        ("no_match", "let x = calculate(5);", "calculate"),
    ];

    for (name, line, symbol) in test_cases {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(line, symbol),
            |b, (line, symbol)| {
                b.iter(|| black_box(is_definition_line(black_box(line), black_box(symbol))));
            },
        );
    }

    group.finish();
}

/// Benchmark is_import_line for various import styles.
fn bench_is_import_line(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_search/is_import_line");

    let test_cases = [
        (
            "rust_use",
            "use std::process::Command;",
            "Command",
        ),
        (
            "rust_grouped",
            "use std::io::{Read, Write, BufRead};",
            "Write",
        ),
        (
            "js_named",
            "import { useState, useEffect } from 'react';",
            "useEffect",
        ),
        (
            "js_default",
            "import React from 'react';",
            "React",
        ),
        (
            "python",
            "from os.path import join, exists",
            "join",
        ),
        ("no_match", "let cmd = Command::new(\"ls\");", "Command"),
    ];

    for (name, line, symbol) in test_cases {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(line, symbol),
            |b, (line, symbol)| {
                b.iter(|| black_box(is_import_line(black_box(line), black_box(symbol))));
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Patch search benchmarks
// ---------------------------------------------------------------------------

/// Generate ChangedFile fixtures from diff patches.
fn generate_changed_files(file_count: usize, lines_per_file: usize) -> Vec<ChangedFile> {
    (0..file_count)
        .map(|i| {
            let patch = generate_diff_patch(lines_per_file);
            ChangedFile {
                filename: format!("src/file_{}.rs", i),
                status: "modified".to_string(),
                additions: (lines_per_file / 3) as u32,
                deletions: (lines_per_file / 5) as u32,
                patch: Some(patch),
                viewed: false,
            }
        })
        .collect()
}

/// Benchmark find_definition_in_patches with varying file counts.
///
/// Tests the worst case: symbol is NOT found in any patch.
fn bench_find_definition_in_patches_miss(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_search/find_in_patches_miss");

    for file_count in [5, 20, 50] {
        let files = generate_changed_files(file_count, 200);
        let total_lines: usize = files
            .iter()
            .filter_map(|f| f.patch.as_ref())
            .map(|p| p.lines().count())
            .sum();

        group.throughput(Throughput::Elements(total_lines as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_files", file_count)),
            &files,
            |b, files| {
                b.iter(|| {
                    black_box(find_definition_in_patches(
                        black_box("nonexistent_symbol_xyz"),
                        black_box(files),
                        black_box(0),
                    ))
                });
            },
        );
    }

    group.finish();
}

/// Benchmark find_definition_in_patches with a hit in the last file.
///
/// Tests the scan cost before finding a match.
fn bench_find_definition_in_patches_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_search/find_in_patches_hit");

    for file_count in [5, 20, 50] {
        let mut files = generate_changed_files(file_count, 200);
        // Inject a definition in the LAST file
        let last = files.last_mut().unwrap();
        let mut patch = last.patch.take().unwrap();
        patch.push_str("\n+pub fn target_symbol() {\n");
        last.patch = Some(patch);

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_files", file_count)),
            &files,
            |b, files| {
                b.iter(|| {
                    black_box(find_definition_in_patches(
                        black_box("target_symbol"),
                        black_box(files),
                        black_box(0),
                    ))
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Repository search benchmark (I/O bound)
// ---------------------------------------------------------------------------

/// Benchmark find_definition_in_repo on the octorus repository itself.
///
/// This measures real-world grep performance. Run with:
///   cargo bench -- symbol_search/find_in_repo
fn bench_find_definition_in_repo(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_search/find_in_repo");
    // Reduce sample size for I/O-bound benchmarks
    group.sample_size(10);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    let test_symbols = [
        ("common_fn", "classify_line"),
        ("struct", "DiffCache"),
        ("rare_fn", "sanitize_repo_name"),
        ("not_found", "nonexistent_symbol_xyz_123"),
    ];

    for (name, symbol) in test_symbols {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(symbol, repo_root),
            |b, (symbol, root)| {
                b.iter(|| {
                    rt.block_on(async {
                        black_box(find_definition_in_repo(black_box(symbol), black_box(root)).await)
                    })
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_extract_all_identifiers,
    bench_is_definition_line,
    bench_is_import_line,
    bench_find_definition_in_patches_miss,
    bench_find_definition_in_patches_hit,
    bench_find_definition_in_repo,
);
criterion_main!(benches);
