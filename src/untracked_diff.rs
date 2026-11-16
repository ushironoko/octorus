//! Untracked files have no blob in `HEAD`, so each one is a
//! `git diff --no-index -- /dev/null <file>`.
//!
//! A refresh used to run that command again for every file. The text is reused
//! when the file's stat and the git config that can change a diff are unchanged.
//! A stat whose mtime or ctime falls within two seconds of the read is not
//! stored: on a coarse filesystem a same-size rewrite in that tick would not
//! move the timestamp, and the old text would stay cached. The file on screen
//! is never served from the cache. Several files that miss are read by one
//! `git diff --no-index` of a hardlink tree when that output rewrites to the
//! per-file text. Attributes, custom diff prefixes, directories, and a rewrite
//! that does not line up stay on the per-file command.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tokio::process::Command;
use tokio::sync::watch;

/// One untracked path. Empty stdout becomes `None`, matching the old loader.
///
/// This is the file on screen. It always runs git, including when a batch
/// already cached the same path. Call [`require_fresh`] before the batch so
/// that batch does not fill the screen from the cache first.
pub(crate) async fn patch(workdir: Option<&str>, rel: &str) -> Option<String> {
    let diffed = diff_inflight_for(workdir, rel).await;
    nonempty(diffed.patch)
}

/// The next [`patches`] call for this work tree re-diffs `rel` instead of
/// reusing a cached patch. The screen asks for this before either git runs.
pub(crate) fn require_fresh(workdir: Option<&str>, rel: &str) {
    let Ok(root) = std::fs::canonicalize(work_path(workdir)) else {
        return;
    };
    lock(fresh_paths()).insert((root, rel.to_string()));
}

/// Patches aligned with `rels`. An empty list does not spawn git.
pub(crate) async fn patches(workdir: Option<&str>, rels: &[String]) -> Vec<Option<String>> {
    if rels.is_empty() {
        return Vec::new();
    }
    let Some(ctx) = prepare(workdir, rels).await else {
        let mut out = Vec::with_capacity(rels.len());
        for rel in rels {
            out.push(diff_no_index_in(workdir, rel).await);
        }
        return out;
    };

    let fresh = take_fresh(&ctx.root);
    let mut out = vec![None; rels.len()];
    let mut pending = Vec::new();
    for (index, rel) in rels.iter().enumerate() {
        if fresh.contains(rel.as_str()) {
            pending.push(index);
        } else if let Some(hit) = cached_hit(&ctx, rel) {
            out[index] = Some(hit);
        } else {
            pending.push(index);
        }
    }
    if pending.is_empty() {
        return out;
    }

    let mut overlay = Vec::new();
    let mut single = Vec::new();
    for index in pending {
        if ctx.allow_overlay
            && overlay_name_ok(&rels[index])
            && is_overlay_file(&ctx.work, &rels[index])
        {
            overlay.push(index);
        } else {
            single.push(index);
        }
    }

    if overlay.len() >= 2 {
        let names: Vec<String> = overlay.iter().map(|&index| rels[index].clone()).collect();
        let started = std::time::SystemTime::now();
        let before: Vec<Option<StatKey>> = overlay
            .iter()
            .map(|&index| stat_rel(&ctx.work, &rels[index]))
            .collect();
        match try_overlay(&ctx.work, &names).await {
            Some(mut mapped) => {
                for (slot, index) in overlay.iter().copied().enumerate() {
                    let Some(patch) = mapped.remove(&rels[index]) else {
                        single.push(index);
                        continue;
                    };
                    if ctx.cache_ok && relative_safe(&rels[index]) {
                        if let (Some(before), Some(after)) =
                            (before[slot], stat_rel(&ctx.work, &rels[index]))
                        {
                            // `before` is the stat from before the hardlink. The
                            // link updates ctime, so the racy-tick check uses
                            // that earlier stat and the stored key keeps `after`.
                            if before.same_body(after) && !racy(before, started) {
                                store(&ctx.root, &ctx.fp, &rels[index], after, patch.clone());
                            }
                        }
                    }
                    out[index] = Some(patch);
                }
            }
            None => {
                tracing::debug!(
                    "untracked overlay diff did not map onto per-file paths; using per-file git diff"
                );
                single.extend(overlay);
            }
        }
    } else {
        single.extend(overlay);
    }

    for index in single {
        if out[index].is_some() {
            continue;
        }
        out[index] = diff_with_ctx(&ctx, &rels[index]).await;
    }
    out
}

/// Keep cached patches only for untracked paths still on the list.
pub(crate) fn retain(workdir: Option<&str>, paths: &[String]) {
    let Some(root) = std::fs::canonicalize(work_path(workdir)).ok() else {
        return;
    };
    let keep: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut guard = lock(cache());
    let Some(entry) = guard.get_mut(&root) else {
        return;
    };
    entry.entries.retain(|name, _| keep.contains(name.as_str()));
}

struct Context {
    root: PathBuf,
    work: PathBuf,
    fp: Fingerprint,
    allow_overlay: bool,
    cache_ok: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    config: u128,
    attrs: Vec<(PathBuf, StatKey)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StatKey {
    len: u64,
    mtime: u64,
    mtime_nsec: u32,
    ctime: u64,
    ctime_nsec: u32,
    mode: u32,
    ino: u64,
    dev: u64,
}

struct WorkdirCache {
    fingerprint: Fingerprint,
    entries: HashMap<String, (StatKey, String)>,
}

#[derive(Clone)]
struct DiffOut {
    patch: Option<String>,
    stable: Option<StatKey>,
    /// `SystemTime::now()` from before the pre-diff stat.
    started: std::time::SystemTime,
}

impl DiffOut {
    fn empty() -> Self {
        Self {
            patch: None,
            stable: None,
            started: std::time::UNIX_EPOCH,
        }
    }
}

/// Coarse filesystems (ext2, HFS+, SMB, NFS, FAT) do not move mtime or ctime
/// for a same-size rewrite inside the current tick. Two seconds covers FAT.
const RACY_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

fn racy(stat: StatKey, started: std::time::SystemTime) -> bool {
    near(stat.mtime, stat.mtime_nsec, started) || near(stat.ctime, stat.ctime_nsec, started)
}

fn near(secs: u64, nsec: u32, started: std::time::SystemTime) -> bool {
    let nanos = nsec.min(999_999_999);
    let Some(stamp) = std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(secs, nanos))
    else {
        return true;
    };
    let gap = stamp
        .duration_since(started)
        .unwrap_or_else(|err| err.duration());
    gap <= RACY_WINDOW
}

impl StatKey {
    /// Content identity. A hardlink used to batch diffs updates `ctime` and
    /// drops it again when the temporary link is removed, so `ctime` is not
    /// part of this comparison. The stored key still keeps the `ctime` from
    /// after that link is gone.
    fn same_body(self, other: Self) -> bool {
        self.len == other.len
            && self.mtime == other.mtime
            && self.mtime_nsec == other.mtime_nsec
            && self.mode == other.mode
            && self.ino == other.ino
            && self.dev == other.dev
    }
}

struct Probe {
    prefixes_ok: bool,
    attributes_file: Option<String>,
    unresolved: bool,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, WorkdirCache>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, WorkdirCache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

type ProbeBody = Option<ProbeRaw>;
type ProbeTx = watch::Sender<Option<ProbeBody>>;
type ProbeFlights = Mutex<HashMap<PathBuf, ProbeTx>>;
type DiffKey = (PathBuf, String);
type DiffTx = watch::Sender<Option<DiffOut>>;
type DiffFlights = Mutex<HashMap<DiffKey, DiffTx>>;
type FreshPaths = Mutex<HashSet<(PathBuf, String)>>;

fn probe_flights() -> &'static ProbeFlights {
    static FLIGHTS: OnceLock<ProbeFlights> = OnceLock::new();
    FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn fresh_paths() -> &'static FreshPaths {
    static FRESH: OnceLock<FreshPaths> = OnceLock::new();
    FRESH.get_or_init(|| Mutex::new(HashSet::new()))
}

fn take_fresh(root: &Path) -> HashSet<String> {
    let mut guard = lock(fresh_paths());
    let names: Vec<String> = guard
        .iter()
        .filter(|(dir, _)| dir == root)
        .map(|(_, name)| name.clone())
        .collect();
    for name in &names {
        guard.remove(&(root.to_path_buf(), name.clone()));
    }
    names.into_iter().collect()
}

fn diff_flights() -> &'static DiffFlights {
    static FLIGHTS: OnceLock<DiffFlights> = OnceLock::new();
    FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn work_path(workdir: Option<&str>) -> PathBuf {
    match workdir {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

fn nonempty(patch: Option<String>) -> Option<String> {
    patch.filter(|text| !text.is_empty())
}

async fn prepare(workdir: Option<&str>, rels: &[String]) -> Option<Context> {
    let work = work_path(workdir);
    let root = std::fs::canonicalize(&work).ok()?;
    let raw = git_probe(&work).await?;
    let hash = hash_bytes(&raw.config);
    let probe = parse_probe(&raw.config);
    let (attrs, attr_unresolved) =
        attribute_stats(&work, rels, probe.attributes_file.as_deref(), &raw);
    let unresolved = probe.unresolved || attr_unresolved || !raw.paths_ok;
    let fp = Fingerprint {
        config: hash,
        attrs,
    };
    let cache_ok = !unresolved;
    let allow_overlay = probe.prefixes_ok && fp.attrs.is_empty() && cache_ok;
    if cache_ok {
        begin(&root, &fp);
    }
    Some(Context {
        root,
        work,
        fp,
        allow_overlay,
        cache_ok,
    })
}

fn cached_hit(ctx: &Context, rel: &str) -> Option<String> {
    if !ctx.cache_ok {
        return None;
    }
    let stat = stat_rel(&ctx.work, rel)?;
    lookup(&ctx.root, &ctx.fp, rel, stat)
}

async fn diff_with_ctx(ctx: &Context, rel: &str) -> Option<String> {
    if let Some(hit) = cached_hit(ctx, rel) {
        return Some(hit);
    }
    let diffed = diff_inflight(&ctx.work, &ctx.root, rel).await;
    remember(ctx, rel, &diffed);
    nonempty(diffed.patch)
}

fn remember(ctx: &Context, rel: &str, diffed: &DiffOut) {
    if !ctx.cache_ok || !relative_safe(rel) {
        return;
    }
    let (Some(stat), Some(patch)) = (diffed.stable, diffed.patch.clone()) else {
        return;
    };
    if patch.is_empty() || racy(stat, diffed.started) {
        return;
    }
    store(&ctx.root, &ctx.fp, rel, stat, patch);
}

fn begin(root: &Path, fp: &Fingerprint) {
    let mut guard = lock(cache());
    match guard.get_mut(root) {
        Some(entry) if entry.fingerprint == *fp => {}
        Some(entry) => {
            entry.entries.clear();
            entry.fingerprint = fp.clone();
        }
        None => {
            guard.insert(
                root.to_path_buf(),
                WorkdirCache {
                    fingerprint: fp.clone(),
                    entries: HashMap::new(),
                },
            );
        }
    }
}

fn lookup(root: &Path, fp: &Fingerprint, rel: &str, stat: StatKey) -> Option<String> {
    let guard = lock(cache());
    let entry = guard.get(root)?;
    if entry.fingerprint != *fp {
        return None;
    }
    entry
        .entries
        .get(rel)
        .and_then(|(cached, patch)| (*cached == stat).then(|| patch.clone()))
}

fn store(root: &Path, fp: &Fingerprint, rel: &str, stat: StatKey, patch: String) {
    let mut guard = lock(cache());
    let Some(entry) = guard.get_mut(root) else {
        return;
    };
    if entry.fingerprint != *fp {
        return;
    }
    entry.entries.insert(rel.to_string(), (stat, patch));
}

#[cfg(test)]
fn cached_count(work: &Path) -> usize {
    let Ok(root) = std::fs::canonicalize(work) else {
        return 0;
    };
    lock(cache())
        .get(&root)
        .map(|entry| entry.entries.len())
        .unwrap_or(0)
}

fn stat_rel(work: &Path, rel: &str) -> Option<StatKey> {
    let meta = std::fs::symlink_metadata(work.join(rel)).ok()?;
    Some(stat_key(&meta))
}

fn stat_key(meta: &std::fs::Metadata) -> StatKey {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        StatKey {
            len: meta.len(),
            mtime: u64::try_from(meta.mtime()).unwrap_or(0),
            mtime_nsec: u32::try_from(meta.mtime_nsec()).unwrap_or(0),
            ctime: u64::try_from(meta.ctime()).unwrap_or(0),
            ctime_nsec: u32::try_from(meta.ctime_nsec()).unwrap_or(0),
            mode: meta.mode(),
            ino: meta.ino(),
            dev: meta.dev(),
        }
    }
    #[cfg(not(unix))]
    {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        StatKey {
            len: meta.len(),
            mtime,
            mtime_nsec: 0,
            ctime: 0,
            ctime_nsec: 0,
            mode: 0,
            ino: 0,
            dev: 0,
        }
    }
}

fn existing_file_stat(path: &Path) -> Option<StatKey> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.is_dir() {
        None
    } else {
        Some(stat_key(&meta))
    }
}

#[derive(Clone)]
struct ProbeRaw {
    config: Vec<u8>,
    system_attr: PathBuf,
    global_attr: Option<PathBuf>,
    info_attrs: Vec<PathBuf>,
    bases: Vec<PathBuf>,
    paths_ok: bool,
}

async fn git_probe(work: &Path) -> Option<ProbeRaw> {
    let key = std::fs::canonicalize(work).unwrap_or_else(|_| work.to_path_buf());
    match enlist(probe_flights(), key.clone()) {
        Role::Follower(rx) => wait(rx).await.flatten(),
        Role::Leader(tx) => {
            let mut flight = FlightGuard {
                map: probe_flights(),
                key: Some(key),
                tx: Some(tx),
                value: None,
            };
            flight.value = git_probe_raw(work).await;
            flight.value.clone()
        }
    }
}

async fn git_probe_raw(work: &Path) -> Option<ProbeRaw> {
    let (config, paths, vars) =
        tokio::join!(config_list_raw(work), attr_paths(work), attr_vars(work));
    let config = config?;
    let (info_attrs, bases, paths_ok) = paths;
    let (system_attr, global_attr) = vars;
    Some(ProbeRaw {
        config,
        system_attr,
        global_attr,
        info_attrs,
        bases,
        paths_ok,
    })
}

/// `git rev-parse --git-path info/attributes` and `--git-common-dir`.
/// Relative attribute paths are opened from the work tree and from that
/// common directory. A linked worktree's `.git` is a file, so `work/.git`
/// is not where git reads `info/attributes`.
async fn attr_paths(work: &Path) -> (Vec<PathBuf>, Vec<PathBuf>, bool) {
    let work_base = vec![work.to_path_buf()];
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/attributes",
            "--git-common-dir",
        ])
        .current_dir(work)
        .output()
        .await
        .ok();
    let Some(output) = output else {
        return (Vec::new(), work_base, false);
    };
    if !output.status.success() {
        return (Vec::new(), work_base, false);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let (Some(info), Some(common)) = (lines.next(), lines.next()) else {
        return (Vec::new(), work_base, false);
    };
    let info_path = PathBuf::from(info);
    let common_path = PathBuf::from(common);
    let mut info_attrs = vec![info_path.clone()];
    let common_info = common_path.join("info").join("attributes");
    if common_info != info_path {
        info_attrs.push(common_info);
    }
    let mut bases = vec![work.to_path_buf(), common_path];
    if let Some(git_dir) = info_path.parent().and_then(Path::parent) {
        bases.push(git_dir.to_path_buf());
    }
    bases.sort();
    bases.dedup();
    (info_attrs, bases, true)
}

/// System and global attribute files. `git var` knows the install prefix
/// (`GIT_ATTR_SYSTEM` on git 2.42+). The rest of this output is dropped: it
/// repeats config, including credentials, and a timestamp that must not be
/// part of the fingerprint.
async fn attr_vars(work: &Path) -> (PathBuf, Option<PathBuf>) {
    let fallback = PathBuf::from("/etc/gitattributes");
    let output = Command::new("git")
        .args(["var", "-l"])
        .current_dir(work)
        .output()
        .await
        .ok();
    let Some(output) = output.filter(|output| output.status.success()) else {
        return (fallback, None);
    };
    let mut system = None;
    let mut global = None;
    for line in output.stdout.split(|byte| *byte == b'\n') {
        if let Some(rest) = line.strip_prefix(b"GIT_ATTR_SYSTEM=") {
            if let Some(path) = attr_var_path(rest) {
                system = Some(path);
            }
        } else if let Some(rest) = line.strip_prefix(b"GIT_ATTR_GLOBAL=") {
            if let Some(path) = attr_var_path(rest) {
                global = Some(path);
            }
        }
    }
    (system.unwrap_or(fallback), global)
}

fn attr_var_path(bytes: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

async fn config_list_raw(work: &Path) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(["config", "--list", "--null"])
        .current_dir(work)
        .output()
        .await
        .ok()?;
    if output.status.success() || output.stdout.is_empty() {
        Some(output.stdout)
    } else {
        None
    }
}

fn parse_probe(bytes: &[u8]) -> Probe {
    let mut prefixes_ok = true;
    let mut attributes_file = None;
    let mut unresolved = false;
    for rec in bytes.split(|byte| *byte == 0) {
        if rec.is_empty() {
            continue;
        }
        let Some(split) = rec.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        let key = String::from_utf8_lossy(&rec[..split])
            .trim()
            .to_ascii_lowercase();
        let value = &rec[split + 1..];
        match key.as_str() {
            "diff.noprefix" | "diff.mnemonicprefix" | "diff.relative" if git_bool_true(value) => {
                prefixes_ok = false;
            }
            "diff.srcprefix" | "diff.dstprefix" => {
                prefixes_ok = false;
            }
            "core.attributesfile" => match attributes_file_raw(value) {
                AttrRaw::Empty => {}
                AttrRaw::Path(path) => attributes_file = Some(path),
                AttrRaw::Unresolved => unresolved = true,
            },
            _ => {}
        }
    }
    Probe {
        prefixes_ok,
        attributes_file,
        unresolved,
    }
}

enum AttrRaw {
    Empty,
    Path(String),
    Unresolved,
}

fn attributes_file_raw(value: &[u8]) -> AttrRaw {
    if value.iter().all(|byte| byte.is_ascii_whitespace()) {
        return AttrRaw::Empty;
    }
    let Ok(text) = std::str::from_utf8(value) else {
        return AttrRaw::Unresolved;
    };
    let text = text.trim();
    if text.is_empty() {
        return AttrRaw::Empty;
    }
    if text.starts_with('~') && !text.starts_with("~/") {
        return AttrRaw::Unresolved;
    }
    AttrRaw::Path(text.to_string())
}

fn git_bool_true(value: &[u8]) -> bool {
    let text = String::from_utf8_lossy(value);
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

fn attribute_stats(
    work: &Path,
    rels: &[String],
    configured: Option<&str>,
    raw: &ProbeRaw,
) -> (Vec<(PathBuf, StatKey)>, bool) {
    let mut unresolved = false;
    let mut paths = Vec::new();
    paths.push(raw.system_attr.clone());
    if let Some(global) = &raw.global_attr {
        paths.push(global.clone());
    }
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) if !xdg.is_empty() => paths.push(PathBuf::from(xdg).join("git/attributes")),
        _ => {
            if let Some(home) = std::env::var_os("HOME") {
                paths.push(PathBuf::from(home).join(".config/git/attributes"));
            }
        }
    }
    paths.extend(raw.info_attrs.iter().cloned());
    if raw.paths_ok {
        // A normal repository still has `work/.git/info/attributes`. Keep it
        // so a fingerprint does not depend on rev-parse spelling alone.
        paths.push(work.join(".git").join("info").join("attributes"));
    }
    if let Some(value) = configured {
        match expand_attr(&raw.bases, value) {
            Some(expanded) => paths.extend(expanded),
            None => unresolved = true,
        }
    }
    for rel in rels {
        if !relative_safe(rel) {
            continue;
        }
        let mut dir = Path::new(rel).parent();
        while let Some(parent) = dir {
            paths.push(work.join(parent).join(".gitattributes"));
            if parent.as_os_str().is_empty() {
                break;
            }
            dir = parent.parent();
        }
    }
    paths.sort();
    paths.dedup();
    let mut stats = Vec::new();
    for path in paths {
        if let Some(stat) = existing_file_stat(&path) {
            stats.push((path, stat));
        }
    }
    (stats, unresolved)
}

/// `None` means the path could not be resolved (`~user` without a home, or
/// `~/` with no home). Git opens a relative `core.attributesFile` from the
/// work tree; a linked worktree also needs the common git dir, because
/// `work/.git` is a file there.
fn expand_attr(bases: &[PathBuf], raw: &str) -> Option<Vec<PathBuf>> {
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME")?;
        return Some(vec![PathBuf::from(home).join(rest)]);
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return Some(vec![path]);
    }
    if bases.is_empty() {
        return None;
    }
    Some(bases.iter().map(|base| base.join(&path)).collect())
}

fn hash_bytes(bytes: &[u8]) -> u128 {
    let mut hi: u64 = 0x6a09_e667_f3bc_c909;
    let mut lo: u64 = 0xbb67_ae85_84ca_a73b;
    for &byte in bytes {
        hi = hi
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(u64::from(byte));
        lo = lo.rotate_left(5) ^ hi;
    }
    hi ^= bytes.len() as u64;
    (u128::from(hi) << 64) | u128::from(lo)
}

fn relative_safe(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') || rel.contains('\n') || rel.contains('\r') {
        return false;
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

fn overlay_name_ok(rel: &str) -> bool {
    relative_safe(rel)
        && !rel.contains('\t')
        && !rel.contains(" a/")
        && !rel.contains(" b/")
        && !rel.ends_with(' ')
}

fn is_overlay_file(work: &Path, rel: &str) -> bool {
    #[cfg(unix)]
    {
        let Ok(meta) = std::fs::symlink_metadata(work.join(rel)) else {
            return false;
        };
        meta.file_type().is_symlink() || meta.is_file()
    }
    #[cfg(not(unix))]
    {
        let _ = (work, rel);
        false
    }
}

enum Role<T> {
    Leader(watch::Sender<Option<T>>),
    Follower(watch::Receiver<Option<T>>),
}

fn enlist<K, T>(map: &Mutex<HashMap<K, watch::Sender<Option<T>>>>, key: K) -> Role<T>
where
    K: Eq + std::hash::Hash,
{
    let mut guard = lock(map);
    if let Some(tx) = guard.get(&key) {
        return Role::Follower(tx.subscribe());
    }
    let (tx, _rx) = watch::channel(None);
    guard.insert(key, tx.clone());
    Role::Leader(tx)
}

struct FlightGuard<K, T>
where
    K: Eq + std::hash::Hash + 'static,
    T: Clone + 'static,
{
    map: &'static Mutex<HashMap<K, watch::Sender<Option<T>>>>,
    key: Option<K>,
    tx: Option<watch::Sender<Option<T>>>,
    value: T,
}

impl<K, T> Drop for FlightGuard<K, T>
where
    K: Eq + std::hash::Hash + 'static,
    T: Clone + 'static,
{
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Some(self.value.clone()));
        }
        if let Some(key) = self.key.take() {
            lock(self.map).remove(&key);
        }
    }
}

async fn wait<T: Clone>(mut rx: watch::Receiver<Option<T>>) -> Option<T> {
    loop {
        if let Some(value) = rx.borrow().clone() {
            return Some(value);
        }
        if rx.changed().await.is_err() {
            return None;
        }
    }
}

async fn diff_inflight_for(workdir: Option<&str>, rel: &str) -> DiffOut {
    let work = work_path(workdir);
    match std::fs::canonicalize(&work) {
        Ok(root) => diff_inflight(&work, &root, rel).await,
        Err(_) => DiffOut {
            patch: diff_no_index(Some(&work), rel).await,
            stable: None,
            started: std::time::SystemTime::now(),
        },
    }
}

async fn diff_no_index_in(workdir: Option<&str>, rel: &str) -> Option<String> {
    nonempty(diff_no_index(Some(&work_path(workdir)), rel).await)
}

async fn diff_inflight(work: &Path, root: &Path, rel: &str) -> DiffOut {
    let key = (root.to_path_buf(), rel.to_string());
    match enlist(diff_flights(), key.clone()) {
        Role::Follower(rx) => wait(rx).await.unwrap_or_else(DiffOut::empty),
        Role::Leader(tx) => {
            let mut flight = FlightGuard {
                map: diff_flights(),
                key: Some(key),
                tx: Some(tx),
                value: DiffOut::empty(),
            };
            let started = std::time::SystemTime::now();
            let before = stat_rel(work, rel);
            flight.value.started = started;
            flight.value.patch = diff_no_index(Some(work), rel).await;
            flight.value.stable = match (before, stat_rel(work, rel)) {
                (Some(before), Some(after)) if before.same_body(after) => Some(after),
                _ => None,
            };
            flight.value.clone()
        }
    }
}

async fn diff_no_index(work: Option<&Path>, rel: &str) -> Option<String> {
    let mut command = Command::new("git");
    command.args([
        "-c",
        "core.quotePath=false",
        "diff",
        "--no-ext-diff",
        "--no-color",
        "--no-index",
        "--",
        "/dev/null",
        rel,
    ]);
    if let Some(dir) = work {
        command.current_dir(dir);
    }
    let output = command.output().await.ok()?;
    interpret(output)
}

fn interpret(output: std::process::Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() && stdout.trim().is_empty() && !output.stderr.is_empty() {
        return None;
    }
    nonempty(Some(stdout))
}

async fn git_output(work: &Path, args: &[OsString]) -> Option<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(work)
        .output()
        .await
        .ok()
}

async fn try_overlay(work: &Path, rels: &[String]) -> Option<HashMap<String, String>> {
    #[cfg(unix)]
    {
        try_overlay_unix(work, rels).await
    }
    #[cfg(not(unix))]
    {
        let _ = (work, rels);
        None
    }
}

#[cfg(unix)]
const OVERLAY_PREFIX: &str = "octorus-untracked-";

fn sweep_overlay_temps() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        let Some(cutoff) =
            std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(60))
        else {
            return;
        };
        for entry in dir.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.starts_with(OVERLAY_PREFIX) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            // A live overlay is seconds old. Crash leftovers are older.
            if meta
                .modified()
                .ok()
                .is_some_and(|modified| modified < cutoff)
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
}

async fn try_overlay_unix(work: &Path, rels: &[String]) -> Option<HashMap<String, String>> {
    sweep_overlay_temps();
    let tmp = tempfile::Builder::new()
        .prefix(OVERLAY_PREFIX)
        .tempdir()
        .ok()?;
    let empty = tmp.path().join("empty");
    let ovl = tmp.path().join("ovl");
    std::fs::create_dir(&empty).ok()?;
    std::fs::create_dir(&ovl).ok()?;
    for rel in rels {
        let src = work.join(rel);
        let dest = ovl.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let meta = std::fs::symlink_metadata(&src).ok()?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&src).ok()?;
            std::os::unix::fs::symlink(&target, &dest).ok()?;
        } else if meta.is_file() {
            std::fs::hard_link(&src, &dest).ok()?;
        } else {
            return None;
        }
    }
    let mut args = Vec::with_capacity(9);
    for part in [
        "-c",
        "core.quotePath=false",
        "diff",
        "--no-ext-diff",
        "--no-color",
        "--no-index",
        "--",
    ] {
        args.push(OsString::from(part));
    }
    args.push(empty.into_os_string());
    args.push(ovl.into_os_string());
    let output = git_output(work, &args).await?;
    let stdout = interpret(output)?;
    rewrite_overlay(&stdout, rels)
}

fn rewrite_overlay(stdout: &str, rels: &[String]) -> Option<HashMap<String, String>> {
    let prefix = find_prefix(stdout, rels)?;
    let a_pat = format!("a/{prefix}");
    let b_pat = format!("b/{prefix}");
    let mut rewritten = String::with_capacity(stdout.len());
    let mut in_hunk = false;
    for line in stdout.split_inclusive('\n') {
        let starts_diff = line.starts_with("diff --git ");
        if starts_diff {
            in_hunk = false;
        }
        let header = starts_diff || (!in_hunk && is_pre_hunk_header(line));
        if header && !prefix.is_empty() {
            rewritten.push_str(&line.replace(&a_pat, "a/").replace(&b_pat, "b/"));
        } else {
            rewritten.push_str(line);
        }
        if line.starts_with("@@") {
            in_hunk = true;
        }
    }
    let mut mapped = HashMap::new();
    for patch in split_patches(&rewritten) {
        let header = patch.lines().next()?;
        let rel = rel_from_header(header)?;
        if mapped.insert(rel.to_string(), patch).is_some() {
            return None;
        }
    }
    if mapped.len() == rels.len() && rels.iter().all(|rel| mapped.contains_key(rel)) {
        Some(mapped)
    } else {
        None
    }
}

fn is_pre_hunk_header(line: &str) -> bool {
    let line = line.trim_end_matches(['\n', '\r']);
    line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("dissimilarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("copy from ")
        || line.starts_with("copy to ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("Binary files ")
}

fn find_prefix(stdout: &str, rels: &[String]) -> Option<String> {
    let anchor = rels.iter().max_by_key(|rel| rel.len())?;
    let header = stdout
        .lines()
        .find(|line| line.starts_with("diff --git ") && line.contains(anchor.as_str()))?;
    let trimmed = header.trim_end_matches([' ', '\t', '\r']);
    let b_at = trimmed.rfind(" b/")?;
    let after = &trimmed[b_at + 3..];
    let prefix = after.strip_suffix(anchor.as_str())?;
    if !prefix.is_empty() && !prefix.ends_with('/') {
        return None;
    }
    if prefix.contains('\n') || prefix.contains('\0') {
        return None;
    }
    let a_side = format!("a/{prefix}{anchor}");
    if !trimmed.contains(&a_side) {
        return None;
    }
    Some(prefix.to_string())
}

fn rel_from_header(header: &str) -> Option<&str> {
    let header = header.trim_end_matches([' ', '\t', '\r']);
    let rest = header.strip_prefix("diff --git a/")?;
    let (left, right) = rest.rsplit_once(" b/")?;
    (left == right).then_some(left)
}

fn split_patches(text: &str) -> Vec<String> {
    let mut patches = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") && !current.is_empty() {
            patches.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        patches.push(current);
    }
    patches
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    use crate::loader::{fetch_local_diff, fetch_local_diffs_batched, DataLoadResult};
    use tokio::sync::mpsc;

    fn git_setup(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "octorus-test")
            .env("GIT_AUTHOR_EMAIL", "octorus-test@example.com")
            .env("GIT_COMMITTER_NAME", "octorus-test")
            .env("GIT_COMMITTER_EMAIL", "octorus-test@example.com")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_repo(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git_setup(dir, &["init", "-b", "main"]);
        git_setup(dir, &["config", "user.email", "t@example.com"]);
        git_setup(dir, &["config", "user.name", "t"]);
        git_setup(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "seed\n").unwrap();
        git_setup(dir, &["add", "README.md"]);
        git_setup(dir, &["commit", "-m", "init"]);
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn oracle(dir: &Path, rel: &str) -> Option<String> {
        let output = Command::new("git")
            .args([
                "-c",
                "core.quotePath=false",
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--no-index",
                "--",
                "/dev/null",
                rel,
            ])
            .current_dir(dir)
            .output()
            .unwrap();
        interpret(output)
    }

    fn workdir(dir: &Path) -> Option<String> {
        Some(dir.to_string_lossy().into_owned())
    }

    async fn load_names(dir: &Path) -> Vec<String> {
        let (tx, mut rx) = mpsc::channel(1);
        fetch_local_diff("local".to_string(), workdir(dir), tx).await;
        match rx.recv().await.unwrap() {
            DataLoadResult::Success { files, .. } => {
                files.into_iter().map(|file| file.filename).collect()
            }
            DataLoadResult::Unchanged { .. } => panic!("local diff reported an update check"),
            DataLoadResult::Error(err) => panic!("local diff failed: {err}"),
        }
    }

    #[test]
    fn rewrite_drops_only_the_overlay_prefix() {
        let stdout = "\
diff --git a/TMP/a.txt b/TMP/a.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/TMP/a.txt
@@ -0,0 +1 @@
+see b/TMP/a.txt
diff --git a/TMP/bin.dat b/TMP/bin.dat
new file mode 100644
index 0000000..2222222
Binary files /dev/null and b/TMP/bin.dat differ
";
        let rels = vec!["a.txt".to_string(), "bin.dat".to_string()];
        let mapped = rewrite_overlay(stdout, &rels).unwrap();
        assert_eq!(
            mapped["a.txt"],
            "\
diff --git a/a.txt b/a.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/a.txt
@@ -0,0 +1 @@
+see b/TMP/a.txt
"
        );
        assert_eq!(
            mapped["bin.dat"],
            "\
diff --git a/bin.dat b/bin.dat
new file mode 100644
index 0000000..2222222
Binary files /dev/null and b/bin.dat differ
"
        );
    }

    #[tokio::test]
    async fn patches_match_per_file_git_for_each_shape() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        write(&dir.join("plain.txt"), "hello\n");
        write(&dir.join("nonew.txt"), "no nl");
        write(&dir.join("empty.txt"), "");
        write(&dir.join("crlf.txt"), "line1\r\nline2\r\n");
        let mut binary = b"header\n".to_vec();
        binary.extend_from_slice(&[0, 1, 2, 3, 255]);
        binary.extend(std::iter::repeat_n(b'x', 4096));
        std::fs::write(dir.join("binary.bin"), &binary).unwrap();
        write(&dir.join("sub dir/my file.txt"), "spaced\n");
        write(&dir.join("uni.txt"), "日本語\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            write(&dir.join("exec.sh"), "#!/bin/sh\necho\n");
            let mut perms = std::fs::metadata(dir.join("exec.sh"))
                .unwrap()
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(dir.join("exec.sh"), perms).unwrap();
            std::os::unix::fs::symlink("plain.txt", dir.join("link.txt")).unwrap();
        }

        let mut rels = vec![
            "uni.txt".to_string(),
            "plain.txt".to_string(),
            "sub dir/my file.txt".to_string(),
            "empty.txt".to_string(),
            "binary.bin".to_string(),
            "nonew.txt".to_string(),
            "crlf.txt".to_string(),
        ];
        #[cfg(unix)]
        {
            rels.push("exec.sh".to_string());
            rels.push("link.txt".to_string());
        }
        let wd = workdir(dir);
        let got = patches(wd.as_deref(), &rels).await;
        assert_eq!(got.len(), rels.len());
        for (rel, patch) in rels.iter().zip(&got) {
            assert_eq!(patch, &oracle(dir, rel), "{rel}");
        }
        let again = patches(wd.as_deref(), &rels).await;
        assert_eq!(again, got);
    }

    #[tokio::test]
    async fn attributes_and_prefixes_stay_on_the_per_file_text() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        write(&dir.join(".gitattributes"), "src/*.rs -diff\n");
        git_setup(dir, &["add", ".gitattributes"]);
        git_setup(dir, &["commit", "-m", "attrs"]);
        write(&dir.join("src/new.rs"), "fn kept() {}\n");
        write(&dir.join("src/other.rs"), "fn other() {}\n");
        let rels = vec!["src/new.rs".to_string(), "src/other.rs".to_string()];
        let got = patches(workdir(dir).as_deref(), &rels).await;
        for (rel, patch) in rels.iter().zip(&got) {
            let expected = oracle(dir, rel);
            assert!(
                expected
                    .as_ref()
                    .is_some_and(|text| text.contains("Binary files")),
                "{rel} oracle lost the attribute: {expected:?}"
            );
            assert_eq!(patch, &expected, "{rel}");
        }

        let ignored = tempfile::tempdir().unwrap();
        let ignored = ignored.path();
        init_repo(ignored);
        write(&ignored.join(".gitignore"), ".gitattributes\n");
        git_setup(ignored, &["add", ".gitignore"]);
        git_setup(ignored, &["commit", "-m", "ignore attrs"]);
        write(&ignored.join(".gitattributes"), "* -diff\n");
        write(&ignored.join("one.txt"), "one\n");
        write(&ignored.join("two.txt"), "two\n");
        let rels = vec!["one.txt".to_string(), "two.txt".to_string()];
        let got = patches(workdir(ignored).as_deref(), &rels).await;
        for (rel, patch) in rels.iter().zip(&got) {
            let expected = oracle(ignored, rel);
            assert!(
                expected
                    .as_ref()
                    .is_some_and(|text| text.contains("Binary files")),
                "ignored attributes stopped applying to {rel}: {expected:?}"
            );
            assert_eq!(patch, &expected, "{rel}");
        }

        let prefixed = tempfile::tempdir().unwrap();
        let prefixed = prefixed.path();
        init_repo(prefixed);
        git_repo(prefixed, &["config", "diff.noprefix", "true"]);
        write(&prefixed.join("a.txt"), "aaa\n");
        write(&prefixed.join("b.txt"), "bbb\n");
        let rels = vec!["b.txt".to_string(), "a.txt".to_string()];
        let got = patches(workdir(prefixed).as_deref(), &rels).await;
        for (rel, patch) in rels.iter().zip(&got) {
            assert_eq!(patch, &oracle(prefixed, rel), "{rel}");
        }

        let mnemonic = tempfile::tempdir().unwrap();
        let mnemonic = mnemonic.path();
        init_repo(mnemonic);
        git_repo(mnemonic, &["config", "diff.mnemonicPrefix", "true"]);
        write(&mnemonic.join("a.txt"), "aaa\n");
        write(&mnemonic.join("b.txt"), "bbb\n");
        let got = patches(workdir(mnemonic).as_deref(), &rels).await;
        for (rel, patch) in rels.iter().zip(&got) {
            assert_eq!(patch, &oracle(mnemonic, rel), "{rel}");
        }
    }

    #[tokio::test]
    async fn autocrlf_change_drops_the_cached_patch() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        write(&dir.join("crlf.txt"), "line1\r\nline2\r\n");
        write(&dir.join("plain.txt"), "plain\n");
        let rels = vec!["crlf.txt".to_string(), "plain.txt".to_string()];
        let first = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(first[0], oracle(dir, "crlf.txt"));
        git_repo(dir, &["config", "core.autocrlf", "true"]);
        let second = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(second[0], oracle(dir, "crlf.txt"));
        assert_eq!(second[1], oracle(dir, "plain.txt"));
        assert_ne!(first[0], second[0]);
    }

    #[tokio::test]
    async fn edited_untracked_file_matches_fresh_git() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        write(&dir.join("a.txt"), "one\n");
        write(&dir.join("b.txt"), "two\n");
        let rels = vec!["a.txt".to_string(), "b.txt".to_string()];
        let first = patches(workdir(dir).as_deref(), &rels).await;
        write(&dir.join("a.txt"), "one\nchanged\n");
        let second = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(second[0], oracle(dir, "a.txt"));
        assert_eq!(second[1], oracle(dir, "b.txt"));
        assert_ne!(first[0], second[0]);
        assert_eq!(second[1], first[1]);
    }

    #[tokio::test]
    async fn large_untracked_file_is_not_truncated() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        let mut body = String::new();
        for line in 0..20_000 {
            body.push_str(&format!("line {line} of the untracked file\n"));
        }
        write(&dir.join("big.txt"), &body);
        write(&dir.join("side.txt"), "side\n");
        let rels = vec!["big.txt".to_string(), "side.txt".to_string()];
        let got = patches(workdir(dir).as_deref(), &rels).await;
        let expected = oracle(dir, "big.txt");
        assert_eq!(got[0], expected);
        let patch = got[0].as_ref().unwrap();
        assert!(patch.contains("+line 19999 of the untracked file\n"));
        assert!(patch.len() > body.len());
        assert_eq!(got[1], oracle(dir, "side.txt"));
    }

    #[tokio::test]
    async fn discovery_orders_files_and_skips_ignored_and_nested_content() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        write(&dir.join(".gitignore"), "ign/\n");
        git_setup(dir, &["add", ".gitignore"]);
        git_setup(dir, &["commit", "-m", "ignore"]);
        write(&dir.join("m.txt"), "m\n");
        write(&dir.join("a.txt"), "a\n");
        write(&dir.join("b.txt"), "b\n");
        write(&dir.join("ign/hidden.txt"), "nope\n");
        write(&dir.join("ign/also.txt"), "nope\n");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        git_setup(&dir.join("nested"), &["init", "-b", "main"]);
        write(&dir.join("nested/inside.txt"), "inside\n");

        let names = load_names(dir).await;
        assert_eq!(
            names,
            vec![
                "a.txt".to_string(),
                "b.txt".to_string(),
                "m.txt".to_string(),
                "nested/".to_string(),
            ]
        );
        let nested = patches(workdir(dir).as_deref(), &["nested/".to_string()]).await;
        assert_eq!(nested, vec![None]);

        std::fs::remove_file(dir.join("b.txt")).unwrap();
        let names = load_names(dir).await;
        assert_eq!(
            names,
            vec![
                "a.txt".to_string(),
                "m.txt".to_string(),
                "nested/".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn tracked_batch_and_untracked_chunks_both_load() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        write(&dir.join("README.md"), "seed\nchanged\n");
        write(&dir.join("c.txt"), "c\n");
        write(&dir.join("a.txt"), "a\n");
        write(&dir.join("b.txt"), "b\n");
        let names = load_names(dir).await;
        assert!(names.contains(&"README.md".to_string()));
        assert!(names.contains(&"a.txt".to_string()));

        let (tx, mut rx) = mpsc::channel(8);
        let untracked = vec![
            "c.txt".to_string(),
            "a.txt".to_string(),
            "b.txt".to_string(),
        ];
        fetch_local_diffs_batched(
            workdir(dir),
            vec!["README.md".to_string()],
            untracked.clone(),
            2,
            tx,
        )
        .await;
        let mut got = Vec::new();
        while let Some(batch) = rx.recv().await {
            got.extend(batch);
        }
        assert_eq!(
            got.iter()
                .map(|row| row.filename.as_str())
                .collect::<Vec<_>>(),
            vec!["README.md", "c.txt", "a.txt", "b.txt"]
        );
        assert!(got[0]
            .patch
            .as_ref()
            .is_some_and(|patch| patch.contains("+changed")));
        for (row, rel) in got.iter().skip(1).zip(&untracked) {
            assert_eq!(row.patch, oracle(dir, rel), "{rel}");
        }
    }

    #[tokio::test]
    async fn empty_request_does_not_need_a_repo() {
        let got = patches(None, &[]).await;
        assert!(got.is_empty());
    }

    #[test]
    fn stat_matching_the_store_clock_is_racy() {
        let started = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let at = |mtime: u64, ctime: u64| StatKey {
            len: 3,
            mtime,
            mtime_nsec: 0,
            ctime,
            ctime_nsec: 0,
            mode: 0o100644,
            ino: 1,
            dev: 1,
        };
        assert!(racy(at(1_700_000_000, 1_700_000_000), started));
        assert!(racy(at(1_700_000_000 - 2, 1_700_000_000 - 30), started));
        assert!(racy(at(1_700_000_000 - 30, 1_700_000_000), started));
        assert!(!racy(at(1_700_000_000 - 3, 1_700_000_000 - 3), started));
    }

    #[tokio::test]
    async fn same_size_rewrite_while_mtime_matches_the_store_clock_is_reread() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        write(&dir.join("a.txt"), "v1\n");
        write(&dir.join("b.txt"), "stay\n");
        let rels = vec!["a.txt".to_string(), "b.txt".to_string()];
        let now = std::time::SystemTime::now();
        set_mtime(&dir.join("a.txt"), now);
        set_mtime(&dir.join("b.txt"), now);
        let first = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(first[0], oracle(dir, "a.txt"));
        assert_eq!(
            cached_count(dir),
            0,
            "a stat inside the store tick must not be cached"
        );
        write(&dir.join("a.txt"), "v2\n");
        set_mtime(&dir.join("a.txt"), now);
        let second = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(second[0], oracle(dir, "a.txt"));
        assert!(second[0].as_ref().unwrap().contains("+v2"));
        assert_ne!(first[0], second[0]);

        settle_timestamps().await;
        let warmed = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(warmed[0], second[0]);
        assert!(cached_count(dir) >= 2, "stable files are cached");
        let again = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(again, warmed);
    }

    #[tokio::test]
    async fn worktree_and_submodule_info_attributes_bust_a_warm_cache() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let wt = temp.path().join("wt");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        git_setup(
            &main,
            &["worktree", "add", "--detach", &wt.to_string_lossy()],
        );
        write(&wt.join("a.txt"), "alpha\n");
        write(&wt.join("b.txt"), "beta\n");

        let parent = temp.path().join("parent");
        let origin = temp.path().join("origin");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&origin).unwrap();
        init_repo(&parent);
        init_repo(&origin);
        git_setup(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                &origin.to_string_lossy(),
                "sub",
            ],
        );
        let sub = parent.join("sub");
        write(&sub.join("a.txt"), "alpha\n");
        write(&sub.join("b.txt"), "beta\n");

        settle_timestamps().await;
        let rels = vec!["a.txt".to_string(), "b.txt".to_string()];
        let mut warm = Vec::new();
        for dir in [&wt, &sub] {
            let first = patches(workdir(dir).as_deref(), &rels).await;
            assert_eq!(first[0], oracle(dir, "a.txt"), "{}", dir.display());
            assert!(
                first[0].as_ref().unwrap().contains("+alpha"),
                "{}",
                dir.display()
            );
            assert!(cached_count(dir) >= 2, "{}", dir.display());
            warm.push(first);
        }

        for (dir, first) in [&wt, &sub].into_iter().zip(warm) {
            let info = git_stdout(
                dir,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "info/attributes",
                ],
            );
            let info = PathBuf::from(info.trim());
            if let Some(parent) = info.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&info, "*.txt -diff\n").unwrap();
            let second = patches(workdir(dir).as_deref(), &rels).await;
            let expected = oracle(dir, "a.txt");
            assert!(
                expected
                    .as_ref()
                    .is_some_and(|text| text.contains("Binary files")),
                "{} via {} stayed text: {expected:?}",
                dir.display(),
                info.display()
            );
            assert_eq!(second[0], expected, "{}", dir.display());
            assert_ne!(second[0], first[0], "{}", dir.display());
        }
    }

    fn set_mtime(path: &Path, time: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    async fn settle_timestamps() {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Counts {
        config: usize,
        per_file: usize,
        overlay: usize,
        name_status: usize,
        numstat: usize,
        ls_files: usize,
        rev_parse: usize,
    }

    struct GitLog {
        log: PathBuf,
    }

    impl GitLog {
        fn from_env() -> Self {
            Self {
                log: PathBuf::from(
                    std::env::var_os("OCTORUS_GIT_LOG").expect("count child log path"),
                ),
            }
        }

        fn clear(&self) {
            std::fs::write(&self.log, "").unwrap();
        }

        fn counts(&self, repo: &Path) -> Counts {
            let text = std::fs::read_to_string(&self.log).unwrap_or_default();
            let repo = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
            let mut counts = Counts {
                config: 0,
                per_file: 0,
                overlay: 0,
                name_status: 0,
                numstat: 0,
                ls_files: 0,
                rev_parse: 0,
            };
            for line in text.lines() {
                let Some((cwd, args)) = line.split_once('\t') else {
                    continue;
                };
                let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
                if cwd != repo {
                    continue;
                }
                let tokens: Vec<&str> = args.split_whitespace().collect();
                let has = |token: &str| tokens.contains(&token);
                if has("--no-index") {
                    if has("/dev/null") {
                        counts.per_file += 1;
                    } else {
                        counts.overlay += 1;
                    }
                } else if has("config")
                    || has("var")
                    || has("--git-path")
                    || has("--git-common-dir")
                {
                    counts.config += 1;
                } else if has("--name-status") {
                    counts.name_status += 1;
                } else if has("--numstat") {
                    counts.numstat += 1;
                } else if has("ls-files") {
                    counts.ls_files += 1;
                } else if has("rev-parse") {
                    counts.rev_parse += 1;
                }
            }
            counts
        }

        fn repo_log(&self, repo: &Path) -> String {
            let text = std::fs::read_to_string(&self.log).unwrap_or_default();
            let repo = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
            text.lines()
                .filter(|line| {
                    line.split_once('\t')
                        .and_then(|(cwd, _)| std::fs::canonicalize(cwd).ok())
                        .is_some_and(|cwd| cwd == repo)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    fn run_refresh_count_child() {
        let real = real_git();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("git.log");
        let script = dir.path().join("git");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ -n \"${OCTORUS_GIT_LOG:-}\" ]; then\n  cwd=$(pwd -P)\n  printf '%s\\t%s\\n' \"$cwd\" \"$*\" >> \"$OCTORUS_GIT_LOG\"\nfi\nexec \"$OCTORUS_REAL_GIT\" \"$@\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let mut path = std::ffi::OsString::from(dir.path());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "untracked_diff::tests::refresh_reuses_untracked_patches_without_another_diff",
                "--test-threads=1",
            ])
            .env("OCTORUS_UNTRACKED_COUNT", "1")
            .env("OCTORUS_GIT_LOG", &log)
            .env("OCTORUS_REAL_GIT", &real)
            .env("PATH", &path);
        if let Ok(n) = std::env::var("OCTORUS_MEASURE_N") {
            child.env("OCTORUS_MEASURE_N", n);
        }
        if let Ok(out) = std::env::var("OCTORUS_MEASURE_OUT") {
            child.env("OCTORUS_MEASURE_OUT", out);
        }
        let status = child.status().unwrap();
        assert!(status.success(), "counting child failed");
    }

    fn real_git() -> PathBuf {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("git");
            if candidate.is_file() {
                return candidate;
            }
        }
        panic!("git is not on PATH");
    }

    fn host_attributes(repo: &Path) -> bool {
        let mut paths = vec![
            PathBuf::from("/etc/gitattributes"),
            repo.join(".git/info/attributes"),
            repo.join(".gitattributes"),
        ];
        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(xdg) if !xdg.is_empty() => paths.push(PathBuf::from(xdg).join("git/attributes")),
            _ => {
                if let Some(home) = std::env::var_os("HOME") {
                    paths.push(PathBuf::from(home).join(".config/git/attributes"));
                }
            }
        }
        paths.iter().any(|path| existing_file_stat(path).is_some())
    }

    fn assert_counts(log: &GitLog, repo: &Path, want: Counts) {
        let got = log.counts(repo);
        assert_eq!(got, want, "git log:\n{}", log.repo_log(repo));
    }

    #[tokio::test]
    async fn refresh_reuses_untracked_patches_without_another_diff() {
        // The wrapper sits on PATH. A child process keeps that change away
        // from every other test in this binary.
        if std::env::var_os("OCTORUS_UNTRACKED_COUNT").is_none() {
            run_refresh_count_child();
            return;
        }
        let log = GitLog::from_env();
        if let Ok(raw) = std::env::var("OCTORUS_MEASURE_N") {
            measure_n(&log, raw.parse().expect("measure n")).await;
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        for index in 0..8 {
            write(
                &dir.join(format!("f{index}.txt")),
                &format!("file {index}\n"),
            );
        }
        let rels: Vec<String> = (0..8).map(|index| format!("f{index}.txt")).collect();
        settle_timestamps().await;
        let expected: Vec<_> = rels.iter().map(|rel| oracle(dir, rel)).collect();
        log.clear();
        let got = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(got, expected);
        let cold = log.counts(dir);
        if host_attributes(dir) {
            assert_eq!(cold.overlay, 0, "git log:\n{}", log.repo_log(dir));
            assert_eq!(cold.per_file, rels.len(), "git log:\n{}", log.repo_log(dir));
        } else {
            assert_counts(
                &log,
                dir,
                Counts {
                    config: cold.config,
                    per_file: 0,
                    overlay: 1,
                    name_status: 0,
                    numstat: 0,
                    ls_files: 0,
                    rev_parse: 0,
                },
            );
        }
        assert_eq!(cold.config, 3, "git log:\n{}", log.repo_log(dir));

        log.clear();
        let again = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(again, expected);
        assert_counts(
            &log,
            dir,
            Counts {
                config: 3,
                per_file: 0,
                overlay: 0,
                name_status: 0,
                numstat: 0,
                ls_files: 0,
                rev_parse: 0,
            },
        );

        write(&dir.join("f3.txt"), "file 3\nedited\n");
        log.clear();
        let edited = patches(workdir(dir).as_deref(), &rels).await;
        let edited_counts = log.counts(dir);
        assert_eq!(edited[3], oracle(dir, "f3.txt"));
        assert_eq!(edited[0], expected[0]);
        assert_eq!(
            edited_counts,
            Counts {
                config: 3,
                per_file: 1,
                overlay: 0,
                name_status: 0,
                numstat: 0,
                ls_files: 0,
                rev_parse: 0,
            },
            "git log:\n{}",
            log.repo_log(dir)
        );

        write(&dir.join("f1.txt"), "file 1\nleft\n");
        write(&dir.join("f2.txt"), "file 2\nright\n");
        log.clear();
        let both = patches(workdir(dir).as_deref(), &rels).await;
        let two = log.counts(dir);
        assert_eq!(both[1], oracle(dir, "f1.txt"));
        assert_eq!(both[2], oracle(dir, "f2.txt"));
        if host_attributes(dir) {
            assert_eq!(two.per_file, 2, "git log:\n{}", log.repo_log(dir));
            assert_eq!(two.overlay, 0, "git log:\n{}", log.repo_log(dir));
        } else {
            assert_eq!(two.per_file, 0, "git log:\n{}", log.repo_log(dir));
            assert_eq!(two.overlay, 1, "git log:\n{}", log.repo_log(dir));
        }

        // The two edits are inside the racy tick, so let them age before the
        // no-change refresh. Otherwise those files are re-diffed on purpose.
        settle_timestamps().await;
        let warmed = patches(workdir(dir).as_deref(), &rels).await;
        assert_eq!(warmed, both);
        log.clear();
        let _ = load_names(dir).await;
        require_fresh(workdir(dir).as_deref(), &rels[0]);
        let dir_wd = workdir(dir);
        let (refreshed, selected) = tokio::join!(
            patches(dir_wd.as_deref(), &rels),
            patch(dir_wd.as_deref(), &rels[0]),
        );
        assert_eq!(refreshed, both);
        assert_eq!(selected, both[0]);
        let refresh = log.counts(dir);
        assert_eq!(refresh.per_file, 1, "git log:\n{}", log.repo_log(dir));
        assert_eq!(refresh.overlay, 0, "git log:\n{}", log.repo_log(dir));
        assert_eq!(refresh.name_status, 1, "git log:\n{}", log.repo_log(dir));
        assert_eq!(refresh.numstat, 1, "git log:\n{}", log.repo_log(dir));
        assert_eq!(refresh.ls_files, 1, "git log:\n{}", log.repo_log(dir));
        assert_eq!(refresh.rev_parse, 1, "git log:\n{}", log.repo_log(dir));

        let single = tempfile::tempdir().unwrap();
        let single = single.path();
        init_repo(single);
        write(&single.join("only.txt"), "only\n");
        log.clear();
        let single_dir = workdir(single);
        let (left, right) = tokio::join!(
            patch(single_dir.as_deref(), "only.txt"),
            patch(single_dir.as_deref(), "only.txt"),
        );
        let once = log.counts(single);
        assert_eq!(left, oracle(single, "only.txt"));
        assert_eq!(right, left);
        assert_eq!(once.per_file, 1, "git log:\n{}", log.repo_log(single));
        assert_eq!(once.overlay, 0, "git log:\n{}", log.repo_log(single));

        let opened = tempfile::tempdir().unwrap();
        let opened = opened.path();
        init_repo(opened);
        for index in 0..8 {
            write(
                &opened.join(format!("u{index}.txt")),
                &format!("u {index}\n"),
            );
        }
        log.clear();
        let names = load_names(opened).await;
        let untracked: Vec<String> = names
            .into_iter()
            .filter(|name| name.ends_with(".txt"))
            .collect();
        assert_eq!(untracked.len(), 8, "{untracked:?}");
        let opened_dir = workdir(opened);
        let (selected, batch) = tokio::join!(
            patch(opened_dir.as_deref(), &untracked[0]),
            patches(opened_dir.as_deref(), &untracked),
        );
        let open_counts = log.counts(opened);
        assert_eq!(selected, oracle(opened, &untracked[0]));
        for (rel, patch) in untracked.iter().zip(&batch) {
            assert_eq!(patch, &oracle(opened, rel), "{rel}");
        }
        assert_eq!(open_counts.name_status, 1, "{}", log.repo_log(opened));
        assert_eq!(open_counts.numstat, 1, "{}", log.repo_log(opened));
        assert_eq!(open_counts.ls_files, 1, "{}", log.repo_log(opened));
        assert_eq!(open_counts.rev_parse, 1, "{}", log.repo_log(opened));
        if host_attributes(opened) {
            assert_eq!(open_counts.overlay, 0, "{}", log.repo_log(opened));
            assert!(
                open_counts.per_file == 8 || open_counts.per_file == 7,
                "{}",
                log.repo_log(opened)
            );
        } else {
            assert_eq!(open_counts.overlay, 1, "{}", log.repo_log(opened));
            assert!(open_counts.per_file <= 1, "{}", log.repo_log(opened));
        }
    }

    async fn measure_n(log: &GitLog, n: usize) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        init_repo(dir);
        for index in 0..n {
            write(
                &dir.join(format!("u{index}.txt")),
                &format!("line {index}\n"),
            );
        }
        let rels: Vec<String> = (0..n).map(|index| format!("u{index}.txt")).collect();
        if n > 0 {
            // Steady state: the files are older than the racy tick, which is
            // what a later watcher refresh sees.
            settle_timestamps().await;
        }
        let wd = workdir(dir);
        log.clear();
        let started = std::time::Instant::now();
        let _ = load_names(dir).await;
        if n == 0 {
            let _ = patches(wd.as_deref(), &rels).await;
        } else {
            let _ = tokio::join!(
                patch(wd.as_deref(), &rels[0]),
                patches(wd.as_deref(), &rels),
            );
        }
        let open_ms = started.elapsed().as_millis();
        let open = log.counts(dir);
        log.clear();
        let started = std::time::Instant::now();
        let _ = load_names(dir).await;
        if n > 0 {
            require_fresh(wd.as_deref(), &rels[0]);
            let _ = tokio::join!(
                patches(wd.as_deref(), &rels),
                patch(wd.as_deref(), &rels[0]),
            );
        } else {
            let _ = patches(wd.as_deref(), &rels).await;
        }
        let refresh_ms = started.elapsed().as_millis();
        let refresh = log.counts(dir);
        let line = format!("n={n} open={open:?} {open_ms}ms refresh={refresh:?} {refresh_ms}ms\n");
        if let Ok(path) = std::env::var("OCTORUS_MEASURE_OUT") {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            file.write_all(line.as_bytes()).unwrap();
        }
    }
}
