//! Diff parsing benchmarks for octorus.
//!
//! These benchmarks measure the performance of:
//! - Line classification (PatchClassifier)
//! - Line info extraction (get_line_info)

mod common;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use common::{generate_diff_patch, generate_simple_patch};
use octorus::diff::DiffSide;
use octorus::{get_line_info, linear_marker_rows, CommentAnchor, PatchClassifier, PatchLookup};

fn bench_classify_line(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_parsing/classify_line");

    // Test different line types
    let test_lines = [
        ("header", "@@ -1,10 +1,12 @@"),
        ("meta_diff", "diff --git a/file.rs b/file.rs"),
        ("meta_plus", "+++ b/file.rs"),
        ("meta_minus", "--- a/file.rs"),
        ("added", "+    let x = foo();"),
        ("removed", "-    let y = bar();"),
        ("context", "     fn main() {"),
        ("context_long", "     let very_long_variable_name = some_function_with_many_arguments(arg1, arg2, arg3, arg4, arg5);"),
    ];

    for (name, line) in test_lines {
        group.bench_with_input(BenchmarkId::from_parameter(name), line, |b, line| {
            b.iter(|| {
                black_box(PatchClassifier::new().classify(black_box(line)));
            });
        });
    }

    group.finish();
}

fn bench_classify_line_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_parsing/classify_line_batch");

    for line_count in [100, 500, 1000] {
        let patch = generate_diff_patch(line_count);
        let lines: Vec<&str> = patch.lines().collect();

        group.throughput(Throughput::Elements(line_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(line_count),
            &lines,
            |b, lines| {
                b.iter(|| {
                    let mut classifier = PatchClassifier::new();
                    for line in lines.iter() {
                        black_box(classifier.classify(black_box(line)));
                    }
                });
            },
        );
    }

    group.finish();
}

/// Benchmark get_line_info function.
///
/// Tests extracting line info at various positions in the patch.
fn bench_get_line_info(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_parsing/get_line_info");

    for line_count in [100, 500, 1000] {
        let patch = generate_diff_patch(line_count);

        // Test getting info at beginning, middle, and end
        for (position_name, position) in [
            ("start", 5_usize),
            ("middle", line_count / 2),
            ("end", line_count.saturating_sub(5)),
        ] {
            group.bench_with_input(
                BenchmarkId::new(format!("{}/{}", line_count, position_name), position),
                &(patch.clone(), position),
                |b, (patch, pos)| {
                    b.iter(|| black_box(get_line_info(black_box(patch), black_box(*pos))));
                },
            );
        }
    }

    group.finish();
}

/// Benchmark get_line_info with simple vs complex patches.
///
/// Compares performance on simple uniform patches vs realistic patches.
fn bench_get_line_info_complexity(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_parsing/get_line_info_complexity");

    let line_count = 1000_usize;
    let simple_patch = generate_simple_patch(line_count);
    let complex_patch = generate_diff_patch(line_count);
    let position = line_count / 2;

    group.bench_with_input(
        BenchmarkId::from_parameter("simple"),
        &simple_patch,
        |b, patch| {
            b.iter(|| black_box(get_line_info(black_box(patch), black_box(position))));
        },
    );

    group.bench_with_input(
        BenchmarkId::from_parameter("complex"),
        &complex_patch,
        |b, patch| {
            b.iter(|| black_box(get_line_info(black_box(patch), black_box(position))));
        },
    );

    group.finish();
}

/// Indexed line lookup. The index is built outside the timed loop.
fn bench_patch_lookup_line(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_parsing/patch_lookup_line");

    for line_count in [100, 500, 1000] {
        let patch = generate_diff_patch(line_count);
        let lookup = PatchLookup::build(&patch);
        for (position_name, position) in [
            ("start", 5_usize),
            ("middle", line_count / 2),
            ("end", line_count.saturating_sub(5)),
        ] {
            group.bench_with_input(
                BenchmarkId::new(format!("{line_count}/{position_name}"), position),
                &(patch.clone(), position),
                |b, (patch, pos)| {
                    b.iter(|| {
                        black_box(lookup.line_info(black_box(patch), black_box(*pos)));
                    });
                },
            );
        }
    }

    group.finish();
}

/// One index build, then every comment. Compared with a linear scan per comment.
fn bench_comment_positions(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_parsing/comment_positions");
    group.sample_size(10);

    for line_count in [1_000_usize, 100_000, 1_000_000] {
        let patch = generate_diff_patch(line_count);
        let lookup = PatchLookup::build(&patch);
        let anchors = comment_anchors(&lookup, &patch, 64);
        let patch_linear = patch.clone();
        let anchors_linear = anchors.clone();

        group.throughput(Throughput::Elements(anchors.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("indexed", line_count),
            &patch,
            |b, patch| {
                b.iter(|| {
                    let lookup = PatchLookup::build(black_box(patch));
                    black_box(
                        anchors
                            .iter()
                            .map(|anchor| lookup.marker_rows(*anchor))
                            .collect::<Vec<_>>(),
                    );
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("linear", line_count),
            &patch_linear,
            |b, patch| {
                b.iter(|| {
                    black_box(
                        anchors_linear
                            .iter()
                            .map(|anchor| linear_marker_rows(black_box(patch), *anchor))
                            .collect::<Vec<_>>(),
                    );
                });
            },
        );
        // The lookup above is rebuilt inside the loop. Keep this build live so
        // the anchor harvest cannot be optimized out of the setup.
        black_box(lookup.len());
    }

    group.finish();
}

fn comment_anchors(lookup: &PatchLookup, patch: &str, count: usize) -> Vec<CommentAnchor> {
    let mut anchors = Vec::with_capacity(count);
    let step = (lookup.len() / count.max(1)).max(1);
    for row in (0..lookup.len()).step_by(step) {
        let Some(info) = lookup.line_info(patch, row) else {
            continue;
        };
        let Some(line) = info.new_line_number.or(info.old_line_number) else {
            continue;
        };
        let side = if info.new_line_number.is_some() {
            DiffSide::Right
        } else {
            DiffSide::Left
        };
        let start_line = (line > 2 && anchors.len() % 2 == 0).then_some(line - 2);
        anchors.push(CommentAnchor {
            side,
            line,
            start_line,
            start_side: side,
        });
        if anchors.len() == count {
            break;
        }
    }
    anchors
}

criterion_group!(
    benches,
    bench_classify_line,
    bench_classify_line_batch,
    bench_get_line_info,
    bench_get_line_info_complexity,
    bench_patch_lookup_line,
    bench_comment_positions,
);
criterion_main!(benches);
