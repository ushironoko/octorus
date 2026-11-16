use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::diff::{get_line_info, PatchLookup};
use crate::github::{ChangedFile, PullRequest};
use crate::syntax::ParserPool;

use super::types::*;
use super::{App, DataState};

/// The patch index last built for one file.
///
/// `files_id`, `patch_ptr`, and `patch_len` name allocations. They are
/// compared, never dereferenced. `generation` moves when the file list is
/// edited in place, so a recycled patch buffer cannot pass the pointer check.
pub(crate) struct CachedPatchLookup {
    file_index: usize,
    filename: String,
    files_id: usize,
    patch_hash: u64,
    patch_ptr: usize,
    patch_len: usize,
    generation: u64,
    pub(crate) lookup: PatchLookup,
}

enum LookupDecision {
    Retarget {
        files_id: usize,
        ptr: usize,
        len: usize,
    },
    Rebuild {
        filename: String,
        files_id: usize,
        hash: u64,
        ptr: usize,
        len: usize,
        lookup: Box<PatchLookup>,
    },
}

/// Owned suggestion range taken from the cached index.
pub(crate) struct CachedSuggestion {
    pub end_line: u32,
    pub start_line: u32,
    pub diff_position: u32,
    pub original_code: String,
}

impl App {
    pub(crate) fn calc_diff_line_count(files: &[ChangedFile], selected: usize) -> usize {
        files
            .get(selected)
            .and_then(|f| f.patch.as_ref())
            .map(|p| p.lines().count())
            .unwrap_or(0)
    }

    pub fn files(&self) -> &[ChangedFile] {
        match &self.data_state {
            DataState::Loaded { files, .. } => files,
            _ => &[],
        }
    }

    pub fn pr(&self) -> Option<&PullRequest> {
        match &self.data_state {
            DataState::Loaded { pr, .. } => Some(pr.as_ref()),
            _ => None,
        }
    }

    pub(crate) fn update_diff_line_count(&mut self) {
        let count = Self::calc_diff_line_count(self.files(), self.selected_file);
        self.diff_scroll.set_line_count(count);
    }

    /// Split Viewでファイル選択変更時にdiff状態を同期
    pub(crate) fn sync_diff_to_selected_file(&mut self) {
        self.diff_scroll.reset();
        self.multiline_selection = None;
        self.cmt.comment_panel_open = false;
        self.cmt.comment_panel_scroll = 0;
        self.clear_pending_keys();
        self.symbol_popup = None;
        self.update_diff_line_count();
        if self.cmt.review_comments.is_none() {
            self.load_review_comments();
        }
        self.update_file_comment_positions();
        self.request_lazy_diff();
        self.ensure_diff_cache();
    }
    #[cfg(test)]
    pub(crate) fn patch_lookup_builds(&self) -> u64 {
        self.patch_lookup_builds
    }

    pub(crate) fn bump_files_generation(&mut self) {
        self.files_generation = self.files_generation.wrapping_add(1);
    }

    /// Build the patch index for the selected file, or reuse it.
    ///
    /// The plain diff is drawn without this. Callers are line lookup and
    /// comment placement, so the first paint of a file does not wait on it.
    /// A hit compares the file-list allocation and the patch buffer and does
    /// not hash or scan. The same text in a new buffer (highlight keep,
    /// shared-file `make_mut`, a watcher refresh of an unchanged file)
    /// retargets the existing index. Different text rebuilds it.
    pub(crate) fn ensure_patch_lookup(&mut self) -> bool {
        let decision = {
            let DataState::Loaded { files, .. } = &self.data_state else {
                self.cached_patch_lookup = None;
                return false;
            };
            let files_id = files.allocation_id();
            let Some(file) = files.get(self.selected_file) else {
                self.cached_patch_lookup = None;
                return false;
            };
            let Some(patch) = file.patch.as_deref() else {
                return false;
            };
            let ptr = patch.as_ptr() as usize;
            let len = patch.len();
            let file_index = self.selected_file;
            let generation = self.files_generation;
            let hit = self.cached_patch_lookup.as_ref().is_some_and(|cached| {
                cached.generation == generation
                    && cached.files_id == files_id
                    && cached.file_index == file_index
                    && cached.filename == file.filename
                    && cached.patch_ptr == ptr
                    && cached.patch_len == len
            });
            if hit {
                return true;
            }
            let hash = hash_string(patch);
            if self.cached_patch_lookup.as_ref().is_some_and(|cached| {
                cached.filename == file.filename
                    && cached.patch_hash == hash
                    && !cached.lookup.is_fallback()
            }) {
                LookupDecision::Retarget { files_id, ptr, len }
            } else {
                let lookup = Box::new(PatchLookup::build(patch));
                LookupDecision::Rebuild {
                    filename: file.filename.clone(),
                    files_id,
                    hash,
                    ptr,
                    len,
                    lookup,
                }
            }
        };
        match decision {
            LookupDecision::Retarget { files_id, ptr, len } => {
                if let Some(cached) = self.cached_patch_lookup.as_mut() {
                    cached.file_index = self.selected_file;
                    cached.files_id = files_id;
                    cached.patch_ptr = ptr;
                    cached.patch_len = len;
                    cached.generation = self.files_generation;
                }
                true
            }
            LookupDecision::Rebuild {
                filename,
                files_id,
                hash,
                ptr,
                len,
                lookup,
            } => {
                self.patch_lookup_builds = self.patch_lookup_builds.saturating_add(1);
                let lookup = *lookup;
                self.cached_patch_lookup = Some(CachedPatchLookup {
                    file_index: self.selected_file,
                    filename,
                    files_id,
                    patch_hash: hash,
                    patch_ptr: ptr,
                    patch_len: len,
                    generation: self.files_generation,
                    lookup,
                });
                true
            }
        }
    }

    fn selected_patch_and_lookup(&self) -> Option<(&str, &PatchLookup)> {
        let DataState::Loaded { files, .. } = &self.data_state else {
            return None;
        };
        let patch = files.get(self.selected_file)?.patch.as_deref()?;
        let lookup = &self.cached_patch_lookup.as_ref()?.lookup;
        Some((patch, lookup))
    }

    /// [`crate::diff::get_line_info`] for the selected file, from the cached index.
    pub(crate) fn cached_line_info(
        &mut self,
        line_index: usize,
    ) -> Option<crate::diff::DiffLineInfo> {
        if !self.ensure_patch_lookup() {
            return None;
        }
        let (patch, lookup) = self.selected_patch_and_lookup()?;
        if lookup.is_fallback() {
            get_line_info(patch, line_index)
        } else {
            lookup.line_info(patch, line_index)
        }
    }

    /// [`crate::diff::review_comment_span`] for the selected file, from the cached index.
    pub(crate) fn cached_review_span(
        &mut self,
        start: usize,
        end: usize,
    ) -> Option<crate::diff::ReviewCommentSpan> {
        if !self.ensure_patch_lookup() {
            return None;
        }
        let (patch, lookup) = self.selected_patch_and_lookup()?;
        if lookup.is_fallback() {
            crate::diff::review_comment_span(patch, start, end)
        } else {
            lookup.review_comment_span(start, end)
        }
    }

    /// Suggestion range for the selected file, from the cached index.
    ///
    /// `None` when the range is not a new-file run. The checks match
    /// [`crate::diff::validate_multiline_range`] followed by the line reads
    /// the multiline suggestion composer used to take from [`crate::diff::PatchIndex`].
    pub(crate) fn cached_suggestion(
        &mut self,
        start: usize,
        end: usize,
    ) -> Option<CachedSuggestion> {
        if !self.ensure_patch_lookup() {
            return None;
        }
        let (patch, lookup) = self.selected_patch_and_lookup()?;
        let row_info = |row: usize| {
            if lookup.is_fallback() {
                get_line_info(patch, row)
            } else {
                lookup.line_info(patch, row)
            }
        };
        let valid = if lookup.is_fallback() {
            crate::diff::validate_multiline_range(patch, start, end)
        } else {
            lookup.is_suggestion_range(start, end)
        };
        if !valid {
            return None;
        }
        let end_info = row_info(end)?;
        if !matches!(
            end_info.line_type,
            crate::diff::LineType::Added | crate::diff::LineType::Context
        ) {
            return None;
        }
        let end_line = end_info.new_line_number?;
        let diff_position = end_info.diff_position?;
        let start_line = row_info(start)?.new_line_number?;
        let mut original_lines = Vec::new();
        for row in start..=end {
            if let Some(info) = row_info(row) {
                if matches!(
                    info.line_type,
                    crate::diff::LineType::Added | crate::diff::LineType::Context
                ) {
                    original_lines.push(info.line_content);
                }
            }
        }
        Some(CachedSuggestion {
            end_line,
            start_line,
            diff_position,
            original_code: original_lines.join("\n"),
        })
    }

    pub fn ensure_diff_cache(&mut self) {
        let file_index = self.selected_file;
        let markdown_rich = self.markdown_rich;
        // IMPORTANT: markdown_rich フラグはmarkdownファイルのハイライト結果にのみ影響する。
        // 非markdownファイルでは build_diff_cache() の出力が同一になるため、フラグの
        // 不一致を理由にキャッシュを破棄してはならない。破棄すると、PR description view
        // 等でフラグを切り替えた際に全ファイルのプリフェッチ済みキャッシュが無駄に失われ、
        // ファイル表示時にハイライトなしの状態が一瞬見えるデグレが発生する。
        let is_md = self
            .files()
            .get(file_index)
            .map(|f| crate::language::is_markdown_ext_from_filename(&f.filename))
            .unwrap_or(false);

        // 1. 現在の diff_cache が有効か確認（O(1)）
        if self.diff_store.current_key() == Some(&file_index) {
            if let Some(ref cache) = self.diff_store.current {
                let md_ok = !is_md || cache.markdown_rich == markdown_rich;
                if md_ok {
                    let Some(file) = self.files().get(file_index) else {
                        self.diff_store.clear_current();
                        return;
                    };
                    if let Some(ref patch) = file.patch {
                        let current_hash = hash_string(patch);
                        if cache.patch_hash == current_hash
                            && (cache.highlighted || self.diff_store.highlight_in_flight())
                        {
                            return; // ハイライト済み、または構築中
                        }
                    }
                }
            }
        }

        let Some(file) = self.files().get(file_index) else {
            self.diff_store.clear_current();
            return;
        };
        let Some(patch) = file.patch.clone() else {
            // Phase 1 has no patch yet. A parked highlight stays in the store
            // until the bytes arrive; showing it here would be the previous file.
            self.diff_store.clear_current();
            return;
        };
        let filename = file.filename.clone();
        let current_hash = hash_string(&patch);
        let tab_width = self.config.diff.tab_width;

        // 2. ストアにハイライト済みキャッシュがあるか確認
        //    try_restore は patch_hash のみチェックするため、markdown_rich は別途確認
        if self.diff_store.try_restore(&file_index, Some(current_hash)) {
            // markdown_rich の一致チェック（非markdownファイルでは常に ok）
            let md_ok = !is_md
                || self
                    .diff_store
                    .current
                    .as_ref()
                    .map(|c| c.markdown_rich == markdown_rich)
                    .unwrap_or(false);
            if md_ok {
                let highlighted = self
                    .diff_store
                    .current
                    .as_ref()
                    .is_some_and(|cache| cache.highlighted);
                if highlighted {
                    return; // ストアから復元、バックグラウンド構築不要
                }
                // Prefetch published the plain diff. Keep it on screen and still
                // start the uncapped current-file highlight so it can swap in.
                self.spawn_current_file_highlight(
                    file_index,
                    patch,
                    filename,
                    markdown_rich,
                    tab_width,
                );
                return;
            }
            // markdown_rich 不一致: current を破棄して再構築に進む
            self.diff_store.clear_current();
        }

        // 3. キャッシュミス: プレーンキャッシュを即座に構築（~1ms）
        let mut plain_cache = crate::ui::diff_view::build_plain_diff_cache(&patch, tab_width);
        plain_cache.file_index = file_index;
        plain_cache.markdown_rich = markdown_rich;
        self.diff_store.set_current(file_index, plain_cache);
        self.spawn_current_file_highlight(file_index, patch, filename, markdown_rich, tab_width);
    }

    /// Keep highlighted diffs whose patch still matches the new file list.
    ///
    /// Store keys are file indices, so a reorder, insert, or delete re-keys
    /// each surviving cache by filename. A cache is shown only when that
    /// filename is unique and the patch hash (plus `markdown_rich` for
    /// markdown) matches. A phase-1 file has no patch yet: the previous
    /// highlight is parked in the store and not drawn. The patch load installs
    /// it when the hash matches, and drops it on a mismatch or a missing patch.
    pub(crate) fn reconcile_diff_caches(
        &mut self,
        old_files: &[ChangedFile],
        new_files: &[ChangedFile],
    ) {
        self.diff_store.drop_prefetch_rx();
        let markdown_rich = self.markdown_rich;
        let tab_width = self.config.diff.tab_width;

        let old_counts = name_counts(old_files);
        let new_counts = name_counts(new_files);
        let mut old_by_name: HashMap<&str, &ChangedFile> = HashMap::new();
        for file in old_files {
            if old_counts.get(file.filename.as_str()).copied() == Some(1) {
                old_by_name.insert(file.filename.as_str(), file);
            }
        }
        let mut new_index: HashMap<&str, usize> = HashMap::new();
        for (idx, file) in new_files.iter().enumerate() {
            if new_counts.get(file.filename.as_str()).copied() == Some(1) {
                new_index.insert(file.filename.as_str(), idx);
            }
        }

        let name_for = |idx: usize| -> Option<&str> {
            let name = old_files.get(idx)?.filename.as_str();
            let old_unique = old_counts.get(name).copied() == Some(1);
            let new_ok = new_counts.get(name).copied().unwrap_or(0) <= 1;
            (old_unique && new_ok).then_some(name)
        };

        let previous_key = self.diff_store.current_key().copied();
        let previous_name = previous_key.and_then(name_for).map(str::to_string);

        let mut by_name: HashMap<String, DiffCache> = HashMap::new();
        for (key, cache) in self.diff_store.drain_store() {
            if let Some(name) = name_for(key) {
                by_name.entry(name.to_string()).or_insert(cache);
            }
        }
        if let Some((key, cache)) = self.diff_store.take_current() {
            if let Some(name) = name_for(key) {
                by_name.insert(name.to_string(), cache);
            }
        }

        let viewed_name = new_files
            .get(self.selected_file)
            .map(|file| file.filename.as_str())
            .filter(|name| new_counts.get(*name).copied() == Some(1))
            .map(str::to_string);

        let mut placed_current = false;
        for (name, mut cache) in by_name {
            let Some(&idx) = new_index.get(name.as_str()) else {
                continue;
            };
            let (patch, filename, confirmed, park) = {
                let file = &new_files[idx];
                let old = old_by_name.get(name.as_str()).copied();
                let confirmed = cache_matches_refresh(&cache, file, markdown_rich);
                let park = !confirmed && unconfirmed_cache_parkable(&cache, file, old);
                (file.patch.clone(), file.filename.clone(), confirmed, park)
            };
            if !confirmed && !park {
                continue;
            }
            cache.file_index = idx;
            if park {
                // Not drawn. `ensure_diff_cache` installs it once the patch
                // hash matches, with no plain-text flash.
                self.diff_store.insert_kept(idx, cache);
                continue;
            }
            if viewed_name.as_deref() == Some(name.as_str()) {
                let same_job =
                    previous_name.as_deref() == Some(name.as_str()) && previous_key == Some(idx);
                let needs_respawn = !same_job && !cache.highlighted && patch.is_some();
                self.diff_store.install_current(idx, cache, same_job);
                placed_current = true;
                if needs_respawn {
                    if let Some(patch) = patch {
                        self.spawn_current_file_highlight(
                            idx,
                            patch,
                            filename,
                            markdown_rich,
                            tab_width,
                        );
                    }
                }
            } else {
                self.diff_store.insert_kept(idx, cache);
            }
        }
        if !placed_current {
            // The viewed file changed, or its patch no longer matches. Drop any
            // highlight started for the previous file (#270).
            self.diff_store.cancel_highlight_job();
        }
    }

    /// Drop a cache that does not match `patch` for `file_index`.
    pub(crate) fn invalidate_cached_patch(&mut self, file_index: usize, patch: &str) {
        let Some(filename) = self
            .files()
            .get(file_index)
            .map(|file| file.filename.clone())
        else {
            return;
        };
        let markdown_rich = self.markdown_rich;
        let hash = hash_string(patch);
        let matches = |cache: &DiffCache| {
            let is_md = crate::language::is_markdown_ext_from_filename(&filename);
            cache.patch_hash == hash && (!is_md || cache.markdown_rich == markdown_rich)
        };
        if self.diff_store.current_key() == Some(&file_index) {
            if !self.diff_store.current.as_ref().is_some_and(matches) {
                self.diff_store.clear_current();
            }
        } else if self
            .diff_store
            .store_get(&file_index)
            .is_some_and(|cache| !matches(cache))
        {
            self.diff_store.remove_store(&file_index);
        }
    }

    /// Highlight the file on screen. No size cap and no prefetch time budget.
    fn spawn_current_file_highlight(
        &mut self,
        file_index: usize,
        patch: String,
        filename: String,
        markdown_rich: bool,
        tab_width: u8,
    ) {
        let (tx, rx) = mpsc::channel(1);
        let cancel = self.diff_store.begin_highlight(rx);
        let theme = self.config.diff.theme.clone();

        tokio::task::spawn_blocking(move || {
            let mut parser_pool = ParserPool::new();
            let mut cache = crate::ui::diff_view::build_diff_cache_cancellable(
                &patch,
                &filename,
                &theme,
                &mut parser_pool,
                markdown_rich,
                tab_width,
                &cancel,
            );
            if cancel.is_cancelled() {
                return;
            }
            cache.file_index = file_index;
            let _ = tx.try_send((file_index, cache));
        });
    }

    /// PR description 画面を開く
    pub(crate) fn open_pr_description(&mut self) {
        self.previous_state = self.state;
        self.state = AppState::PrDescription;
        self.pr_description_scroll_offset = 0;
        self.rebuild_pr_description_cache();
    }

    /// PR description のキャッシュを再構築する（スクロール位置・状態遷移は変更しない）
    ///
    /// open_pr_description() から分離されている理由: markdown_rich トグル時にスクロール位置を
    /// 維持したままキャッシュのみ再構築する必要があるため。
    pub(crate) fn rebuild_pr_description_cache(&mut self) {
        let body = self
            .pr()
            .and_then(|pr| pr.body.as_deref())
            .unwrap_or("")
            .to_string();

        // キャッシュの再利用判定: body_hash + markdown_rich
        let body_hash = hash_string(&body);
        let markdown_rich = self.markdown_rich;
        if let Some(ref cache) = self.pr_description_cache {
            if cache.patch_hash == body_hash && cache.markdown_rich == markdown_rich {
                return; // キャッシュ有効
            }
        }

        if body.is_empty() {
            self.pr_description_cache = None;
            return;
        }

        let patch = build_pr_description_patch(&body);
        let tab_width = self.config.diff.tab_width;
        let theme = self.config.diff.theme.clone();

        let mut parser_pool = ParserPool::new();
        let mut cache = crate::ui::diff_view::build_diff_cache(
            &patch,
            "description.md",
            &theme,
            &mut parser_pool,
            markdown_rich,
            tab_width,
        );
        cache.file_index = usize::MAX; // sentinel value
        cache.patch_hash = body_hash;
        self.pr_description_cache = Some(cache);
    }
}

fn name_counts(files: &[ChangedFile]) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for file in files {
        *counts.entry(file.filename.as_str()).or_default() += 1;
    }
    counts
}

fn cache_matches_refresh(cache: &DiffCache, new_file: &ChangedFile, markdown_rich: bool) -> bool {
    let Some(patch) = new_file.patch.as_deref() else {
        return false;
    };
    let is_md = crate::language::is_markdown_ext_from_filename(&new_file.filename);
    if is_md && cache.markdown_rich != markdown_rich {
        return false;
    }
    cache.patch_hash == hash_string(patch)
}

/// Phase 1 may keep the previous highlight off-screen when the file's
/// status and numstat are unchanged, so a later identical patch can be
/// installed without rebuilding it. A known structural change is dropped.
fn unconfirmed_cache_parkable(
    cache: &DiffCache,
    new_file: &ChangedFile,
    old_file: Option<&ChangedFile>,
) -> bool {
    if new_file.patch.is_some() {
        return false;
    }
    let Some(old) = old_file else {
        return false;
    };
    old.status == new_file.status
        && old.additions == new_file.additions
        && old.deletions == new_file.deletions
        && old
            .patch
            .as_deref()
            .is_some_and(|patch| hash_string(patch) == cache.patch_hash)
}

/// PR body を全行 context 行の疑似 patch に変換する
pub fn build_pr_description_patch(body: &str) -> String {
    let body = body.replace("\r\n", "\n");
    let line_count = body.lines().count().max(1);
    let mut patch = format!("@@ -1,{} +1,{} @@\n", line_count, line_count);
    for line in body.lines() {
        patch.push(' ');
        patch.push_str(line);
        patch.push('\n');
    }
    patch
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    #[test]
    fn test_basic_body() {
        let patch = build_pr_description_patch("Hello\nWorld");
        assert_eq!(patch, "@@ -1,2 +1,2 @@\n Hello\n World\n");
    }

    #[test]
    fn test_single_line() {
        let patch = build_pr_description_patch("Single line");
        assert_eq!(patch, "@@ -1,1 +1,1 @@\n Single line\n");
    }

    #[test]
    fn test_empty_body() {
        let patch = build_pr_description_patch("");
        // empty body produces no content lines, line_count.max(1) = 1
        assert_eq!(patch, "@@ -1,1 +1,1 @@\n");
    }

    #[test]
    fn test_crlf_conversion() {
        let patch = build_pr_description_patch("Line1\r\nLine2\r\nLine3");
        assert_eq!(patch, "@@ -1,3 +1,3 @@\n Line1\n Line2\n Line3\n");
    }

    #[test]
    fn test_lines_starting_with_plus_minus() {
        let patch = build_pr_description_patch("+added\n-removed\n normal");
        assert_eq!(patch, "@@ -1,3 +1,3 @@\n +added\n -removed\n  normal\n");
    }

    #[test]
    fn test_empty_lines_in_body() {
        let patch = build_pr_description_patch("Hello\n\nWorld");
        assert_eq!(patch, "@@ -1,3 +1,3 @@\n Hello\n \n World\n");
    }
}
