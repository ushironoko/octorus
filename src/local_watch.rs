//! Decide which filesystem events should reload a local diff.
//!
//! `.git` and `.octorus` are ignored outright. Everything else is checked
//! against git: tracked files and untracked files that are not gitignored
//! refresh, and a change to `.gitignore` itself always refreshes. Ignored
//! paths (including `target/` and `node_modules/`) do not.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use notify::EventKind;

/// Cached `git check-ignore` / `git ls-files` answers for one working tree.
pub struct GitIgnoreCache {
    root: PathBuf,
    /// `git ls-files` succeeded, so check-ignore results are trustworthy.
    git_ok: bool,
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
            root: PathBuf::new(),
            git_ok: false,
            tracked: HashSet::new(),
            tracked_prefixes: HashSet::new(),
            decisions: HashMap::new(),
            ignored_prefixes: Vec::new(),
            allow_all: true,
        }
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let tracked = load_tracked(&root);
        let git_ok = tracked_loaded(&root);
        let tracked_prefixes = prefixes_of(&tracked);
        Self {
            root,
            git_ok,
            tracked,
            tracked_prefixes,
            decisions: HashMap::new(),
            ignored_prefixes: Vec::new(),
            allow_all: false,
        }
    }

    /// Drop cached decisions and reload the index. Called when `.gitignore`
    /// or `.git/index` changes.
    pub fn invalidate(&mut self) {
        if self.allow_all {
            return;
        }
        self.tracked = load_tracked(&self.root);
        self.git_ok = tracked_loaded(&self.root);
        self.tracked_prefixes = prefixes_of(&self.tracked);
        self.decisions.clear();
        self.ignored_prefixes.clear();
    }

    /// `true` when `path` should not trigger a local refresh.
    pub fn is_ignored(&mut self, path: &Path) -> bool {
        if self.allow_all {
            return false;
        }
        if path.file_name().is_some_and(|name| name == ".gitignore") {
            return false;
        }
        let Some(rel) = self.relativize(path) else {
            return fallback_ignore(path);
        };
        if self.tracked.contains(&rel) {
            return false;
        }
        if self
            .ignored_prefixes
            .iter()
            .any(|prefix| rel.starts_with(prefix))
        {
            return true;
        }
        if let Some(ignored) = self.decisions.get(&rel) {
            return *ignored;
        }

        let components: Vec<_> = rel.components().collect();
        let mut acc = PathBuf::new();
        for (i, component) in components.iter().enumerate() {
            acc.push(component);
            let is_last = i + 1 == components.len();
            if self.tracked.contains(&acc) {
                return false;
            }
            if let Some(ignored) = self.decisions.get(&acc).copied() {
                if ignored && !self.tracked_prefixes.contains(&acc) {
                    return true;
                }
                if !is_last {
                    continue;
                }
                return ignored;
            }
            let ignored = self.check_ignore(&acc);
            self.decisions.insert(acc.clone(), ignored);
            if ignored && !self.tracked_prefixes.contains(&acc) {
                self.ignored_prefixes.push(acc.clone());
                return true;
            }
            if is_last {
                return ignored;
            }
        }
        false
    }

    fn relativize(&self, path: &Path) -> Option<PathBuf> {
        if let Ok(rel) = path.strip_prefix(&self.root) {
            if rel.as_os_str().is_empty() {
                return None;
            }
            return Some(rel.to_path_buf());
        }
        if path.is_relative() {
            return Some(path.to_path_buf());
        }
        None
    }

    fn check_ignore(&self, rel: &Path) -> bool {
        match git_check_ignore(&self.root, rel) {
            Some(ignored) => ignored,
            None if self.git_ok => false,
            None => fallback_ignore(rel),
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

fn is_gitignore_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".gitignore")
}

/// `true` when at least one path is a real content change.
pub fn should_refresh_local_change(
    paths: &[PathBuf],
    kind: &EventKind,
    ignore: &mut GitIgnoreCache,
) -> bool {
    if matches!(kind, EventKind::Access(_)) {
        return false;
    }
    let mut refresh = false;
    for path in paths {
        if is_git_index(path) {
            ignore.invalidate();
        }
        if is_git_file(path) || is_octorus_config_file(path) {
            continue;
        }
        if is_gitignore_file(path) {
            ignore.invalidate();
            refresh = true;
            continue;
        }
        if !ignore.is_ignored(path) {
            refresh = true;
        }
    }
    refresh
}

fn prefixes_of(tracked: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut prefixes = HashSet::new();
    for path in tracked {
        let mut acc = PathBuf::new();
        let components: Vec<_> = path.components().collect();
        for component in components.iter().take(components.len().saturating_sub(1)) {
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
        .map(PathBuf::from)
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

/// `Some(true)` ignored, `Some(false)` not ignored, `None` if git could not answer.
fn git_check_ignore(root: &Path, rel: &Path) -> Option<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", "--"])
        .arg(rel)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

fn fallback_ignore(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str();
        name == "target" || name == "node_modules"
    })
}
