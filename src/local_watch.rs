//! Decide which filesystem events should reload a local diff.
//!
//! `.git` and `.octorus` are ignored outright when the caller asks for that.
//! Everything else is checked against git: tracked files and untracked files
//! that are not gitignored refresh, and a change to `.gitignore` itself always
//! refreshes. Ignored paths (including `target/` and `node_modules/`) do not.
//!
//! The tracked set is loaded on the first event, after `watcher.watch()`, so a
//! write that lands while `git ls-files` runs is still delivered.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use notify::EventKind;

/// Cached `git check-ignore` / `git ls-files` answers for one working tree.
pub struct GitIgnoreCache {
    /// Watch root as given, after dropping `.` components. May be relative.
    raw_root: PathBuf,
    /// Absolute watch root as given, without resolving symlinks.
    given_root: PathBuf,
    /// `canonicalize(given_root)`, or `given_root` when that fails.
    canonical_root: PathBuf,
    /// Directory passed to `git -C`.
    git_root: PathBuf,
    /// `git ls-files` / `rev-parse` has run at least once.
    loaded: bool,
    /// `git rev-parse` succeeded, so a failed check-ignore is "not ignored".
    git_ok: bool,
    /// Index changed. Reload once on the next classification, not per event.
    index_dirty: bool,
    /// Exclude rules changed and still need a reload.
    rules_dirty: bool,
    tracked: HashSet<PathBuf>,
    /// Ancestors of tracked paths. An ignored directory that contains one of
    /// these cannot be skipped wholesale (a force-added file still refreshes).
    tracked_prefixes: HashSet<PathBuf>,
    /// Relative path → ignored?
    decisions: HashMap<PathBuf, bool>,
    /// Ignored directories with no tracked descendants.
    ignored_prefixes: Vec<PathBuf>,
    allow_all: bool,
}

impl GitIgnoreCache {
    /// Never treat a path as gitignored. `.git` / `.octorus` filters still apply.
    pub fn allow_all() -> Self {
        Self {
            raw_root: PathBuf::new(),
            given_root: PathBuf::new(),
            canonical_root: PathBuf::new(),
            git_root: PathBuf::new(),
            loaded: true,
            git_ok: false,
            index_dirty: false,
            rules_dirty: false,
            tracked: HashSet::new(),
            tracked_prefixes: HashSet::new(),
            decisions: HashMap::new(),
            ignored_prefixes: Vec::new(),
            allow_all: true,
        }
    }

    /// Remember the watch root. Git is not queried until the first event.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let raw_root = lexical_clean(&root.into());
        let given_root = logical_absolute(raw_root.clone());
        let canonical_root =
            std::fs::canonicalize(&given_root).unwrap_or_else(|_| given_root.clone());
        Self {
            git_root: canonical_root.clone(),
            raw_root,
            given_root,
            canonical_root,
            loaded: false,
            git_ok: false,
            index_dirty: false,
            rules_dirty: false,
            tracked: HashSet::new(),
            tracked_prefixes: HashSet::new(),
            decisions: HashMap::new(),
            ignored_prefixes: Vec::new(),
            allow_all: false,
        }
    }

    /// Drop cached decisions and reload the index.
    pub fn invalidate(&mut self) {
        if self.allow_all {
            return;
        }
        self.reload();
    }

    fn note_index(&mut self) {
        if !self.allow_all {
            self.index_dirty = true;
        }
    }

    fn note_rules(&mut self) {
        if !self.allow_all {
            self.rules_dirty = true;
        }
    }

    fn flush(&mut self) {
        if self.allow_all {
            return;
        }
        if !self.loaded || self.index_dirty || self.rules_dirty {
            self.reload();
        }
    }

    fn reload(&mut self) {
        self.loaded = true;
        self.index_dirty = false;
        self.rules_dirty = false;
        self.tracked = load_tracked(&self.git_root);
        self.git_ok = tracked_loaded(&self.git_root);
        self.tracked_prefixes = prefixes_of(&self.tracked);
        self.decisions.clear();
        self.ignored_prefixes.clear();
    }

    /// `true` when `path` should not trigger a local refresh.
    pub fn is_ignored(&mut self, path: &Path) -> bool {
        if self.allow_all {
            return false;
        }
        if is_gitignore_file(path) {
            return false;
        }
        self.flush();
        if let Some(ignored) = self.cached_decision(path) {
            return ignored;
        }
        let Some(rel) = self.relativize(path) else {
            return false;
        };
        self.resolve_unknown(std::slice::from_ref(&rel));
        self.cached_decision(path).unwrap_or(false)
    }

    /// `Some(true)` ignored, `Some(false)` refresh, `None` needs git.
    fn cached_decision(&self, path: &Path) -> Option<bool> {
        let rel = self.relativize(path)?;
        if self.tracked.contains(&rel) {
            return Some(false);
        }
        if self
            .ignored_prefixes
            .iter()
            .any(|prefix| rel.starts_with(prefix))
        {
            return Some(true);
        }
        self.decisions.get(&rel).copied()
    }

    fn relativize(&self, path: &Path) -> Option<PathBuf> {
        let path = lexical_clean(path);
        if let Some(rel) = self.strip_known(&path) {
            return Some(rel);
        }
        if path.is_relative() {
            return Some(path);
        }
        // macOS reports the same directory as `/var/...` and `/private/var/...`.
        // Try the other spelling before treating the path as outside the tree.
        let Ok(canon) = std::fs::canonicalize(&path) else {
            return None;
        };
        self.strip_known(&lexical_clean(&canon))
    }

    fn strip_known(&self, path: &Path) -> Option<PathBuf> {
        for root in [&self.given_root, &self.canonical_root, &self.raw_root] {
            if let Some(rel) = strip_root(path, root) {
                return Some(rel);
            }
        }
        None
    }

    fn resolve_unknown(&mut self, rels: &[PathBuf]) {
        if rels.is_empty() || self.allow_all {
            return;
        }
        let mut query = Vec::new();
        let mut seen = HashSet::new();
        for rel in rels {
            let mut acc = PathBuf::new();
            for component in rel.components() {
                acc.push(component);
                if self.decisions.contains_key(&acc) || self.tracked.contains(&acc) {
                    continue;
                }
                if seen.insert(acc.clone()) {
                    query.push(acc.clone());
                }
            }
        }
        let ignored = if self.git_ok {
            git_check_ignore_batch(&self.git_root, &query)
        } else {
            None
        };
        for rel in query {
            let is_ignored = match &ignored {
                Some(set) => set.contains(&rel),
                None if self.git_ok => false,
                None => fallback_ignore(&rel),
            };
            if is_ignored && !self.tracked_prefixes.contains(&rel) {
                self.ignored_prefixes.push(rel.clone());
            }
            self.decisions.insert(rel, is_ignored);
        }
    }
}

pub fn is_git_file(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".git")
}

pub fn is_octorus_config_file(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".octorus")
}

fn is_git_index(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    (name == "index" || name == "index.lock") && is_git_file(path)
}

fn is_exclude_or_config(path: &Path) -> bool {
    if !is_git_file(path) {
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    if name == "config" {
        return true;
    }
    name == "exclude"
        && path
            .parent()
            .and_then(|parent| parent.file_name())
            .is_some_and(|parent| parent == "info")
}

fn is_gitignore_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".gitignore")
}

/// `true` when at least one path is a real content change.
///
/// `refresh_pending` skips git. Index and exclude edits are only recorded so
/// the next classification reloads them. `rescan` with no paths is an overflow
/// recovery and refreshes. The first path that must refresh ends the walk.
pub fn should_refresh_local_change(
    paths: &[PathBuf],
    kind: &EventKind,
    rescan: bool,
    refresh_pending: bool,
    skip_octorus: bool,
    ignore: &mut GitIgnoreCache,
) -> bool {
    if paths.is_empty() && rescan {
        return true;
    }
    if matches!(kind, EventKind::Access(_)) {
        return false;
    }
    if refresh_pending {
        for path in paths {
            if is_git_index(path) {
                ignore.note_index();
            } else if is_exclude_or_config(path) || is_gitignore_file(path) {
                ignore.note_rules();
            }
        }
        return false;
    }
    ignore.flush();

    let mut unknown = Vec::new();
    for path in paths {
        if is_git_index(path) {
            ignore.note_index();
            continue;
        }
        if is_exclude_or_config(path) {
            ignore.invalidate();
            continue;
        }
        if is_git_file(path) || (skip_octorus && is_octorus_config_file(path)) {
            continue;
        }
        if is_gitignore_file(path) {
            ignore.invalidate();
            return true;
        }
        if ignore.index_dirty || ignore.rules_dirty {
            ignore.flush();
        }
        match ignore.cached_decision(path) {
            Some(true) => continue,
            Some(false) => return true,
            None => {
                if let Some(rel) = ignore.relativize(path) {
                    unknown.push(rel);
                } else {
                    // Not under this work tree. Refresh, as an unclassified
                    // path did before ignore filtering existed.
                    return true;
                }
            }
        }
    }
    if unknown.is_empty() {
        return false;
    }
    ignore.resolve_unknown(&unknown);
    for rel in &unknown {
        let ignored = ignore
            .ignored_prefixes
            .iter()
            .any(|prefix| rel.starts_with(prefix))
            || ignore.decisions.get(rel).copied().unwrap_or(false);
        if !ignored {
            return true;
        }
    }
    false
}

fn strip_root(path: &Path, root: &Path) -> Option<PathBuf> {
    if root.as_os_str().is_empty() {
        return None;
    }
    let rel = lexical_clean(path.strip_prefix(root).ok()?);
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel)
    }
}

fn logical_absolute(path: PathBuf) -> PathBuf {
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    };
    lexical_clean(&abs)
}

/// Drop `.` and collapse `..` without resolving symlinks.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::ParentDir) | None => out.push(".."),
                Some(Component::RootDir) | Some(Component::Prefix(_)) | Some(Component::CurDir) => {
                }
            },
            other => out.push(other),
        }
    }
    out
}

fn prefixes_of(tracked: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut prefixes = HashSet::new();
    for path in tracked {
        let mut acc = PathBuf::new();
        let components: Vec<_> = path.components().collect();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            if matches!(component, Component::CurDir) {
                continue;
            }
            acc.push(component);
            prefixes.insert(acc.clone());
        }
    }
    prefixes
}

fn load_tracked(root: &Path) -> HashSet<PathBuf> {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .filter_map(|path| std::str::from_utf8(path).ok())
        .map(|path| lexical_clean(Path::new(path)))
        .collect()
}

fn tracked_loaded(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Ignored paths from one `git check-ignore --stdin -z`. `None` if git could not answer.
fn git_check_ignore_batch(root: &Path, rels: &[PathBuf]) -> Option<HashSet<PathBuf>> {
    if rels.is_empty() {
        return Some(HashSet::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    {
        let mut stdin = child.stdin.take()?;
        for rel in rels {
            stdin.write_all(rel.to_string_lossy().as_bytes()).ok()?;
            stdin.write_all(&[0]).ok()?;
        }
    }
    let output = child.wait_with_output().ok()?;
    match output.status.code() {
        Some(0) | Some(1) => Some(
            output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .filter_map(|path| std::str::from_utf8(path).ok())
                .map(|path| lexical_clean(Path::new(path)))
                .collect(),
        ),
        _ => None,
    }
}

fn fallback_ignore(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str();
        name == "target" || name == "node_modules"
    })
}
