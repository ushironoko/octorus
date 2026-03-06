use crossterm::event::{self, KeyCode, KeyModifiers};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use tokio::sync::mpsc;

use crate::config::KeybindingsConfig;
use crate::github;
use crate::keybinding::KeySequence;
use crate::syntax::ParserPool;
use crate::ui::diff_view::{build_commit_diff_cache, build_plain_diff_cache};

use super::types::*;
use super::{App, AppState};

impl App {
    /// 1ページあたりのコミット取得件数
    const COMMITS_PER_PAGE: u32 = 30;

    /// Git Log 画面を開く
    pub(crate) fn open_git_log(&mut self) {
        let mut state = GitLogState::new();
        self.fetch_commits_page(&mut state, 1);
        self.git_log_state = Some(state);
        self.state = AppState::GitLogSplitCommitList;
    }

    /// コミット一覧の指定ページをバックグラウンドで取得
    fn fetch_commits_page(&self, gl: &mut GitLogState, page: u32) {
        gl.commits_loading = true;
        gl.commits_page = page;

        let (tx, rx) = mpsc::channel(1);
        gl.commit_list_receiver = Some(rx);

        let per_page = Self::COMMITS_PER_PAGE;

        if self.local_mode {
            let working_dir = self.working_dir.clone();
            let offset = (page - 1) * per_page;
            tokio::spawn(async move {
                let result = github::fetch_local_commits(working_dir.as_deref(), offset, per_page)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(result).await;
            });
        } else {
            let repo = self.repo.clone();
            let pr_number = self.pr_number();
            tokio::spawn(async move {
                let result = github::fetch_pr_commits(&repo, pr_number, page, per_page)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(result).await;
            });
        }
    }

    /// 追加のコミットを読み込み（無限スクロール用）
    fn load_more_commits(&mut self) {
        let Some(ref mut gl) = self.git_log_state else {
            return;
        };
        if gl.commits_loading || !gl.commits_has_more {
            return;
        }
        let next_page = gl.commits_page + 1;
        self.fetch_commits_page_for_current(next_page);
    }

    /// 現在の git_log_state に対してページ取得を開始（&mut self が必要なケース）
    fn fetch_commits_page_for_current(&mut self, page: u32) {
        let Some(ref mut gl) = self.git_log_state else {
            return;
        };
        gl.commits_loading = true;
        gl.commits_page = page;

        let (tx, rx) = mpsc::channel(1);
        gl.commit_list_receiver = Some(rx);

        let per_page = Self::COMMITS_PER_PAGE;

        if self.local_mode {
            let working_dir = self.working_dir.clone();
            let offset = (page - 1) * per_page;
            tokio::spawn(async move {
                let result = github::fetch_local_commits(working_dir.as_deref(), offset, per_page)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(result).await;
            });
        } else {
            let repo = self.repo.clone();
            let pr_number = self.pr_number();
            tokio::spawn(async move {
                let result = github::fetch_pr_commits(&repo, pr_number, page, per_page)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(result).await;
            });
        }
    }

    /// Git Log 画面を閉じる（全状態を破棄）
    pub(crate) fn close_git_log(&mut self) {
        self.git_log_state = None;
        self.state = AppState::FileList;
    }

    /// コミット選択変更時に diff をバックグラウンド取得
    pub(crate) fn start_fetch_commit_diff(&mut self) {
        let Some(ref mut gl) = self.git_log_state else {
            return;
        };
        let Some(commit) = gl.commits.get(gl.selected_commit) else {
            return;
        };
        let sha = commit.sha.clone();

        // キャッシュヒットチェック
        if let Some(cached) = gl.diff_cache_map.get(&sha) {
            gl.diff_cache = Some(DiffCache {
                file_index: gl.selected_commit,
                patch_hash: cached.patch_hash,
                lines: cached.lines.clone(),
                interner: cached.interner.clone(),
                highlighted: cached.highlighted,
                markdown_rich: false,
            });
            gl.diff_loading = false;
            gl.diff_error = None;
            gl.selected_line = 0;
            gl.scroll_offset = 0;
            // Cancel any in-flight diff fetch to prevent stale responses
            // from overwriting the current cache-hit result
            gl.pending_diff_sha = None;
            gl.commit_diff_receiver = None;
            gl.highlight_receiver = None;
            return;
        }

        gl.diff_loading = true;
        gl.diff_error = None;
        gl.pending_diff_sha = Some(sha.clone());

        let (tx, rx) = mpsc::channel(1);
        gl.commit_diff_receiver = Some(rx);

        if self.local_mode {
            let working_dir = self.working_dir.clone();
            tokio::spawn(async move {
                let result = github::fetch_local_commit_diff(working_dir.as_deref(), &sha)
                    .await
                    .map(|diff| (sha, diff))
                    .map_err(|e| e.to_string());
                let _ = tx.send(result).await;
            });
        } else {
            let repo = self.repo.clone();
            tokio::spawn(async move {
                let result = github::fetch_commit_diff(&repo, &sha)
                    .await
                    .map(|diff| (sha, diff))
                    .map_err(|e| e.to_string());
                let _ = tx.send(result).await;
            });
        }
    }

    /// コミット一覧取得後にバックグラウンドで先頭 N 件の diff をプリフェッチ
    ///
    /// 選択中のコミットは `start_fetch_commit_diff()` が処理するためスキップする。
    ///
    /// 2フェーズ構成:
    ///   Phase 1: 並列 `tokio::spawn` で diff テキストを fetch（ネットワーク I/O 並列化）
    ///   Phase 2: 単一 `spawn_blocking` で順次ハイライト構築（`ParserPool` 共有）
    ///
    /// 既存の `start_prefetch_all_files()` パターンに準拠。
    fn start_prefetch_commit_diffs(&mut self) {
        let Some(ref mut gl) = self.git_log_state else {
            return;
        };

        // 既存のプリフェッチを中断
        gl.prefetch_diff_receiver = None;

        let selected_sha = gl
            .commits
            .get(gl.selected_commit)
            .map(|c| c.sha.clone())
            .unwrap_or_default();

        let shas_to_prefetch: Vec<String> = gl
            .commits
            .iter()
            .take(MAX_GIT_LOG_DIFF_CACHE)
            .filter(|c| c.sha != selected_sha && !gl.diff_cache_map.contains_key(&c.sha))
            .map(|c| c.sha.clone())
            .collect();

        if shas_to_prefetch.is_empty() {
            return;
        }

        // Phase 1 → Phase 2 の中間チャネル（fetch結果をhighlighterへ渡す）
        let (fetch_tx, mut fetch_rx) =
            mpsc::channel::<(String, String)>(MAX_GIT_LOG_DIFF_CACHE);
        // Phase 2 → poll の結果チャネル
        let (result_tx, result_rx) = mpsc::channel(MAX_GIT_LOG_DIFF_CACHE);
        gl.prefetch_diff_receiver = Some(result_rx);

        let local_mode = self.local_mode;
        let repo = self.repo.clone();
        let working_dir = self.working_dir.clone();
        let theme = self.config.diff.theme.clone();
        let tab_width = self.config.diff.tab_width;

        // Phase 1: 並列 async fetch
        for sha in shas_to_prefetch {
            let tx = fetch_tx.clone();
            let repo = repo.clone();
            let working_dir = working_dir.clone();

            tokio::spawn(async move {
                let diff_text = if local_mode {
                    github::fetch_local_commit_diff(working_dir.as_deref(), &sha).await
                } else {
                    github::fetch_commit_diff(&repo, &sha).await
                };
                if let Ok(diff_text) = diff_text {
                    let _ = tx.send((sha, diff_text)).await;
                }
            });
        }
        drop(fetch_tx); // 全 fetch 完了で fetch_rx が Disconnected になる

        // Phase 2: 単一 spawn_blocking で順次ハイライト（ParserPool 共有）
        tokio::task::spawn_blocking(move || {
            let mut parser_pool = ParserPool::new();
            while let Some((sha, diff_text)) = fetch_rx.blocking_recv() {
                let cache =
                    build_commit_diff_cache(&diff_text, &theme, &mut parser_pool, tab_width);
                if result_tx.blocking_send((sha, cache)).is_err() {
                    break; // receiver がドロップされた
                }
            }
        });
    }

    /// ポーリング: コミット一覧 + diff の受信
    pub(crate) fn poll_git_log_updates(&mut self) {
        let Some(ref mut gl) = self.git_log_state else {
            return;
        };

        // コミット一覧の受信
        if let Some(ref mut rx) = gl.commit_list_receiver {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(page) => {
                        let is_first_page = gl.commits.is_empty();
                        gl.commits.extend(page.items);
                        gl.commits_has_more = page.has_more;
                        gl.commits_loading = false;
                        gl.commits_error = None;
                        // 初回ページ到着時のみ: 最初のコミットの diff 取得 + プリフェッチ
                        if is_first_page && !gl.commits.is_empty() {
                            gl.commit_list_receiver = None;
                            self.start_fetch_commit_diff();
                            self.start_prefetch_commit_diffs();
                            return;
                        }
                    }
                    Err(e) => {
                        gl.commits_loading = false;
                        gl.commits_error = Some(e);
                    }
                }
                if let Some(ref mut gl) = self.git_log_state {
                    gl.commit_list_receiver = None;
                }
                return;
            }
        }

        // コミット diff の受信
        if let Some(ref mut rx) = gl.commit_diff_receiver {
            if let Ok(result) = rx.try_recv() {
                let tab_width = self.config.diff.tab_width;
                match result {
                    Ok((sha, diff_text)) => {
                        let mut cache = build_plain_diff_cache(&diff_text, tab_width);
                        cache.file_index = gl.selected_commit;

                        let is_current = gl
                            .pending_diff_sha
                            .as_ref()
                            .is_some_and(|pending| *pending == sha);
                        if is_current {
                            gl.commit_diff = Some(diff_text.clone());
                            gl.diff_cache = Some(cache);
                            gl.diff_loading = false;
                            gl.diff_error = None;
                            gl.selected_line = 0;
                            gl.scroll_offset = 0;
                        }

                        // ハイライト版をバックグラウンドで構築
                        let theme = self.config.diff.theme.clone();
                        let sha_clone = sha.clone();
                        let selected = gl.selected_commit;
                        let (tx, rx_hl) = mpsc::channel(1);
                        gl.highlight_receiver = Some(rx_hl);

                        tokio::task::spawn_blocking(move || {
                            let mut parser_pool = ParserPool::new();
                            let mut hl_cache = build_commit_diff_cache(
                                &diff_text,
                                &theme,
                                &mut parser_pool,
                                tab_width,
                            );
                            hl_cache.file_index = selected;
                            let _ = tx.try_send((sha_clone, hl_cache));
                        });
                    }
                    Err(e) => {
                        let is_current = gl.pending_diff_sha.is_some();
                        if is_current {
                            gl.diff_loading = false;
                            gl.diff_error = Some(e);
                        }
                    }
                }
                gl.commit_diff_receiver = None;
            }
        }

        // ハイライト済み diff キャッシュの受信
        if let Some(ref mut rx) = gl.highlight_receiver {
            if let Ok((sha, hl_cache)) = rx.try_recv() {
                Self::evict_git_log_diff_cache(gl, &sha);
                gl.diff_cache_map.insert(sha.clone(), DiffCache {
                    file_index: hl_cache.file_index,
                    patch_hash: hl_cache.patch_hash,
                    lines: hl_cache.lines.clone(),
                    interner: hl_cache.interner.clone(),
                    highlighted: true,
                    markdown_rich: false,
                });

                // 現在選択中のコミットならスワップ
                let is_current = gl
                    .commits
                    .get(gl.selected_commit)
                    .is_some_and(|c| c.sha == sha);
                if is_current {
                    gl.diff_cache = Some(hl_cache);
                }
                gl.highlight_receiver = None;
            }
        }

        // プリフェッチ diff の受信（1 tick で複数結果をドレイン）
        // gl の二重借用を避けるため、receiver を一時的に take する
        if let Some(mut rx) = gl.prefetch_diff_receiver.take() {
            loop {
                match rx.try_recv() {
                    Ok((sha, hl_cache)) => {
                        // キャッシュ済みならスキップ
                        if gl.diff_cache_map.contains_key(&sha) {
                            continue;
                        }
                        Self::evict_git_log_diff_cache(gl, &sha);
                        gl.diff_cache_map.insert(sha.clone(), DiffCache {
                            file_index: 0,
                            patch_hash: hl_cache.patch_hash,
                            lines: hl_cache.lines.clone(),
                            interner: hl_cache.interner.clone(),
                            highlighted: true,
                            markdown_rich: false,
                        });

                        // 現在選択中でまだプレーンキャッシュなら HL にアップグレード
                        let is_current = gl
                            .commits
                            .get(gl.selected_commit)
                            .is_some_and(|c| c.sha == sha);
                        if is_current
                            && gl
                                .diff_cache
                                .as_ref()
                                .is_some_and(|c| !c.highlighted)
                        {
                            gl.diff_cache = Some(hl_cache);
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {
                        gl.prefetch_diff_receiver = Some(rx);
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
        }
    }

    /// diff_cache_map の LRU eviction
    fn evict_git_log_diff_cache(gl: &mut GitLogState, _current_sha: &str) {
        if gl.diff_cache_map.len() < MAX_GIT_LOG_DIFF_CACHE {
            return;
        }

        let selected = gl.selected_commit;
        let mut farthest_sha: Option<String> = None;
        let mut max_distance: usize = 0;

        for sha in gl.diff_cache_map.keys() {
            let distance = gl
                .commits
                .iter()
                .position(|c| c.sha == *sha)
                .map(|pos| pos.abs_diff(selected))
                .unwrap_or(usize::MAX);

            if distance > max_distance || farthest_sha.is_none() {
                max_distance = distance;
                farthest_sha = Some(sha.clone());
            }
        }

        if let Some(sha) = farthest_sha {
            gl.diff_cache_map.remove(&sha);
        }
    }

    /// GitLogSplitCommitList の入力処理
    pub(crate) fn handle_git_log_split_commit_list_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> anyhow::Result<()> {
        let kb = self.config.keybindings.clone();

        // Quit / Esc → FileList に戻る
        if self.matches_single_key(&key, &kb.quit) || key.code == KeyCode::Esc {
            self.close_git_log();
            return Ok(());
        }

        // エラー時の r でリトライ
        if self
            .git_log_state
            .as_ref()
            .is_some_and(|gl| gl.commits_error.is_some())
            && key.code == KeyCode::Char('r')
        {
            // 追加ロード失敗の場合はエラーをクリアして再取得
            if let Some(ref gl) = self.git_log_state {
                if !gl.commits.is_empty() {
                    let page = gl.commits_page;
                    if let Some(ref mut gl) = self.git_log_state {
                        gl.commits_error = None;
                    }
                    self.fetch_commits_page_for_current(page);
                    return Ok(());
                }
            }
            self.git_log_state = None;
            self.open_git_log();
            return Ok(());
        }

        // diff エラー時の r でリトライ（diff ペインにフォーカスできないため
        // ここでもリトライを受け付ける）
        if self
            .git_log_state
            .as_ref()
            .is_some_and(|gl| gl.diff_error.is_some())
            && key.code == KeyCode::Char('r')
        {
            self.start_fetch_commit_diff();
            return Ok(());
        }

        // コミット移動 → 移動後に diff 取得
        let moved = handle_commit_list_navigation(
            &mut self.git_log_state,
            &key,
            &kb,
            terminal.size()?.height as usize,
        );
        if moved {
            self.start_fetch_commit_diff();
            // 無限スクロール: 残り5件で次ページを取得
            if let Some(ref gl) = self.git_log_state {
                if gl.commits_has_more
                    && !gl.commits_loading
                    && gl.selected_commit + 5 >= gl.commits.len()
                {
                    self.load_more_commits();
                }
            }
            return Ok(());
        }

        // Focus diff pane (Tab, Enter, Right, l)
        if key.code == KeyCode::Tab
            || self.matches_single_key(&key, &kb.open_panel)
            || self.matches_single_key(&key, &kb.move_right)
            || key.code == KeyCode::Right
        {
            if self
                .git_log_state
                .as_ref()
                .is_some_and(|gl| gl.diff_cache.is_some())
            {
                self.state = AppState::GitLogSplitDiff;
            }
            return Ok(());
        }

        // Help
        if self.matches_single_key(&key, &kb.help) {
            self.previous_state = AppState::GitLogSplitCommitList;
            self.state = AppState::Help;
            self.help_scroll_offset = 0;
            self.config_scroll_offset = 0;
            return Ok(());
        }

        Ok(())
    }

    /// GitLogSplitDiff の入力処理（右ペインフォーカス）
    pub(crate) fn handle_git_log_split_diff_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> anyhow::Result<()> {
        let kb = self.config.keybindings.clone();
        let term_h = terminal.size()?.height as usize;
        let visible_lines = (term_h * 65 / 100).saturating_sub(8);

        // Back to commit list (Left, h, Esc)
        if self.matches_single_key(&key, &kb.move_left)
            || key.code == KeyCode::Left
            || key.code == KeyCode::Esc
        {
            self.state = AppState::GitLogSplitCommitList;
            return Ok(());
        }

        // Quit to FileList
        if self.matches_single_key(&key, &kb.quit) {
            self.close_git_log();
            return Ok(());
        }

        // Fullscreen diff (Enter, Right, l)
        if self.matches_single_key(&key, &kb.open_panel)
            || self.matches_single_key(&key, &kb.move_right)
            || key.code == KeyCode::Right
        {
            if self
                .git_log_state
                .as_ref()
                .is_some_and(|gl| gl.diff_cache.is_some())
            {
                self.state = AppState::GitLogDiffView;
            }
            return Ok(());
        }

        // diff error retry
        if self
            .git_log_state
            .as_ref()
            .is_some_and(|gl| gl.diff_error.is_some())
            && key.code == KeyCode::Char('r')
        {
            self.start_fetch_commit_diff();
            return Ok(());
        }

        // Diff scroll navigation
        handle_diff_scroll_navigation(&mut self.git_log_state, &key, &kb, visible_lines);

        Ok(())
    }

    /// GitLogDiffView の入力処理（フルスクリーン diff）
    pub(crate) fn handle_git_log_diff_view_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> anyhow::Result<()> {
        let kb = self.config.keybindings.clone();
        let term_h = terminal.size()?.height as usize;
        let visible_lines = term_h.saturating_sub(8);

        // Back to split diff (q, Esc, Left, h)
        if self.matches_single_key(&key, &kb.quit)
            || key.code == KeyCode::Esc
            || self.matches_single_key(&key, &kb.move_left)
            || key.code == KeyCode::Left
        {
            self.state = AppState::GitLogSplitDiff;
            return Ok(());
        }

        // Diff scroll navigation
        handle_diff_scroll_navigation(&mut self.git_log_state, &key, &kb, visible_lines);

        Ok(())
    }
}

/// コミット一覧のナビゲーション（self を借用しない free function）
///
/// 選択が変更された場合は true を返す（呼び出し側で diff 取得をトリガー）
fn handle_commit_list_navigation(
    git_log_state: &mut Option<GitLogState>,
    key: &event::KeyEvent,
    kb: &KeybindingsConfig,
    term_height: usize,
) -> bool {
    let Some(ref mut gl) = git_log_state else {
        return false;
    };
    if gl.commits.is_empty() {
        return false;
    }

    let old = gl.selected_commit;
    let max_idx = gl.commits.len().saturating_sub(1);

    // Move down
    if matches_key(key, &kb.move_down) || key.code == KeyCode::Down {
        gl.selected_commit = (gl.selected_commit + 1).min(max_idx);
    }
    // Move up
    else if matches_key(key, &kb.move_up) || key.code == KeyCode::Up {
        gl.selected_commit = gl.selected_commit.saturating_sub(1);
    }
    // Page down
    else if matches_key(key, &kb.page_down) || is_shift_char(key, 'j') {
        let step = term_height.saturating_sub(8).max(1);
        gl.selected_commit = (gl.selected_commit + step).min(max_idx);
    }
    // Page up
    else if matches_key(key, &kb.page_up) || is_shift_char(key, 'k') {
        let step = term_height.saturating_sub(8).max(1);
        gl.selected_commit = gl.selected_commit.saturating_sub(step);
    }
    // Jump to first (g)
    else if key.code == KeyCode::Char('g') {
        gl.selected_commit = 0;
    }
    // Jump to last (G)
    else if matches_key(key, &kb.jump_to_last) {
        gl.selected_commit = max_idx;
    } else {
        return false;
    }

    gl.selected_commit != old
}

/// Diff スクロールナビゲーション（self を借用しない free function）
fn handle_diff_scroll_navigation(
    git_log_state: &mut Option<GitLogState>,
    key: &event::KeyEvent,
    kb: &KeybindingsConfig,
    visible_lines: usize,
) {
    let Some(ref mut gl) = git_log_state else {
        return;
    };
    let line_count = gl
        .diff_cache
        .as_ref()
        .map(|c| c.lines.len())
        .unwrap_or(0);
    if line_count == 0 {
        return;
    }

    let max_line = line_count.saturating_sub(1);

    // Move down
    if matches_key(key, &kb.move_down) || key.code == KeyCode::Down {
        gl.selected_line = (gl.selected_line + 1).min(max_line);
    }
    // Move up
    else if matches_key(key, &kb.move_up) || key.code == KeyCode::Up {
        gl.selected_line = gl.selected_line.saturating_sub(1);
    }
    // Page down
    else if matches_key(key, &kb.page_down) || is_shift_char(key, 'j') {
        gl.selected_line = (gl.selected_line + 20).min(max_line);
    }
    // Page up
    else if matches_key(key, &kb.page_up) || is_shift_char(key, 'k') {
        gl.selected_line = gl.selected_line.saturating_sub(20);
    }
    // Jump to first (g)
    else if key.code == KeyCode::Char('g') {
        gl.selected_line = 0;
        gl.scroll_offset = 0;
        return;
    }
    // Jump to last (G)
    else if matches_key(key, &kb.jump_to_last) {
        gl.selected_line = max_line;
    } else {
        return;
    }

    adjust_git_log_scroll(gl, visible_lines);
}

/// 単一キーマッチ（App::matches_single_key の free function 版）
fn matches_key(event: &event::KeyEvent, seq: &KeySequence) -> bool {
    if !seq.is_single() {
        return false;
    }
    if let Some(first) = seq.first() {
        first.matches(event)
    } else {
        false
    }
}

/// Shift+文字キー判定（App::is_shift_char_shortcut の free function 版）
fn is_shift_char(event: &event::KeyEvent, lower: char) -> bool {
    if event.modifiers.contains(KeyModifiers::CONTROL)
        || event.modifiers.contains(KeyModifiers::ALT)
    {
        return false;
    }
    let upper = lower.to_ascii_uppercase();
    match event.code {
        KeyCode::Char(c) if c == upper => true,
        KeyCode::Char(c) if c == lower && event.modifiers.contains(KeyModifiers::SHIFT) => true,
        _ => false,
    }
}

/// Git Log 用のスクロール調整
fn adjust_git_log_scroll(gl: &mut GitLogState, visible_lines: usize) {
    if visible_lines == 0 {
        return;
    }
    if gl.selected_line < gl.scroll_offset {
        gl.scroll_offset = gl.selected_line;
    }
    if gl.selected_line >= gl.scroll_offset + visible_lines {
        gl.scroll_offset = gl.selected_line.saturating_sub(visible_lines) + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_close_git_log_clears_state() {
        let mut app = App::new_for_test();
        app.git_log_state = Some(GitLogState::new());
        app.state = AppState::GitLogSplitCommitList;

        app.close_git_log();

        assert!(app.git_log_state.is_none());
        assert_eq!(app.state, AppState::FileList);
    }

    #[test]
    fn test_adjust_git_log_scroll_down() {
        let mut gl = GitLogState::new();
        gl.selected_line = 30;
        gl.scroll_offset = 0;
        adjust_git_log_scroll(&mut gl, 20);
        assert_eq!(gl.scroll_offset, 11); // 30 - 20 + 1 = 11
    }

    #[test]
    fn test_adjust_git_log_scroll_up() {
        let mut gl = GitLogState::new();
        gl.selected_line = 5;
        gl.scroll_offset = 10;
        adjust_git_log_scroll(&mut gl, 20);
        assert_eq!(gl.scroll_offset, 5);
    }

    #[test]
    fn test_evict_git_log_diff_cache() {
        let mut gl = GitLogState::new();
        gl.commits = (0..15)
            .map(|i| crate::github::PrCommit {
                sha: format!("sha{:02}", i),
                message: format!("commit {}", i),
                author_name: "author".to_string(),
                author_login: None,
                date: String::new(),
            })
            .collect();
        gl.selected_commit = 5;

        // Fill cache to MAX
        for i in 0..MAX_GIT_LOG_DIFF_CACHE {
            let sha = format!("sha{:02}", i);
            gl.diff_cache_map.insert(
                sha,
                DiffCache {
                    file_index: i,
                    patch_hash: 0,
                    lines: vec![],
                    interner: lasso::Rodeo::default(),
                    highlighted: false,
                    markdown_rich: false,
                },
            );
        }

        assert_eq!(gl.diff_cache_map.len(), MAX_GIT_LOG_DIFF_CACHE);

        // Eviction should remove the farthest entry from selected_commit (5)
        App::evict_git_log_diff_cache(&mut gl, "sha10");
        assert_eq!(gl.diff_cache_map.len(), MAX_GIT_LOG_DIFF_CACHE - 1);
        // sha00 (distance 5) is farther than sha09 (distance 4)
        assert!(!gl.diff_cache_map.contains_key("sha00"));
    }

    /// Regression test: moving to a cached commit while an uncached fetch is
    /// in-flight must cancel the stale receiver/pending_diff_sha so that the
    /// stale response cannot overwrite the cache-hit result.
    #[test]
    fn test_cache_hit_clears_inflight_state() {
        let mut app = App::new_for_test();
        let mut gl = GitLogState::new();

        gl.commits = (0..3)
            .map(|i| crate::github::PrCommit {
                sha: format!("sha{}", i),
                message: format!("commit {}", i),
                author_name: "author".to_string(),
                author_login: None,
                date: String::new(),
            })
            .collect();

        // Pre-populate cache for commit 1 ("sha1")
        gl.diff_cache_map.insert(
            "sha1".to_string(),
            DiffCache {
                file_index: 1,
                patch_hash: 111,
                lines: vec![],
                interner: lasso::Rodeo::default(),
                highlighted: false,
                markdown_rich: false,
            },
        );

        // Simulate in-flight fetch for commit 0 ("sha0")
        let (tx, rx) = mpsc::channel(1);
        gl.commit_diff_receiver = Some(rx);
        gl.pending_diff_sha = Some("sha0".to_string());
        gl.diff_loading = true;

        // Now select commit 1 (which is cached)
        gl.selected_commit = 1;
        app.git_log_state = Some(gl);

        app.start_fetch_commit_diff();

        let gl = app.git_log_state.as_ref().unwrap();

        // Cache hit should have cleared the stale in-flight state
        assert!(gl.pending_diff_sha.is_none(), "pending_diff_sha should be cleared on cache hit");
        assert!(gl.commit_diff_receiver.is_none(), "commit_diff_receiver should be cleared on cache hit");
        assert!(!gl.diff_loading, "diff_loading should be false on cache hit");

        // The diff_cache should reflect the cached commit 1 data
        let cache = gl.diff_cache.as_ref().unwrap();
        assert_eq!(cache.patch_hash, 111);

        // Ensure the sender is now orphaned (receiver dropped)
        drop(tx);
    }
}
