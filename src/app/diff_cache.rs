use tokio::sync::mpsc;

use crate::github::{ChangedFile, PullRequest};
use crate::syntax::ParserPool;

use super::types::*;
use super::{App, DataState, MAX_HIGHLIGHTED_CACHE_ENTRIES};

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

    #[allow(dead_code)]
    pub fn is_data_available(&self) -> bool {
        matches!(self.data_state, DataState::Loaded { .. })
    }
    pub(crate) fn update_diff_line_count(&mut self) {
        self.diff_line_count = Self::calc_diff_line_count(self.files(), self.selected_file);
    }

    /// Split Viewでファイル選択変更時にdiff状態を同期
    pub(crate) fn sync_diff_to_selected_file(&mut self) {
        self.selected_line = 0;
        self.scroll_offset = 0;
        self.multiline_selection = None;
        self.comment_panel_open = false;
        self.comment_panel_scroll = 0;
        self.clear_pending_keys();
        self.symbol_popup = None;
        self.update_diff_line_count();
        if !self.local_mode && self.review_comments.is_none() {
            self.load_review_comments();
        }
        self.update_file_comment_positions();
        self.request_lazy_diff();
        self.ensure_diff_cache();
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
        if let Some(ref cache) = self.diff_cache {
            let md_ok = !is_md || cache.markdown_rich == markdown_rich;
            if cache.file_index == file_index && md_ok {
                let Some(file) = self.files().get(file_index) else {
                    self.diff_cache = None;
                    return;
                };
                let Some(ref patch) = file.patch else {
                    self.diff_cache = None;
                    return;
                };
                let current_hash = hash_string(patch);
                if cache.patch_hash == current_hash {
                    return; // キャッシュ有効
                }
            }
        }

        // 古い receiver をドロップ（競合防止）
        self.diff_cache_receiver = None;

        // 現在のハイライト済みキャッシュをストアに退避（上限チェック付き）
        if let Some(cache) = self.diff_cache.take() {
            if cache.highlighted
                && self.highlighted_cache_store.len() < MAX_HIGHLIGHTED_CACHE_ENTRIES
            {
                self.highlighted_cache_store.insert(cache.file_index, cache);
            }
        }

        let Some(file) = self.files().get(file_index) else {
            self.diff_cache = None;
            return;
        };
        let Some(patch) = file.patch.clone() else {
            self.diff_cache = None;
            return;
        };
        let filename = file.filename.clone();

        // 2. ストアにハイライト済みキャッシュがあるか確認
        //    md_ok: 非markdownファイルでは markdown_rich の不一致を無視する（上記コメント参照）
        if let Some(cached) = self.highlighted_cache_store.remove(&file_index) {
            let current_hash = hash_string(&patch);
            let md_ok = !is_md || cached.markdown_rich == markdown_rich;
            if cached.patch_hash == current_hash && md_ok {
                self.diff_cache = Some(cached);
                return; // ストアから復元、バックグラウンド構築不要
            }
            // 無効なキャッシュは破棄
        }

        // 3. キャッシュミス: プレーンキャッシュを即座に構築（~1ms）
        let tab_width = self.config.diff.tab_width;
        let mut plain_cache = crate::ui::diff_view::build_plain_diff_cache(&patch, tab_width);
        plain_cache.file_index = file_index;
        self.diff_cache = Some(plain_cache);

        // 完全版キャッシュをバックグラウンドで構築
        let (tx, rx) = mpsc::channel(1);
        self.diff_cache_receiver = Some(rx);

        let theme = self.config.diff.theme.clone();

        tokio::task::spawn_blocking(move || {
            let mut parser_pool = ParserPool::new();
            let mut cache = crate::ui::diff_view::build_diff_cache(
                &patch,
                &filename,
                &theme,
                &mut parser_pool,
                markdown_rich,
                tab_width,
            );
            cache.file_index = file_index;
            let _ = tx.try_send(cache);
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
        assert_eq!(
            patch,
            "@@ -1,3 +1,3 @@\n +added\n -removed\n  normal\n"
        );
    }

    #[test]
    fn test_empty_lines_in_body() {
        let patch = build_pr_description_patch("Hello\n\nWorld");
        assert_eq!(patch, "@@ -1,3 +1,3 @@\n Hello\n \n World\n");
    }
}
