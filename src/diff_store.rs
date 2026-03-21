use std::collections::HashMap;
use std::hash::Hash;
use tokio::sync::mpsc;

use crate::app::DiffCache;
use crate::syntax::ParserPool;
use crate::ui::diff_view::build_diff_cache;

/// ハイライトキャッシュストアの最大エントリ数（メモリ上限）
///
/// 大規模PRでのOOM防止。超過時は現在選択中のファイルから最も遠いエントリを削除。
pub const MAX_STORE_ENTRIES: usize = 50;

/// プリフェッチ対象ファイルの最大数
pub const MAX_PREFETCH_FILES: usize = 50;

// ========================================
// DiffScrollState
// ========================================

/// スクロールモード
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollMode {
    /// margin = visible_lines / 2。App, GitOps 用。
    /// カーソルがビューポート中央を超えるとスクロールが開始。
    Margin,
    /// viewport 端でスクロール。GitLog 用。
    /// カーソルがビューポートの最上/最下に到達したときのみスクロール。
    Edge,
}

/// Diff 表示のスクロール状態
pub struct DiffScrollState {
    pub selected_line: usize,
    pub scroll_offset: usize,
    pub line_count: usize,
    mode: ScrollMode,
}

impl DiffScrollState {
    pub fn new(mode: ScrollMode) -> Self {
        Self {
            selected_line: 0,
            scroll_offset: 0,
            line_count: 0,
            mode,
        }
    }

    /// 全状態をリセット
    pub fn reset(&mut self) {
        self.selected_line = 0;
        self.scroll_offset = 0;
    }

    /// 行数を設定し、selected_line / scroll_offset をクランプ
    pub fn set_line_count(&mut self, count: usize) {
        self.line_count = count;
        if count == 0 {
            self.selected_line = 0;
            self.scroll_offset = 0;
        } else {
            let max = count - 1;
            if self.selected_line > max {
                self.selected_line = max;
            }
            if self.scroll_offset > max {
                self.scroll_offset = max;
            }
        }
    }

    /// スクロールを調整（常にビューポートサイズを引数で受け取る）
    pub fn adjust_scroll(&mut self, visible_lines: usize) {
        match self.mode {
            ScrollMode::Margin => self.adjust_scroll_margin(visible_lines),
            ScrollMode::Edge => self.adjust_scroll_edge(visible_lines),
        }
    }

    /// Margin モード: カーソルがビューポート中央付近を通過するとスクロール
    fn adjust_scroll_margin(&mut self, visible_lines: usize) {
        if visible_lines == 0 {
            return;
        }
        if self.line_count <= visible_lines {
            self.scroll_offset = 0;
            return;
        }

        let margin = visible_lines / 2;

        // カーソルが上マージンより上
        if self.selected_line < self.scroll_offset + margin {
            self.scroll_offset = self.selected_line.saturating_sub(margin);
        }
        // カーソルが下マージンより下
        if self.selected_line + margin >= self.scroll_offset + visible_lines {
            self.scroll_offset = self
                .selected_line
                .saturating_sub(visible_lines.saturating_sub(margin + 1));
        }
    }

    /// Edge モード: カーソルがビューポート端に到達した場合のみスクロール
    fn adjust_scroll_edge(&mut self, visible_lines: usize) {
        if visible_lines == 0 {
            return;
        }
        if self.selected_line < self.scroll_offset {
            self.scroll_offset = self.selected_line;
        }
        if self.selected_line >= self.scroll_offset + visible_lines {
            self.scroll_offset = self.selected_line.saturating_sub(visible_lines) + 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.line_count > 0 {
            self.selected_line = (self.selected_line + 1).min(self.line_count.saturating_sub(1));
        }
    }

    pub fn move_up(&mut self) {
        self.selected_line = self.selected_line.saturating_sub(1);
    }

    pub fn page_down(&mut self, step: usize) {
        if self.line_count > 0 {
            self.selected_line =
                (self.selected_line + step).min(self.line_count.saturating_sub(1));
        }
    }

    pub fn page_up(&mut self, step: usize) {
        self.selected_line = self.selected_line.saturating_sub(step);
    }

    pub fn jump_to_first(&mut self) {
        self.selected_line = 0;
        self.scroll_offset = 0;
    }

    pub fn jump_to_last(&mut self) {
        if self.line_count > 0 {
            self.selected_line = self.line_count.saturating_sub(1);
        }
    }
}

// ========================================
// DiffCacheStore<K>
// ========================================

/// プリフェッチ対象アイテム
pub struct PrefetchItem<K> {
    pub key: K,
    pub file_index: usize,
    pub filename: String,
    pub patch: String,
}

/// DiffCacheStore から receiver を除いた snapshot 用サブセット
pub struct DiffCacheSnapshot<K: Hash + Eq + Clone> {
    pub current: Option<DiffCache>,
    pub current_key: Option<K>,
    pub store: HashMap<K, DiffCache>,
}

/// ジェネリックキー型の Diff キャッシュストア
///
/// App/GitOps は `K = usize`（file_index）、GitLog は `K = String`（SHA）。
pub struct DiffCacheStore<K: Hash + Eq + Clone + Send + 'static> {
    pub current: Option<DiffCache>,
    current_key: Option<K>,
    pub(crate) store: HashMap<K, DiffCache>,
    max_store_entries: usize,
    highlight_rx: Option<mpsc::Receiver<(K, DiffCache)>>,
    prefetch_rx: Option<mpsc::Receiver<(K, DiffCache)>>,
}

impl<K: Hash + Eq + Clone + Send + 'static> DiffCacheStore<K> {
    pub fn new(max_store_entries: usize) -> Self {
        Self {
            current: None,
            current_key: None,
            store: HashMap::new(),
            max_store_entries,
            highlight_rx: None,
            prefetch_rx: None,
        }
    }

    pub fn current_key(&self) -> Option<&K> {
        self.current_key.as_ref()
    }

    /// ストアから `remove()` して current にセット。旧 current は store に退避（highlighted のみ）。
    ///
    /// `expected_hash` が Some の場合、patch_hash が一致しなければ false を返す。
    pub fn try_restore(&mut self, key: &K, expected_hash: Option<u64>) -> bool {
        if let Some(cached) = self.store.remove(key) {
            if let Some(hash) = expected_hash {
                if cached.patch_hash != hash {
                    // 無効 — store に戻さず破棄
                    return false;
                }
            }
            // 旧 current を store に退避
            self.retire_current();
            self.current = Some(cached);
            self.current_key = Some(key.clone());
            true
        } else {
            false
        }
    }

    /// current をセット。旧 current が highlighted なら store に退避。plain は破棄。
    pub fn set_current(&mut self, key: K, cache: DiffCache) {
        self.retire_current();
        self.current = Some(cache);
        self.current_key = Some(key);
    }

    /// ハイライトレシーバー設定
    pub fn set_highlight_rx(&mut self, rx: mpsc::Receiver<(K, DiffCache)>) {
        self.highlight_rx = Some(rx);
    }

    /// プリフェッチレシーバー設定
    pub fn set_prefetch_rx(&mut self, rx: mpsc::Receiver<(K, DiffCache)>) {
        self.prefetch_rx = Some(rx);
    }

    /// ハイライトキャッシュのポーリング。current_key + patch_hash で stale チェック。
    /// 更新があれば true を返す。
    pub fn poll_highlight(&mut self) -> bool {
        let Some(ref mut rx) = self.highlight_rx else {
            return false;
        };

        match rx.try_recv() {
            Ok((key, cache)) => {
                self.highlight_rx = None;
                // stale チェック: current_key が一致し、patch_hash も一致する場合のみ current にセット
                let is_current = self.current_key.as_ref() == Some(&key);
                let hash_matches = self
                    .current
                    .as_ref()
                    .map(|c| c.patch_hash == cache.patch_hash)
                    .unwrap_or(false);

                if is_current && hash_matches {
                    self.current = Some(cache);
                    true
                } else {
                    // stale だが highlighted なのでストアに格納
                    if cache.highlighted && self.store.len() < self.max_store_entries {
                        self.store.insert(key, cache);
                    }
                    false
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => false,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                self.highlight_rx = None;
                false
            }
        }
    }

    /// プリフェッチ結果のポーリング。距離ベース eviction 付き。
    pub fn poll_prefetch(&mut self, distance_fn: impl Fn(&K) -> usize) {
        let Some(ref mut rx) = self.prefetch_rx else {
            return;
        };

        loop {
            match rx.try_recv() {
                Ok((key, cache)) => {
                    // 現在表示中でハイライト済みならスキップ
                    if self
                        .current
                        .as_ref()
                        .is_some_and(|c| c.highlighted)
                        && self.current_key.as_ref() == Some(&key)
                    {
                        continue;
                    }
                    // ストアに既にあればスキップ
                    if self.store.contains_key(&key) {
                        continue;
                    }
                    // サイズ上限チェック: 超過時は最も遠いエントリを削除
                    if self.store.len() >= self.max_store_entries {
                        let evict_key = self
                            .store
                            .keys()
                            .max_by_key(|k| distance_fn(k))
                            .cloned();
                        if let Some(k) = evict_key {
                            self.store.remove(&k);
                        }
                    }
                    self.store.insert(key, cache);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.prefetch_rx = None;
                    break;
                }
            }
        }
    }

    /// 全破棄
    pub fn clear(&mut self) {
        self.current = None;
        self.current_key = None;
        self.store.clear();
        self.highlight_rx = None;
        self.prefetch_rx = None;
    }

    /// current + highlight_rx のみ破棄
    pub fn clear_current(&mut self) {
        self.current = None;
        self.current_key = None;
        self.highlight_rx = None;
    }

    /// prefetch_rx をドロップ（プリフェッチ停止）
    pub fn drop_prefetch_rx(&mut self) {
        self.prefetch_rx = None;
    }

    /// 全ファイルのハイライト済みキャッシュをバックグラウンドで事前構築
    ///
    /// 呼び出し側は未キャッシュのアイテム一覧を収集して渡す。
    /// チャネル・spawn_blocking・ParserPool 管理は DiffCacheStore 側で行う。
    pub fn start_prefetch(
        &mut self,
        items: Vec<PrefetchItem<K>>,
        theme: &str,
        markdown_rich: bool,
        tab_width: u8,
    ) {
        if items.is_empty() {
            return;
        }
        let channel_size = items.len();
        let (tx, rx) = mpsc::channel(channel_size);
        self.prefetch_rx = Some(rx);

        let theme = theme.to_string();
        tokio::task::spawn_blocking(move || {
            let mut parser_pool = ParserPool::new();
            for item in items {
                let mut cache = build_diff_cache(
                    &item.patch,
                    &item.filename,
                    &theme,
                    &mut parser_pool,
                    markdown_rich,
                    tab_width,
                );
                cache.file_index = item.file_index;
                if tx.blocking_send((item.key, cache)).is_err() {
                    break;
                }
            }
        });
    }

    /// 条件一致エントリを store から選択削除
    pub fn invalidate_if(&mut self, pred: impl Fn(&K, &DiffCache) -> bool) {
        self.store.retain(|k, v| !pred(k, v));
    }

    /// ストアのエントリ数を返す
    pub fn store_len(&self) -> usize {
        self.store.len()
    }

    /// ストアに指定キーが含まれるか
    pub fn store_contains_key(&self, key: &K) -> bool {
        self.store.contains_key(key)
    }

    /// プリフェッチレシーバーが設定されているか
    pub fn has_prefetch_rx(&self) -> bool {
        self.prefetch_rx.is_some()
    }

    /// ハイライトレシーバーが設定されているか
    pub fn has_highlight_rx(&self) -> bool {
        self.highlight_rx.is_some()
    }

    /// snapshot を取得（receiver はドロップ）
    pub fn take_snapshot(&mut self) -> DiffCacheSnapshot<K> {
        self.highlight_rx = None;
        self.prefetch_rx = None;
        DiffCacheSnapshot {
            current: self.current.take(),
            current_key: self.current_key.take(),
            store: std::mem::take(&mut self.store),
        }
    }

    /// snapshot から復元
    pub fn restore_snapshot(&mut self, snapshot: DiffCacheSnapshot<K>) {
        self.current = snapshot.current;
        self.current_key = snapshot.current_key;
        self.store = snapshot.store;
        self.highlight_rx = None;
        self.prefetch_rx = None;
    }

    /// 旧 current を store に退避（highlighted のみ）
    fn retire_current(&mut self) {
        if let Some(old) = self.current.take() {
            if old.highlighted {
                if let Some(key) = self.current_key.take() {
                    if self.store.len() < self.max_store_entries {
                        self.store.insert(key, old);
                    }
                }
            }
        }
        self.current_key = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lasso::Rodeo;

    // ========================================
    // DiffScrollState テスト
    // ========================================

    // --- Margin モード（既存 adjust_scroll テスト移植） ---

    #[test]
    fn test_margin_above_viewport() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.selected_line = 2;
        s.scroll_offset = 5;
        s.line_count = 100;

        s.adjust_scroll(20);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn test_margin_below_viewport() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.selected_line = 30;
        s.scroll_offset = 5;
        s.line_count = 100;

        s.adjust_scroll(20);
        assert!(s.scroll_offset > 5);
        assert!(s.selected_line >= s.scroll_offset);
        assert!(s.selected_line < s.scroll_offset + 20);
    }

    #[test]
    fn test_margin_within_viewport() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.selected_line = 15;
        s.scroll_offset = 5;
        s.line_count = 100;

        s.adjust_scroll(20);
        assert!(s.selected_line >= s.scroll_offset);
        assert!(s.selected_line < s.scroll_offset + 20);
    }

    #[test]
    fn test_margin_zero_visible() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.selected_line = 10;
        s.scroll_offset = 5;
        s.line_count = 100;

        s.adjust_scroll(0);
        assert_eq!(s.scroll_offset, 5);
    }

    #[test]
    fn test_margin_at_last_line() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 50;
        s.selected_line = 49;
        s.scroll_offset = 0;

        s.adjust_scroll(20);
        assert!(s.selected_line >= s.scroll_offset);
        assert!(s.selected_line < s.scroll_offset + 20);
        assert_eq!(s.scroll_offset, 40);
    }

    #[test]
    fn test_margin_single_line() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 1;
        s.selected_line = 0;
        s.scroll_offset = 0;

        s.adjust_scroll(20);
        assert_eq!(s.scroll_offset, 0);
        assert_eq!(s.selected_line, 0);
    }

    #[test]
    fn test_margin_invariant_all_positions() {
        for line_count in [1usize, 5, 10, 20, 50, 100] {
            for visible_lines in [1usize, 5, 10, 20, 40] {
                for selected_line in 0..line_count {
                    for initial_scroll in [
                        0,
                        selected_line.saturating_sub(visible_lines),
                        selected_line,
                    ] {
                        let mut s = DiffScrollState::new(ScrollMode::Margin);
                        s.line_count = line_count;
                        s.selected_line = selected_line;
                        s.scroll_offset = initial_scroll;

                        s.adjust_scroll(visible_lines);

                        assert!(
                            s.selected_line >= s.scroll_offset,
                            "cursor above: sel={}, scroll={}, vis={}, count={}, init={}",
                            s.selected_line, s.scroll_offset, visible_lines, line_count, initial_scroll,
                        );
                        assert!(
                            s.selected_line < s.scroll_offset + visible_lines,
                            "cursor below: sel={}, scroll={}, vis={}, count={}, init={}",
                            s.selected_line, s.scroll_offset, visible_lines, line_count, initial_scroll,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_margin_sequential_down_no_jump() {
        let line_count = 100;
        let visible_lines = 20;

        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = line_count;

        let mut prev_scroll = 0;
        for line in 0..line_count {
            s.selected_line = line;
            s.adjust_scroll(visible_lines);

            assert!(
                s.selected_line >= s.scroll_offset
                    && s.selected_line < s.scroll_offset + visible_lines,
            );
            assert!(
                s.scroll_offset <= prev_scroll + 1,
                "scroll jumped at line={}: prev={}, now={}",
                line, prev_scroll, s.scroll_offset,
            );
            prev_scroll = s.scroll_offset;
        }
    }

    #[test]
    fn test_margin_sequential_up_no_jump() {
        let line_count = 100;
        let visible_lines = 20;

        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = line_count;
        s.selected_line = line_count - 1;
        s.scroll_offset = line_count.saturating_sub(visible_lines);
        s.adjust_scroll(visible_lines);

        let mut prev_scroll = s.scroll_offset;
        for line in (0..line_count).rev() {
            s.selected_line = line;
            s.adjust_scroll(visible_lines);

            assert!(
                s.selected_line >= s.scroll_offset
                    && s.selected_line < s.scroll_offset + visible_lines,
            );
            assert!(
                prev_scroll <= s.scroll_offset + 1,
                "scroll jumped at line={}: prev={}, now={}",
                line, prev_scroll, s.scroll_offset,
            );
            prev_scroll = s.scroll_offset;
        }
    }

    #[test]
    fn test_margin_file_shorter_than_viewport() {
        let visible_lines = 40;
        for line_count in [1, 5, 10, 39] {
            for line in 0..line_count {
                let mut s = DiffScrollState::new(ScrollMode::Margin);
                s.line_count = line_count;
                s.selected_line = line;

                s.adjust_scroll(visible_lines);
                assert_eq!(s.scroll_offset, 0);
            }
        }
    }

    // --- Edge モード（GitLog テスト移植） ---

    #[test]
    fn test_edge_scroll_down() {
        let mut s = DiffScrollState::new(ScrollMode::Edge);
        s.selected_line = 30;
        s.scroll_offset = 0;
        s.line_count = 100;

        s.adjust_scroll(20);
        assert_eq!(s.scroll_offset, 11); // 30 - 20 + 1 = 11
    }

    #[test]
    fn test_edge_scroll_up() {
        let mut s = DiffScrollState::new(ScrollMode::Edge);
        s.selected_line = 5;
        s.scroll_offset = 10;
        s.line_count = 100;

        s.adjust_scroll(20);
        assert_eq!(s.scroll_offset, 5);
    }

    #[test]
    fn test_edge_zero_visible() {
        let mut s = DiffScrollState::new(ScrollMode::Edge);
        s.selected_line = 10;
        s.scroll_offset = 5;
        s.line_count = 100;

        s.adjust_scroll(0);
        // No change
        assert_eq!(s.scroll_offset, 5);
    }

    #[test]
    fn test_edge_within_viewport() {
        let mut s = DiffScrollState::new(ScrollMode::Edge);
        s.selected_line = 15;
        s.scroll_offset = 10;
        s.line_count = 100;

        s.adjust_scroll(20);
        // 15 >= 10 and 15 < 10 + 20 → no change
        assert_eq!(s.scroll_offset, 10);
    }

    // --- set_line_count テスト ---

    #[test]
    fn test_set_line_count_clamp() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.selected_line = 50;
        s.scroll_offset = 40;

        s.set_line_count(30);
        assert_eq!(s.selected_line, 29);
        assert_eq!(s.scroll_offset, 29);
    }

    #[test]
    fn test_set_line_count_zero() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.selected_line = 10;
        s.scroll_offset = 5;

        s.set_line_count(0);
        assert_eq!(s.selected_line, 0);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn test_set_line_count_no_clamp_needed() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.selected_line = 5;
        s.scroll_offset = 3;

        s.set_line_count(100);
        assert_eq!(s.selected_line, 5);
        assert_eq!(s.scroll_offset, 3);
    }

    // --- move_down / move_up / page / jump テスト ---

    #[test]
    fn test_move_down_boundary() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 5;
        s.selected_line = 4;
        s.move_down();
        assert_eq!(s.selected_line, 4); // clamped

        s.selected_line = 3;
        s.move_down();
        assert_eq!(s.selected_line, 4);
    }

    #[test]
    fn test_move_down_zero_lines() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 0;
        s.move_down();
        assert_eq!(s.selected_line, 0);
    }

    #[test]
    fn test_move_up_boundary() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 5;
        s.selected_line = 0;
        s.move_up();
        assert_eq!(s.selected_line, 0); // clamped

        s.selected_line = 3;
        s.move_up();
        assert_eq!(s.selected_line, 2);
    }

    #[test]
    fn test_page_down() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 100;
        s.selected_line = 10;
        s.page_down(20);
        assert_eq!(s.selected_line, 30);

        s.selected_line = 90;
        s.page_down(20);
        assert_eq!(s.selected_line, 99);
    }

    #[test]
    fn test_page_up() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 100;
        s.selected_line = 30;
        s.page_up(20);
        assert_eq!(s.selected_line, 10);

        s.selected_line = 5;
        s.page_up(20);
        assert_eq!(s.selected_line, 0);
    }

    #[test]
    fn test_jump_to_first() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 100;
        s.selected_line = 50;
        s.scroll_offset = 40;
        s.jump_to_first();
        assert_eq!(s.selected_line, 0);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn test_jump_to_last() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 100;
        s.selected_line = 0;
        s.jump_to_last();
        assert_eq!(s.selected_line, 99);
    }

    #[test]
    fn test_jump_to_last_empty() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 0;
        s.jump_to_last();
        assert_eq!(s.selected_line, 0);
    }

    #[test]
    fn test_reset() {
        let mut s = DiffScrollState::new(ScrollMode::Margin);
        s.line_count = 100;
        s.selected_line = 50;
        s.scroll_offset = 30;
        s.reset();
        assert_eq!(s.selected_line, 0);
        assert_eq!(s.scroll_offset, 0);
        assert_eq!(s.line_count, 100); // line_count は維持
    }

    // ========================================
    // DiffCacheStore テスト
    // ========================================

    fn make_cache(highlighted: bool, patch_hash: u64) -> DiffCache {
        DiffCache {
            file_index: 0,
            patch_hash,
            lines: vec![],
            interner: Rodeo::default(),
            highlighted,
            markdown_rich: false,
        }
    }

    #[test]
    fn test_set_current_retires_highlighted() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(0, make_cache(true, 100));
        // 新しい current をセット → 旧 current がストアに退避
        store.set_current(1, make_cache(false, 200));

        assert!(store.store_contains_key(&0));
        assert_eq!(store.current_key(), Some(&1));
    }

    #[test]
    fn test_set_current_discards_plain() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(0, make_cache(false, 100)); // plain
        store.set_current(1, make_cache(false, 200));

        // plain キャッシュはストアに退避されない
        assert!(!store.store_contains_key(&0));
    }

    #[test]
    fn test_try_restore_success() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.store.insert(5, make_cache(true, 500));

        assert!(store.try_restore(&5, None));
        assert_eq!(store.current_key(), Some(&5));
        assert!(store.current.as_ref().unwrap().patch_hash == 500);
        assert!(!store.store_contains_key(&5));
    }

    #[test]
    fn test_try_restore_with_hash_match() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.store.insert(5, make_cache(true, 500));

        assert!(store.try_restore(&5, Some(500)));
        assert_eq!(store.current_key(), Some(&5));
    }

    #[test]
    fn test_try_restore_with_hash_mismatch() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.store.insert(5, make_cache(true, 500));

        assert!(!store.try_restore(&5, Some(999)));
        // cache は破棄される
        assert!(!store.store_contains_key(&5));
        assert!(store.current.is_none());
    }

    #[test]
    fn test_try_restore_not_found() {
        let mut store = DiffCacheStore::<usize>::new(50);
        assert!(!store.try_restore(&5, None));
    }

    #[test]
    fn test_try_restore_retires_current() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(0, make_cache(true, 100));
        store.store.insert(5, make_cache(true, 500));

        store.try_restore(&5, None);
        // 旧 current (key=0) がストアに退避
        assert!(store.store_contains_key(&0));
        assert_eq!(store.current_key(), Some(&5));
    }

    #[test]
    fn test_poll_highlight_updates_current() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(3, make_cache(false, 300));

        let (tx, rx) = mpsc::channel(1);
        store.set_highlight_rx(rx);
        tx.try_send((3, make_cache(true, 300))).unwrap();

        assert!(store.poll_highlight());
        assert!(store.current.as_ref().unwrap().highlighted);
    }

    #[test]
    fn test_poll_highlight_stale_key() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(3, make_cache(false, 300));

        let (tx, rx) = mpsc::channel(1);
        store.set_highlight_rx(rx);
        // 異なるキー
        tx.try_send((99, make_cache(true, 300))).unwrap();

        assert!(!store.poll_highlight());
        // stale でも highlighted ならストアに格納
        assert!(store.store_contains_key(&99));
    }

    #[test]
    fn test_poll_highlight_stale_hash() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(3, make_cache(false, 300));

        let (tx, rx) = mpsc::channel(1);
        store.set_highlight_rx(rx);
        // キーは一致するがハッシュが異なる
        tx.try_send((3, make_cache(true, 999))).unwrap();

        assert!(!store.poll_highlight());
    }

    #[test]
    fn test_poll_prefetch_with_eviction() {
        let mut store = DiffCacheStore::<usize>::new(3);

        // 3件格納
        store.store.insert(0, make_cache(true, 100));
        store.store.insert(1, make_cache(true, 200));
        store.store.insert(2, make_cache(true, 300));

        let (tx, rx) = mpsc::channel(1);
        store.set_prefetch_rx(rx);
        tx.try_send((5, make_cache(true, 500))).unwrap();
        drop(tx);

        // distance_fn: 距離は key そのもの（current_selected = 5 想定）
        store.poll_prefetch(|k| k.abs_diff(5));

        assert!(store.store_contains_key(&5));
        assert_eq!(store.store_len(), 3); // eviction されて3件維持
        // key=0 が最も遠い（距離5）
        assert!(!store.store_contains_key(&0));
    }

    #[test]
    fn test_poll_prefetch_skips_current() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(3, make_cache(true, 300));

        let (tx, rx) = mpsc::channel(1);
        store.set_prefetch_rx(rx);
        tx.try_send((3, make_cache(true, 300))).unwrap();
        drop(tx);

        store.poll_prefetch(|k| *k);
        // current と同じキーはスキップ
        assert!(!store.store_contains_key(&3));
    }

    #[test]
    fn test_poll_prefetch_skips_existing() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.store.insert(3, make_cache(true, 300));

        let (tx, rx) = mpsc::channel(1);
        store.set_prefetch_rx(rx);
        tx.try_send((3, make_cache(true, 999))).unwrap();
        drop(tx);

        store.poll_prefetch(|k| *k);
        // 既にストアにあるキーはスキップ（上書きしない）
        assert_eq!(store.store.get(&3).unwrap().patch_hash, 300);
    }

    #[test]
    fn test_invalidate_if() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.store.insert(0, make_cache(true, 100));
        store.store.insert(1, {
            let mut c = make_cache(true, 200);
            c.markdown_rich = true;
            c
        });
        store.store.insert(2, make_cache(true, 300));

        store.invalidate_if(|_, c| c.markdown_rich);
        assert_eq!(store.store_len(), 2);
        assert!(!store.store_contains_key(&1));
    }

    #[test]
    fn test_clear() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(0, make_cache(true, 100));
        store.store.insert(1, make_cache(true, 200));

        store.clear();
        assert!(store.current.is_none());
        assert!(store.current_key.is_none());
        assert_eq!(store.store_len(), 0);
    }

    #[test]
    fn test_clear_current() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(0, make_cache(true, 100));
        store.store.insert(1, make_cache(true, 200));

        store.clear_current();
        assert!(store.current.is_none());
        assert!(store.current_key.is_none());
        // ストアは維持
        assert!(store.store_contains_key(&1));
    }

    #[test]
    fn test_take_snapshot_and_restore() {
        let mut store = DiffCacheStore::<usize>::new(50);
        store.set_current(3, make_cache(true, 300));
        store.store.insert(5, make_cache(true, 500));

        let snapshot = store.take_snapshot();
        assert!(store.current.is_none());
        assert_eq!(store.store_len(), 0);

        // snapshot の中身を確認
        assert!(snapshot.current.is_some());
        assert_eq!(snapshot.current_key, Some(3));
        assert!(snapshot.store.contains_key(&5));

        // 復元
        store.restore_snapshot(snapshot);
        assert_eq!(store.current_key(), Some(&3));
        assert!(store.store_contains_key(&5));
    }

    #[test]
    fn test_store_retirement_limit() {
        let mut store = DiffCacheStore::<usize>::new(2);
        store.set_current(0, make_cache(true, 100));
        store.set_current(1, make_cache(true, 200));
        // ストアに key=0 が退避
        assert!(store.store_contains_key(&0));

        store.set_current(2, make_cache(true, 300));
        // ストアに key=1 が退避 → ストア上限2
        assert!(store.store_contains_key(&1));
        assert_eq!(store.store_len(), 2);

        // これ以上退避しても上限で破棄
        store.set_current(3, make_cache(true, 400));
        // key=2 は退避できない（ストア上限）
        assert!(!store.store_contains_key(&2));
        assert_eq!(store.store_len(), 2);
    }
}
