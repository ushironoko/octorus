//! Slab queries must render the same highlights as one whole-tree query.
//!
//! Capture order is part of the highlight: `apply_line_highlights` resolves
//! overlaps from that order, and the whole-tree cursor emits duplicate
//! captures. These tests compare the rendered spans, not the match set.

use std::sync::atomic::{AtomicUsize, Ordering};

use lasso::Rodeo;
use rand::Rng;
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha8Rng;

use super::{
    apply_line_highlights, collect_line_highlights_bounded,
    collect_line_highlights_with_injections, visible_tree_depth, ForcePlan, HighlightAbort,
    HighlightControl, Highlighter, LineHighlights, QueryPlan, DEEP_TREE_MIN_DEPTH,
};
use crate::syntax::ParserPool;
use crate::ui::diff_view::build_diff_cache;

fn force(plan: Option<QueryPlan>) -> ForcePlan {
    match plan {
        Some(QueryPlan::Whole) => ForcePlan::whole(),
        Some(QueryPlan::Slabs) => ForcePlan::slabs(),
        None => ForcePlan::set(None),
    }
}

fn render_source(filename: &str, source: &str, plan: Option<QueryPlan>) -> String {
    let _guard = force(plan);
    let mut pool = ParserPool::new();
    let highlighter = Highlighter::for_file(filename, "Dracula");
    let Some(parsed) = highlighter.parse_source(source, &mut pool) else {
        return String::new();
    };
    let style_cache = highlighter.style_cache().expect("cst highlighter");
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let highlights = collect_line_highlights_with_injections(
        source,
        &parsed.tree,
        parsed.lang,
        style_cache,
        &mut pool,
        ext,
    );
    render_lines(source, &highlights)
}

fn render_lines(source: &str, highlights: &LineHighlights) -> String {
    let mut interner = Rodeo::default();
    let mut out = String::new();
    for (i, line) in source.split_inclusive('\n').enumerate() {
        let line = line.trim_end_matches('\n');
        let spans = apply_line_highlights(line, highlights.get(i), &mut interner);
        for span in &spans {
            out.push_str(interner.resolve(&span.content));
            out.push('\u{1f}');
            out.push_str(&format!(
                "{:?}/{:?}",
                span.style.fg, span.style.add_modifier
            ));
            out.push('\u{1e}');
        }
        out.push('\n');
    }
    out
}

fn assert_render_eq(label: &str, kind: &str, whole: &str, other: &str) {
    if whole == other {
        return;
    }
    let whole_lines: Vec<&str> = whole.lines().collect();
    let other_lines: Vec<&str> = other.lines().collect();
    let mut detail = format!(
        "{label}: {kind} highlight differs from the whole-tree query ({} lines vs {})",
        whole_lines.len(),
        other_lines.len()
    );
    let mut found = false;
    for (i, (left, right)) in whole_lines.iter().zip(&other_lines).enumerate() {
        if left != right {
            detail.push_str(&format!(
                "\nfirst difference at line {i}\n whole: {left}\n other: {right}"
            ));
            found = true;
            break;
        }
    }
    if !found && whole_lines.len() != other_lines.len() {
        detail.push_str("\ncommon prefix matches; lengths differ");
    }
    panic!("{detail}");
}

fn assert_same_highlight(filename: &str, source: &str) {
    let whole = render_source(filename, source, Some(QueryPlan::Whole));
    let slabs = render_source(filename, source, Some(QueryPlan::Slabs));
    let auto = render_source(filename, source, None);
    assert_render_eq(filename, "slab", &whole, &slabs);
    assert_render_eq(filename, "auto", &whole, &auto);
    assert!(
        !whole.is_empty() || source.trim().is_empty(),
        "{filename} produced no highlighted spans"
    );
}

fn tree_depth(filename: &str, source: &str) -> u32 {
    let mut pool = ParserPool::new();
    let highlighter = Highlighter::for_file(filename, "Dracula");
    let parsed = highlighter
        .parse_source(source, &mut pool)
        .unwrap_or_else(|| panic!("{filename} did not parse"));
    visible_tree_depth(&parsed.tree)
}

fn repo_file(path: &str) -> String {
    let root = env!("CARGO_MANIFEST_DIR");
    std::fs::read_to_string(format!("{root}/{path}")).unwrap_or_else(|err| panic!("{path}: {err}"))
}

fn ghc(name: &str) -> String {
    match name {
        "Expr.hs" => include_str!("../../../tests/fixtures/highlight/ghc/Expr.hs").to_string(),
        "Pat.hs" => include_str!("../../../tests/fixtures/highlight/ghc/Pat.hs").to_string(),
        "Type.hs" => include_str!("../../../tests/fixtures/highlight/ghc/Type.hs").to_string(),
        _ => panic!("unknown ghc fixture {name}"),
    }
}

/// One module header, body repeated. Depth stays near the original module.
fn scale_haskell_module(source: &str, line_count: usize) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut last_import = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("import ") {
            last_import = i;
        }
    }
    let header_end = (last_import + 1).min(lines.len());
    let mut out = Vec::with_capacity(line_count);
    out.extend_from_slice(&lines[..header_end]);
    let body = if header_end < lines.len() {
        &lines[header_end..]
    } else {
        &lines[..]
    };
    while out.len() < line_count {
        out.extend_from_slice(body);
    }
    out.truncate(line_count);
    let mut scaled = out.join("\n");
    scaled.push('\n');
    scaled
}

fn language_fixtures() -> Vec<(&'static str, String)> {
    vec![
        (
            "sample.rs",
            r#"
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Config {
    name: String,
    count: u32,
}

impl Config {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), count: 0 }
    }

    pub fn bump(&mut self) -> Result<u32, &'static str> {
        self.count = self.count.checked_add(1).ok_or("overflow")?;
        Ok(self.count)
    }
}

fn main() {
    let mut seen = HashMap::new();
    let mut cfg = Config::new("demo");
    for i in 0..8 {
        seen.insert(i, cfg.bump().unwrap());
    }
    println!("{:?}", cfg);
}
"#
            .to_string(),
        ),
        (
            "sample.ts",
            r#"
import { readFile } from "fs/promises";

interface Row { id: number; name: string }

export async function load(path: string): Promise<Row[]> {
    const text = await readFile(path, "utf8");
    return text.split("\n").filter(Boolean).map((line, id) => {
        const name = line.trim();
        return { id, name };
    });
}

const rows = await load("rows.txt");
console.log(rows[0]?.name ?? "empty");
"#
            .to_string(),
        ),
        (
            "sample.tsx",
            r#"
import React, { useState } from "react";

type Props = { title: string; count: number };

export function Counter({ title, count }: Props) {
    const [value, setValue] = useState(count);
    return (
        <button type="button" onClick={() => setValue(value + 1)}>
            {title}: {value}
        </button>
    );
}
"#
            .to_string(),
        ),
        (
            "sample.js",
            r#"
export function groupBy(items, key) {
    const out = new Map();
    for (const item of items) {
        const bucket = key(item);
        if (!out.has(bucket)) out.set(bucket, []);
        out.get(bucket).push(item);
    }
    return out;
}
"#
            .to_string(),
        ),
        (
            "sample.jsx",
            r#"
export function List({ items }) {
    return (
        <ul>
            {items.map((item) => (
                <li key={item.id}>{item.name}</li>
            ))}
        </ul>
    );
}
"#
            .to_string(),
        ),
        (
            "sample.go",
            r#"
package main

import "fmt"

type Server struct {
    Addr string
    Port int
}

func (s *Server) Listen() error {
    if s.Port == 0 {
        return fmt.Errorf("missing port")
    }
    fmt.Printf("listening on %s:%d\n", s.Addr, s.Port)
    return nil
}
"#
            .to_string(),
        ),
        (
            "sample.py",
            r#"
from dataclasses import dataclass

@dataclass
class Point:
    x: float
    y: float

    def length(self) -> float:
        return (self.x ** 2 + self.y ** 2) ** 0.5

def total(points: list[Point]) -> float:
    return sum(point.length() for point in points)
"#
            .to_string(),
        ),
        (
            "sample.rb",
            r#"
module Demo
  class Cache
    def initialize
      @store = {}
    end

    def fetch(key)
      @store[key] ||= yield
    end
  end
end
"#
            .to_string(),
        ),
        (
            "sample.zig",
            r#"
const std = @import("std");

pub fn add(a: i32, b: i32) i32 {
    return a + b;
}

pub fn main() void {
    const value = add(1, 2);
    std.debug.print("{d}\n", .{value});
}
"#
            .to_string(),
        ),
        (
            "sample.c",
            r#"
#include <stdio.h>

int add(int a, int b) { return a + b; }

int main(void) {
    int value = add(1, 2);
    printf("%d\n", value);
    return 0;
}
"#
            .to_string(),
        ),
        (
            "sample.cpp",
            r#"
#include <string>
#include <vector>

struct User {
    std::string name;
    int age;
};

int adults(const std::vector<User>& users) {
    int count = 0;
    for (const auto& user : users) {
        if (user.age >= 18) count++;
    }
    return count;
}
"#
            .to_string(),
        ),
        (
            "sample.java",
            r#"
import java.util.List;

public final class Total {
    public static int sum(List<Integer> values) {
        int total = 0;
        for (Integer value : values) {
            if (value != null) total += value;
        }
        return total;
    }
}
"#
            .to_string(),
        ),
        (
            "sample.cs",
            r#"
using System.Collections.Generic;

public static class Total {
    public static int Sum(IEnumerable<int> values) {
        int total = 0;
        foreach (var value in values) {
            total += value;
        }
        return total;
    }
}
"#
            .to_string(),
        ),
        (
            "sample.lua",
            r#"
local function add(a, b)
    return a + b
end

local value = add(1, 2)
print(value)
"#
            .to_string(),
        ),
        (
            "sample.sh",
            r#"
#!/usr/bin/env bash
set -euo pipefail

name="${1:-world}"
if [[ -z "$name" ]]; then
    echo "missing name" >&2
    exit 1
fi
echo "hello ${name}"
"#
            .to_string(),
        ),
        (
            "sample.php",
            r#"
<?php
declare(strict_types=1);

function add(int $a, int $b): int {
    return $a + $b;
}

echo add(1, 2), PHP_EOL;
"#
            .to_string(),
        ),
        (
            "sample.swift",
            r#"
struct User {
    let name: String
    var age: Int

    func greet() -> String {
        "Hello, \(name)"
    }
}

let user = User(name: "Ada", age: 36)
print(user.greet())
"#
            .to_string(),
        ),
        (
            "sample.hs",
            r#"
module Demo where

fib :: Integer -> Integer
fib n
  | n <= 0 = 0
  | n == 1 = 1
  | otherwise = fib (n - 1) + fib (n - 2)

process :: [Int] -> [Int]
process xs = map (* 2) xs
"#
            .to_string(),
        ),
        (
            "sample.mbt",
            r#"
fn add(x : Int, y : Int) -> Int {
  x + y
}

fn main {
  println(add(1, 2))
}
"#
            .to_string(),
        ),
        (
            "sample.css",
            r#"
.container {
    display: flex;
    color: #333;
}

@media (max-width: 768px) {
    .container { padding: 8px; }
}
"#
            .to_string(),
        ),
        (
            "sample.svelte",
            r#"
<script lang="ts">
    let count: number = 0;
    function increment() {
        count += 1;
    }
</script>

<button on:click={increment}>{count}</button>

<style>
    button { color: rebeccapurple; }
</style>
"#
            .to_string(),
        ),
        (
            "sample.vue",
            r#"
<script lang="ts">
    const count: number = 0;
    function increment() {
        return count + 1;
    }
</script>

<template>
    <button @click="increment">{{ count }}</button>
</template>

<style>
    button { color: red; }
</style>
"#
            .to_string(),
        ),
        (
            "sample.md",
            "# Title\n\nSome **bold** and *italic* text.\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n"
                .to_string(),
        ),
    ]
}

fn nested_rust() -> String {
    let levels = 24;
    let mut source = String::from("fn deep(flag: bool) -> i32 {\n    let mut value = 0;\n");
    for i in 0..levels {
        source.push_str(&"    ".repeat(i + 1));
        source.push_str(&format!("if flag || value == {i} {{\n"));
    }
    source.push_str(&"    ".repeat(levels + 1));
    source.push_str("value += 1;\n");
    for i in (0..levels).rev() {
        source.push_str(&"    ".repeat(i + 1));
        source.push_str("}\n");
    }
    source.push_str("    value\n}\n");
    source
}

fn fixtures_with_nesting() -> Vec<(&'static str, String)> {
    let mut fixtures = language_fixtures();
    fixtures.insert(1, ("nested.rs", nested_rust()));
    fixtures
}

#[test]
fn slab_highlight_matches_whole_tree_on_language_fixtures() {
    for (filename, source) in fixtures_with_nesting() {
        assert_same_highlight(filename, &source);
    }
}

#[test]
fn slab_highlight_matches_whole_tree_on_repo_rust() {
    let files = ["src/syntax/themes.rs", "src/language.rs", "src/diff/mod.rs"];
    let mut combined = String::new();
    for path in files {
        let source = repo_file(path);
        assert_same_highlight(path, &source);
        combined.push_str(&source);
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
    }
    assert!(
        combined.lines().count() >= 4_000,
        "repo rust fixture is smaller than expected"
    );
    assert_same_highlight("combined.rs", &combined);
}

#[test]
fn slab_highlight_matches_whole_tree_on_real_ghc() {
    for name in ["Expr.hs", "Pat.hs", "Type.hs"] {
        assert_same_highlight(name, &ghc(name));
    }
    let scaled = scale_haskell_module(&ghc("Expr.hs"), 8_000);
    assert_eq!(scaled.lines().count(), 8_000);
    assert_same_highlight("Expr8k.hs", &scaled);

    let mut concatenated = String::new();
    for name in ["Expr.hs", "Pat.hs", "Type.hs"] {
        concatenated.push_str(&ghc(name));
        if !concatenated.ends_with('\n') {
            concatenated.push('\n');
        }
    }
    assert_same_highlight("concatenated.hs", &concatenated);
}

#[test]
fn ordinary_files_stay_on_the_whole_tree_path() {
    let expr = ghc("Expr.hs");
    let expr_depth = tree_depth("Expr.hs", &expr);
    assert!(
        expr_depth <= DEEP_TREE_MIN_DEPTH,
        "GHC.Hs.Expr depth {expr_depth} would leave the whole-tree path"
    );
    let scaled = scale_haskell_module(&expr, 8_000);
    let scaled_depth = tree_depth("Expr8k.hs", &scaled);
    assert!(
        scaled_depth <= DEEP_TREE_MIN_DEPTH,
        "realistic 8k Haskell depth {scaled_depth} would leave the whole-tree path"
    );
    let rust = repo_file("src/diff/mod.rs");
    let rust_depth = tree_depth("mod.rs", &rust);
    assert!(
        rust_depth <= DEEP_TREE_MIN_DEPTH,
        "Rust depth {rust_depth} would leave the whole-tree path"
    );
}

#[test]
fn synthetic_haskell_highlight_matches_and_uses_slabs() {
    let patch = generate_haskell_diff_patch(1_000);
    let source = post_change_source(&patch);
    let depth = tree_depth("Synthetic.hs", &source);
    assert!(
        depth > DEEP_TREE_MIN_DEPTH,
        "synthetic 1k depth {depth} would stay on the superlinear path"
    );
    assert_same_highlight("Synthetic.hs", &source);

    let whole = render_patch("Synthetic.hs", &patch, Some(QueryPlan::Whole));
    let slabs = render_patch("Synthetic.hs", &patch, Some(QueryPlan::Slabs));
    let auto = render_patch("Synthetic.hs", &patch, None);
    assert_render_eq("synthetic diff", "slab", &whole, &slabs);
    assert_render_eq("synthetic diff", "auto", &whole, &auto);
    assert!(
        whole.contains('\u{1f}'),
        "synthetic diff was not highlighted"
    );
}

fn render_patch(filename: &str, patch: &str, plan: Option<QueryPlan>) -> String {
    let _guard = force(plan);
    let mut pool = ParserPool::new();
    let cache = build_diff_cache(patch, filename, "Dracula", &mut pool, false, 4);
    let mut out = String::new();
    for line in &cache.lines {
        out.push_str(&format!("{:?}:", line.line_type));
        for span in &line.spans {
            out.push_str(cache.interner.resolve(&span.content));
            out.push('\u{1f}');
            out.push_str(&format!(
                "{:?}/{:?}",
                span.style.fg, span.style.add_modifier
            ));
            out.push('\u{1e}');
        }
        out.push('\n');
    }
    out
}

fn post_change_source(patch: &str) -> String {
    let mut source = String::new();
    for line in patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            continue;
        }
        if let Some(content) = line.strip_prefix('+').or_else(|| line.strip_prefix(' ')) {
            source.push_str(content);
            source.push('\n');
        }
    }
    source
}

/// Same generator as `benches/common/mod.rs`. Shuffled lines plus a trailing
/// comment, which is what makes the Haskell tree thousands of nodes deep.
fn generate_haskell_diff_patch(line_count: usize) -> String {
    let mut rng = ChaCha8Rng::seed_from_u64(42);
    let mut lines = Vec::with_capacity(line_count);
    lines.push(format!("@@ -1,{} +1,{} @@", line_count / 2, line_count / 2));
    for i in 1..line_count {
        if i % 50 == 0 {
            let current_line = (i as u32 / 50) * 50 + 1;
            lines.push(format!(
                "@@ -{},{} +{},{} @@",
                current_line, 30, current_line, 30
            ));
            continue;
        }
        let line_type: u8 = rng.random_range(0..10);
        let content = generate_haskell_line(&mut rng, i);
        match line_type {
            0..=1 => lines.push(format!("+{content}")),
            2..=3 => lines.push(format!("-{content}")),
            _ => lines.push(format!(" {content}")),
        }
    }
    lines.join("\n")
}

fn generate_haskell_line(rng: &mut ChaCha8Rng, line_num: usize) -> String {
    let templates = [
        "module Main (main, processData) where",
        "import qualified Data.Map.Strict as Map",
        "import Control.Monad.Trans.State (StateT, get, put, evalStateT)",
        "data Tree a = Leaf | Node (Tree a) a (Tree a) deriving (Show, Eq, Functor)",
        "class (Monad m) => MonadReader r m | m -> r where",
        "  ask :: m r",
        "instance Functor Tree where",
        "  fmap f Leaf = Leaf",
        "  fmap f (Node l x r) = Node (fmap f l) (f x) (fmap f r)",
        "type family Elem (xs :: [k]) :: k where",
        "  Elem (x ': _) = x",
        "processData :: (MonadIO m, MonadReader Config m) => FilePath -> m [Result]",
        "processData path = do",
        "  contents <- liftIO $ readFile path",
        "  config <- ask",
        "  let parsed = parseWith (configParser config) contents",
        "  case parsed of",
        "    Left err -> throwError $ ParseError err",
        "    Right val -> pure $ transform val",
        "transform :: Value -> [Result]",
        "transform = \\case",
        "  Object fields -> concatMap transformField (Map.toList fields)",
        "  Array items  -> zipWith transformItem [0..] (toList items)",
        "  _            -> []",
        "  where",
        "    transformField (k, v) = [Result k (show v)]",
        "    transformItem i v = [Result (show i) (show v)]",
        "newtype AppM a = AppM { runAppM :: ReaderT Config (ExceptT AppError IO) a }",
        "  deriving (Functor, Applicative, Monad, MonadIO, MonadReader Config)",
        "{-# LANGUAGE GADTs, TypeFamilies, DataKinds, RankNTypes #-}",
        "-- | Main entry point with error handling",
        "main :: IO ()",
        "main = do",
        "  args <- getArgs",
        "  result <- runExceptT $ runReaderT (runAppM app) defaultConfig",
        "  either (hPutStrLn stderr . show) pure result",
        "fibonacci :: Integer -> Integer",
        "fibonacci n",
        "  | n <= 0    = 0",
        "  | n == 1    = 1",
        "  | otherwise = fibonacci (n - 1) + fibonacci (n - 2)",
    ];
    let idx = rng.random_range(0..templates.len());
    format!("{} -- line {}", templates[idx], line_num)
}

struct AbortAfter {
    hits: AtomicUsize,
    after: usize,
    reason: HighlightAbort,
    limit: Option<u32>,
}

impl HighlightControl for AbortAfter {
    fn poll_abort(&self) -> Option<HighlightAbort> {
        let seen = self.hits.fetch_add(1, Ordering::Relaxed);
        (seen >= self.after).then_some(self.reason)
    }

    fn match_limit(&self) -> Option<u32> {
        self.limit
    }
}

fn highlight_bounded(
    filename: &str,
    source: &str,
    control: &dyn HighlightControl,
) -> Result<(), HighlightAbort> {
    let mut pool = ParserPool::new();
    let highlighter = Highlighter::for_file(filename, "Dracula");
    let parsed = highlighter
        .parse_source(source, &mut pool)
        .expect("source parses");
    let query = pool.get_or_create_query(parsed.lang).expect("query");
    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let style_cache = highlighter.style_cache().expect("style");
    collect_line_highlights_bounded(
        source,
        &parsed.tree,
        query,
        &capture_names,
        style_cache,
        control,
    )
    .map(|_| ())
}

#[test]
fn slab_path_still_honors_cancellation_and_match_limit() {
    let source = post_change_source(&generate_haskell_diff_patch(1_000));
    assert!(tree_depth("Synthetic.hs", &source) > DEEP_TREE_MIN_DEPTH);

    let cancel = AbortAfter {
        hits: AtomicUsize::new(0),
        after: 0,
        reason: HighlightAbort::Cancelled,
        limit: None,
    };
    assert_eq!(
        highlight_bounded("Synthetic.hs", &source, &cancel).err(),
        Some(HighlightAbort::Cancelled)
    );

    let timeout = AbortAfter {
        hits: AtomicUsize::new(0),
        after: 4,
        reason: HighlightAbort::TimedOut,
        limit: Some(65_536),
    };
    assert_eq!(
        highlight_bounded("Synthetic.hs", &source, &timeout).err(),
        Some(HighlightAbort::TimedOut)
    );
    assert!(
        timeout.hits.load(Ordering::Relaxed) > 4,
        "the budget must be polled while the slab query is running"
    );

    let limited = AbortAfter {
        hits: AtomicUsize::new(0),
        after: usize::MAX,
        reason: HighlightAbort::Cancelled,
        limit: Some(1),
    };
    assert_eq!(
        highlight_bounded("Synthetic.hs", &source, &limited).err(),
        Some(HighlightAbort::MatchLimit)
    );
}
