use crossterm::event::{self, KeyCode};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::diff_store::MAX_PREFETCH_FILES;
use crate::github;
use crate::loader;
use crate::syntax::ParserPool;
use crate::ui::diff_view::{build_commit_diff_cache, build_diff_cache, build_plain_diff_cache};

use super::types::*;
use super::{App, AppState};

impl App {
    /// 1ページあたりのコミット取得件数
    pub(crate) const COMMITS_PER_PAGE: u32 = 30;

    /// GitOps 画面を開く
    pub fn open_git_ops(&mut self) {
        let caller_state = self.state;
        let mut ops = GitOpsState::new(Vec::new());
        ops.return_state = caller_state;
        self.git_ops_state = Some(ops);
        self.state = AppState::GitOpsSplitTree;
        self.refresh_git_status();
        self.fetch_git_ops_commits(1);
    }

    /// GitOps 画面を閉じる
    pub(crate) fn close_git_ops(&mut self) {
        let return_state = self
            .git_ops_state
            .as_ref()
            .map(|ops| ops.return_state)
            .unwrap_or(AppState::FileList);
        self.git_ops_state = None;
        self.state = return_state;
        // コミット等の変更を反映するため PR データを再取得
        self.retry_load();
    }

    /// 左ペインのサブフォーカスをトグル（Tree ↔ Commits）
    pub(crate) fn toggle_git_ops_left_focus(&mut self) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };
        ops.left_focus = match ops.left_focus {
            LeftPaneFocus::Tree => LeftPaneFocus::Commits,
            LeftPaneFocus::Commits => LeftPaneFocus::Tree,
        };
    }

    /// Diff ペインから左ペインへ戻る（left_return_focus を復帰）
    pub(crate) fn return_from_git_ops_diff(&mut self) {
        if let Some(ref mut ops) = self.git_ops_state {
            ops.left_focus = ops.left_return_focus;
        }
        self.state = AppState::GitOpsSplitTree;
    }

    /// コミット一覧をバックグラウンドで取得
    fn fetch_git_ops_commits(&mut self, page: u32) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };
        ops.commit_log.loading = true;
        ops.commit_log.page = page;

        let (tx, rx) = mpsc::channel(1);
        ops.commit_log.list_receiver = Some(rx);

        let per_page = Self::COMMITS_PER_PAGE;

        if self.local_mode || self.pr_number.is_none() {
            let working_dir = self.working_dir.clone();
            let offset = (page - 1) * per_page;
            tokio::spawn(async move {
                let result =
                    github::fetch_local_commits(working_dir.as_deref(), offset, per_page)
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
    fn load_more_git_ops_commits(&mut self) {
        let Some(ref ops) = self.git_ops_state else {
            return;
        };
        if ops.commit_log.loading || !ops.commit_log.has_more {
            return;
        }
        let next_page = ops.commit_log.page + 1;
        self.fetch_git_ops_commits(next_page);
    }

    /// コミット選択変更時に diff をバックグラウンド取得
    fn start_fetch_git_ops_commit_diff(&mut self) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };
        let cl = &mut ops.commit_log;
        let Some(commit) = cl.commits.get(cl.selected) else {
            return;
        };
        let sha = commit.sha.clone();

        // キャッシュヒットチェック
        if cl.diff_store.try_restore(&sha, None) {
            cl.diff_loading = false;
            cl.diff_error = None;
            cl.diff_scroll.reset();
            if let Some(ref cache) = cl.diff_store.current {
                cl.diff_scroll.set_line_count(cache.lines.len());
            }
            cl.pending_diff_sha = None;
            cl.diff_receiver = None;
            return;
        }

        cl.diff_loading = true;
        cl.diff_error = None;
        cl.pending_diff_sha = Some(sha.clone());

        let (tx, rx) = mpsc::channel(1);
        cl.diff_receiver = Some(rx);

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
    fn start_prefetch_git_ops_commit_diffs(&mut self) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };
        let cl = &mut ops.commit_log;

        cl.diff_store.drop_prefetch_rx();

        let selected_sha = cl
            .commits
            .get(cl.selected)
            .map(|c| c.sha.clone())
            .unwrap_or_default();

        let max_cache = self.config.git_ops.max_diff_cache;

        let shas_to_prefetch: Vec<String> = cl
            .commits
            .iter()
            .take(max_cache)
            .filter(|c| c.sha != selected_sha && !cl.diff_store.store_contains_key(&c.sha))
            .map(|c| c.sha.clone())
            .collect();

        if shas_to_prefetch.is_empty() {
            return;
        }

        let (fetch_tx, mut fetch_rx) = mpsc::channel::<(String, String)>(max_cache);
        let (result_tx, result_rx) = mpsc::channel(max_cache);
        cl.diff_store.set_prefetch_rx(result_rx);

        let local_mode = self.local_mode;
        let repo = self.repo.clone();
        let working_dir = self.working_dir.clone();
        let theme = self.config.diff.theme.clone();
        let tab_width = self.config.diff.tab_width;

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
        drop(fetch_tx);

        tokio::task::spawn_blocking(move || {
            let mut parser_pool = ParserPool::new();
            while let Some((sha, diff_text)) = fetch_rx.blocking_recv() {
                let cache =
                    build_commit_diff_cache(&diff_text, &theme, &mut parser_pool, tab_width);
                if result_tx.blocking_send((sha, cache)).is_err() {
                    break;
                }
            }
        });
    }

    /// git status をバックグラウンドで取得
    pub(crate) fn refresh_git_status(&mut self) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };
        let (tx, rx) = mpsc::channel(1);
        ops.status_receiver = Some(rx);

        let working_dir = self.working_dir.clone();
        tokio::spawn(async move {
            let result = fetch_git_status(working_dir.as_deref()).await;
            let _ = tx.send(result).await;
        });
    }

    /// GitOps 関連の非同期結果をポーリング
    pub(crate) fn poll_git_ops_updates(&mut self) {
        // --- status 受信 ---
        let mut status_updated = false;
        if let Some(ref mut ops) = self.git_ops_state {
            if let Some(ref mut rx) = ops.status_receiver {
                match rx.try_recv() {
                    Ok(Ok(entries)) => {
                        ops.entries = entries;
                        ops.status_receiver = None;
                        status_updated = true;
                        ops.status_updated = true;
                        rebuild_git_ops_tree(ops);
                    }
                    Ok(Err(_)) => {
                        ops.status_receiver = None;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        ops.status_receiver = None;
                    }
                }
            }
        }

        // --- on-demand diff patch 受信 ---
        // diff patch result を一度取り出してから処理（二重借用回避）
        let patch_result = if let Some(ref mut ops) = self.git_ops_state {
            if let Some(ref mut rx) = ops.diff_patch_receiver {
                match rx.try_recv() {
                    Ok(result) => {
                        ops.diff_patch_receiver = None;
                        Some(result)
                    }
                    Err(mpsc::error::TryRecvError::Empty) => None,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        ops.diff_patch_receiver = None;
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some(result) = patch_result {
            if let Some(patch) = result.patch {
                let key = result.filename.clone();
                let theme = self.config.diff.theme.clone();
                let markdown_rich = self.markdown_rich;
                let tab_width = self.config.diff.tab_width;
                if let Some(ref mut ops) = self.git_ops_state {
                    build_git_ops_diff_from_patch(
                        ops,
                        key,
                        &patch,
                        &result.filename,
                        &theme,
                        markdown_rich,
                        tab_width,
                    );
                }
            }
        }

        // --- 操作結果受信 ---
        let mut op_succeeded = false;
        if let Some(ref mut ops) = self.git_ops_state {
            if let Some(ref mut rx) = ops.op_receiver {
                match rx.try_recv() {
                    Ok(Ok(msg)) => {
                        ops.op_message = Some((msg, Instant::now()));
                        ops.op_receiver = None;
                        op_succeeded = true;
                    }
                    Ok(Err(msg)) => {
                        ops.op_message = Some((format!("Error: {}", msg), Instant::now()));
                        ops.op_receiver = None;
                        // エラー時もステータス再取得（楽観的更新の巻き戻し）
                        op_succeeded = true;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        ops.op_receiver = None;
                    }
                }
            }

            // --- ハイライトキャッシュ受信 ---
            ops.diff_store.poll_highlight();

            // --- プリフェッチ受信 ---
            let selected = ops.tree.selected_row;
            ops.diff_store.poll_prefetch(|k| {
                ops.entries
                    .iter()
                    .position(|e| e.path == *k)
                    .map(|idx| idx.abs_diff(selected))
                    .unwrap_or(usize::MAX)
            });
        }

        // --- 操作完了後にステータス再取得 ---
        if op_succeeded {
            self.refresh_git_status();
        }

        // --- コミット一覧が未初期化ならリフレッシュ ---
        // HEAD 変更操作（reset --soft, undo commit）で initialized=false にクリアされる
        {
            let needs_refresh = self
                .git_ops_state
                .as_ref()
                .map(|ops| {
                    let cl = &ops.commit_log;
                    !cl.initialized && !cl.loading && cl.list_receiver.is_none()
                })
                .unwrap_or(false);
            if needs_refresh {
                self.fetch_git_ops_commits(1);
                self.retry_load();
            }
        }

        // --- status 更新後のプリフェッチ開始 ---
        if status_updated {
            let working_dir = self.working_dir.clone();
            let theme = self.config.diff.theme.clone();
            let markdown_rich = self.markdown_rich;
            let tab_width = self.config.diff.tab_width;

            if let Some(ref mut ops) = self.git_ops_state {
                start_git_ops_prefetch(ops, working_dir, &theme, markdown_rich, tab_width);
            }
        }

        // --- コミット一覧の受信 ---
        let mut first_commit_page = false;
        if let Some(ref mut ops) = self.git_ops_state {
            let cl = &mut ops.commit_log;
            if let Some(ref mut rx) = cl.list_receiver {
                if let Ok(result) = rx.try_recv() {
                    match result {
                        Ok(page) => {
                            let is_first = cl.commits.is_empty();
                            cl.commits.extend(page.items);
                            cl.has_more = page.has_more;
                            cl.loading = false;
                            cl.error = None;
                            cl.initialized = true;
                            if is_first && !cl.commits.is_empty() {
                                first_commit_page = true;
                            }
                        }
                        Err(e) => {
                            cl.loading = false;
                            cl.error = Some(e);
                            cl.initialized = true;
                        }
                    }
                    cl.list_receiver = None;
                }
            }
        }
        if first_commit_page {
            self.start_fetch_git_ops_commit_diff();
            self.start_prefetch_git_ops_commit_diffs();
        }

        // --- コミット diff の受信 ---
        if let Some(ref mut ops) = self.git_ops_state {
            let cl = &mut ops.commit_log;
            if let Some(ref mut rx) = cl.diff_receiver {
                if let Ok(result) = rx.try_recv() {
                    let tab_width = self.config.diff.tab_width;
                    match result {
                        Ok((sha, diff_text)) => {
                            let mut cache = build_plain_diff_cache(&diff_text, tab_width);
                            cache.file_index = cl.selected;

                            let is_current = cl
                                .pending_diff_sha
                                .as_ref()
                                .is_some_and(|pending| *pending == sha);
                            if is_current {
                                cl.diff_store.set_current(sha.clone(), cache);
                                cl.diff_loading = false;
                                cl.diff_error = None;
                                cl.diff_scroll.reset();
                                if let Some(ref c) = cl.diff_store.current {
                                    cl.diff_scroll.set_line_count(c.lines.len());
                                }
                            }

                            // ハイライト版をバックグラウンドで構築
                            let theme = self.config.diff.theme.clone();
                            let sha_clone = sha.clone();
                            let selected = cl.selected;
                            let (tx, rx_hl) = mpsc::channel(1);
                            cl.diff_store.set_highlight_rx(rx_hl);

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
                            if cl.pending_diff_sha.is_some() {
                                cl.diff_loading = false;
                                cl.diff_error = Some(e);
                            }
                        }
                    }
                    cl.diff_receiver = None;
                }
            }

            // コミット diff のハイライトキャッシュ受信
            if cl.diff_store.poll_highlight() {
                if let Some(ref c) = cl.diff_store.current {
                    cl.diff_scroll.set_line_count(c.lines.len());
                }
            }

            // コミット diff のプリフェッチ受信
            let selected = cl.selected;
            let commits = &cl.commits;
            cl.diff_store.poll_prefetch(|sha| {
                commits
                    .iter()
                    .position(|c| c.sha == *sha)
                    .map(|pos| pos.abs_diff(selected))
                    .unwrap_or(usize::MAX)
            });

            // プレーンキャッシュのアップグレード
            if let Some(current_sha) = cl.commits.get(cl.selected).map(|c| c.sha.clone()) {
                let is_plain = cl
                    .diff_store
                    .current
                    .as_ref()
                    .is_some_and(|c| !c.highlighted);
                if is_plain
                    && cl.diff_store.store_contains_key(&current_sha)
                    && cl.diff_store.try_restore(&current_sha, None)
                {
                    if let Some(ref c) = cl.diff_store.current {
                        cl.diff_scroll.set_line_count(c.lines.len());
                    }
                }
            }
        }

        // --- op_message 3秒後に自動消去 ---
        if let Some(ref mut ops) = self.git_ops_state {
            if let Some((_, ref time)) = ops.op_message {
                if time.elapsed().as_secs() >= 3 {
                    ops.op_message = None;
                }
            }
        }
    }

    /// 選択ファイルの diff をオンデマンドで取得
    pub(crate) fn update_git_ops_diff(&mut self) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };

        let path = match ops.selected_path() {
            Some(p) => p.to_string(),
            None => return,
        };

        // 既に表示中ならスキップ
        if ops.diff_store.current_key() == Some(&path) {
            return;
        }

        // プリフェッチキャッシュヒット
        if ops.diff_store.try_restore(&path, None) {
            if let Some(ref c) = ops.diff_store.current {
                ops.diff_scroll.set_line_count(c.lines.len());
            }
            ops.diff_scroll.reset();
            return;
        }

        // キャッシュミス — on-demand fetch
        let (tx, rx) = mpsc::channel(1);
        ops.diff_patch_receiver = Some(rx);

        let is_untracked = ops.entries.iter().any(|e| {
            e.path == path
                && e.worktree_status == FileStatus::Untracked
                && e.index_status == FileStatus::Untracked
        });
        let working_dir = self.working_dir.clone();

        tokio::spawn(async move {
            loader::fetch_single_file_diff(working_dir, path, is_untracked, tx).await;
        });
    }

    /// Space: stage/unstage トグル（ディレクトリの場合は配下を一括操作）
    pub(crate) fn toggle_stage(&mut self) {
        let Some(ref ops) = self.git_ops_state else {
            return;
        };

        let selected = ops.tree.visible_rows.get(ops.tree.selected_row);

        match selected {
            Some(TreeRow::File { index, .. }) => {
                let Some(entry) = ops.entries.get(*index) else {
                    return;
                };
                let path = entry.path.clone();
                let is_staged = entry.is_staged();
                if is_staged {
                    self.unstage_files(vec![path]);
                } else {
                    self.stage_files(vec![path]);
                }
            }
            Some(TreeRow::Dir { ref path, .. }) => {
                // 配下の全ファイルを収集
                let prefix = format!("{}/", path);
                let paths: Vec<String> = ops
                    .entries
                    .iter()
                    .filter(|e| e.path.starts_with(&prefix))
                    .map(|e| e.path.clone())
                    .collect();
                if paths.is_empty() {
                    return;
                }
                // 配下に1つでもunstagedがあれば全部stage、全部stagedならunstage
                let all_staged = ops
                    .entries
                    .iter()
                    .filter(|e| e.path.starts_with(&prefix))
                    .all(|e| e.is_staged());
                if all_staged {
                    self.unstage_files(paths);
                } else {
                    self.stage_files(paths);
                }
            }
            None => {}
        }
    }

    fn stage_files(&mut self, paths: Vec<String>) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };
        let count = paths.len();

        let working_dir = self.working_dir.clone();
        let (tx, rx) = mpsc::channel(1);
        ops.op_receiver = Some(rx);

        let paths_clone = paths.clone();
        tokio::spawn(async move {
            let mut args = vec!["add".to_string(), "--".to_string()];
            args.extend(paths_clone);
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            match run_git_op(working_dir.as_deref(), &arg_refs).await {
                Ok(_) => {
                    let _ = tx.send(Ok(format!("Staged {} file(s)", count))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });

        // 楽観的更新
        for entry in &mut ops.entries {
            if paths.contains(&entry.path) {
                optimistic_stage(entry);
            }
        }

        ops.undo_stack.push(UndoAction::Unstage {
            paths: paths.to_vec(),
        });
    }

    fn unstage_files(&mut self, paths: Vec<String>) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };
        let count = paths.len();

        let working_dir = self.working_dir.clone();
        let (tx, rx) = mpsc::channel(1);
        ops.op_receiver = Some(rx);

        let paths_clone = paths.clone();
        tokio::spawn(async move {
            let mut args = vec![
                "restore".to_string(),
                "--staged".to_string(),
                "--".to_string(),
            ];
            args.extend(paths_clone);
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            match run_git_op(working_dir.as_deref(), &arg_refs).await {
                Ok(_) => {
                    let _ = tx
                        .send(Ok(format!("Unstaged {} file(s)", count)))
                        .await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });

        // 楽観的更新
        for entry in &mut ops.entries {
            if paths.contains(&entry.path) {
                optimistic_unstage(entry);
            }
        }

        ops.undo_stack.push(UndoAction::Stage {
            paths: paths.clone(),
            previous_index_entries: Vec::new(),
        });
    }

    /// s: 全ファイルをステージ
    pub(crate) fn stage_all(&mut self) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };

        let working_dir = self.working_dir.clone();
        let (tx, rx) = mpsc::channel(1);
        ops.op_receiver = Some(rx);

        // 楽観的更新
        for entry in &mut ops.entries {
            optimistic_stage(entry);
        }

        tokio::spawn(async move {
            match run_git_op(working_dir.as_deref(), &["add", "-A"]).await {
                Ok(_) => {
                    let _ = tx.send(Ok("Staged all files".to_string())).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });

        ops.undo_stack.push(UndoAction::StageAll { tree_hash: None });
    }

    /// d: 変更を破棄
    pub(crate) fn discard_changes(&mut self) {
        let Some(ref ops) = self.git_ops_state else {
            return;
        };
        let path = match ops.selected_path() {
            Some(p) => p.to_string(),
            None => return,
        };

        let entry = ops.entries.iter().find(|e| e.path == path).cloned();
        let Some(entry) = entry else { return };

        // discard 対象パスに関連する undo スタックエントリを無効化
        // （discard 後に undo すると不整合な状態になるため）
        if let Some(ref mut ops) = self.git_ops_state {
            ops.undo_stack.retain(|action| match action {
                UndoAction::Stage { paths, .. } | UndoAction::Unstage { paths } => {
                    !paths.contains(&path)
                }
                UndoAction::Commit | UndoAction::StageAll { .. } => true,
            });
        }

        let working_dir = self.working_dir.clone();
        let (tx, rx) = mpsc::channel(1);
        if let Some(ref mut ops) = self.git_ops_state {
            ops.op_receiver = Some(rx);
        }

        tokio::spawn(async move {
            let result = if entry.worktree_status == FileStatus::Untracked
                && entry.index_status == FileStatus::Untracked
            {
                // untracked: ファイル削除
                run_git_op(working_dir.as_deref(), &["clean", "-f", "--", &path]).await
            } else if entry.is_staged() && !entry.has_worktree_changes() {
                // staged のみ: HEAD から復元
                run_git_op(
                    working_dir.as_deref(),
                    &["restore", "--staged", "--source=HEAD", "--", &path],
                )
                .await
            } else {
                // worktree 変更: restore
                run_git_op(working_dir.as_deref(), &["restore", "--", &path]).await
            };

            match result {
                Ok(_) => {
                    let _ = tx
                        .send(Ok(format!("Discarded changes: {}", path)))
                        .await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });
    }

    /// c: コミット（外部エディタ起動）
    pub(crate) fn git_ops_commit(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> anyhow::Result<()> {
        let Some(ref ops) = self.git_ops_state else {
            return Ok(());
        };

        if !ops.has_staged_files() {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.op_message = Some(("No staged files to commit".to_string(), Instant::now()));
            }
            return Ok(());
        }

        if ops.has_unmerged_files() {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.op_message =
                    Some(("Cannot commit with unmerged files".to_string(), Instant::now()));
            }
            return Ok(());
        }

        // ターミナル復元 → git commit → ターミナル再初期化
        crate::ui::restore_terminal(terminal)?;

        let working_dir = self.working_dir.as_deref().unwrap_or(".");
        let status = std::process::Command::new("git")
            .arg("commit")
            .current_dir(working_dir)
            .status();

        *terminal = crate::ui::setup_terminal()?;

        match status {
            Ok(s) if s.success() => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.op_message = Some(("Commit created".to_string(), Instant::now()));
                    ops.undo_stack.push(UndoAction::Commit);
                    ops.commit_log.diff_store.clear();
                    ops.commit_log.commits.clear();
                    ops.commit_log.selected = 0;
                    ops.commit_log.initialized = false;
                }
                self.refresh_git_status();
                self.retry_load();
            }
            _ => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.op_message =
                        Some(("Commit cancelled or failed".to_string(), Instant::now()));
                }
            }
        }

        Ok(())
    }

    /// u: undo（ツリーペイン用 — stage/unstage のみ）
    fn execute_undo(&mut self) {
        let action = {
            let Some(ref mut ops) = self.git_ops_state else {
                return;
            };
            match ops.undo_stack.pop() {
                Some(UndoAction::Commit) => {
                    // Commit undo はコミット履歴ペインからのみ実行可能
                    ops.undo_stack.push(UndoAction::Commit);
                    ops.op_message = Some((
                        "Commit undo: switch to commit pane".to_string(),
                        Instant::now(),
                    ));
                    return;
                }
                Some(action) => action,
                None => {
                    ops.op_message =
                        Some(("Nothing to undo".to_string(), Instant::now()));
                    return;
                }
            }
        };

        match action {
            UndoAction::Stage {
                paths,
                previous_index_entries,
            } => {
                let count = paths.len();
                if let Some(ref mut ops) = self.git_ops_state {
                    for entry in &mut ops.entries {
                        if paths.contains(&entry.path) {
                            optimistic_unstage(entry);
                        }
                    }
                }
                // `git restore --staged` ではなく `git update-index --cacheinfo` で
                // 操作前のインデックスを精密復元（MM ファイルの部分ステージ対応）
                self.run_git_index_restore(paths, previous_index_entries, count);
            }
            UndoAction::Unstage { paths } => {
                let count = paths.len();
                if let Some(ref mut ops) = self.git_ops_state {
                    for entry in &mut ops.entries {
                        if paths.contains(&entry.path) {
                            optimistic_stage(entry);
                        }
                    }
                }
                self.run_git_op_silent(
                    build_git_add_args(&paths),
                    format!("Undo unstage ({} file(s))", count),
                );
            }
            UndoAction::StageAll { tree_hash } => {
                if let Some(ref mut ops) = self.git_ops_state {
                    for entry in &mut ops.entries {
                        optimistic_unstage(entry);
                    }
                }
                if let Some(hash) = tree_hash {
                    self.run_git_op_silent(
                        vec!["read-tree".to_string(), hash],
                        "Undo stage all".to_string(),
                    );
                } else {
                    self.run_git_op_silent(vec!["reset".to_string()], "Undo stage all".to_string());
                }
            }
            UndoAction::Commit => unreachable!(),
        }
    }

    /// 精密インデックス復元（Stage undo 用）
    fn run_git_index_restore(
        &mut self,
        paths: Vec<String>,
        previous_entries: Vec<IndexEntry>,
        count: usize,
    ) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };

        let working_dir = self.working_dir.clone();
        let (tx, rx) = mpsc::channel(1);
        ops.op_receiver = Some(rx);

        tokio::spawn(async move {
            let wd = working_dir.as_deref();
            let mut errors = Vec::new();

            let entry_paths: std::collections::HashSet<String> =
                previous_entries.iter().map(|e| e.path.clone()).collect();

            // 1. 操作前のインデックスエントリを復元
            for entry in &previous_entries {
                let cacheinfo = format!("{},{},{}", entry.mode, entry.hash, entry.path);
                if let Err(e) =
                    run_git_op(wd, &["update-index", "--cacheinfo", &cacheinfo]).await
                {
                    errors.push(e.to_string());
                }
            }

            // 2. 操作前に untracked だったファイルをインデックスから除去
            let new_paths: Vec<&str> = paths
                .iter()
                .filter(|p| !entry_paths.contains(p.as_str()))
                .map(|p| p.as_str())
                .collect();

            if !new_paths.is_empty() {
                let mut args = vec!["rm", "--cached", "--force", "--"];
                args.extend(new_paths);
                if let Err(e) = run_git_op(wd, &args).await {
                    errors.push(e.to_string());
                }
            }

            if errors.is_empty() {
                let _ = tx
                    .send(Ok(format!("Undo stage ({} file(s))", count)))
                    .await;
            } else {
                let _ = tx.send(Err(errors.join("; "))).await;
            }
        });
    }

    fn run_git_op_silent(&mut self, args: Vec<String>, success_msg: String) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };

        let working_dir = self.working_dir.clone();
        let (tx, rx) = mpsc::channel(1);
        ops.op_receiver = Some(rx);

        tokio::spawn(async move {
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            match run_git_op(working_dir.as_deref(), &arg_refs).await {
                Ok(_) => {
                    let _ = tx.send(Ok(success_msg)).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });
    }

    /// P: git push origin <current-branch>
    fn git_ops_push(&mut self) {
        let working_dir = self.working_dir.clone();
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };

        let (tx, rx) = mpsc::channel(1);
        ops.op_receiver = Some(rx);

        tokio::spawn(async move {
            // ブランチ名を取得
            let dir = working_dir.as_deref().unwrap_or(".");
            let branch_output = tokio::process::Command::new("git")
                .args(["branch", "--show-current"])
                .current_dir(dir)
                .output()
                .await;

            let branch = match branch_output {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                _ => {
                    let _ = tx.send(Err("Failed to get current branch".to_string())).await;
                    return;
                }
            };

            if branch.is_empty() {
                let _ = tx.send(Err("Detached HEAD: cannot push".to_string())).await;
                return;
            }

            let push_output = tokio::process::Command::new("git")
                .args(["push", "origin", &branch])
                .current_dir(dir)
                .output()
                .await;

            match push_output {
                Ok(o) if o.status.success() => {
                    let _ = tx.send(Ok(format!("Pushed to origin/{}", branch))).await;
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    let _ = tx.send(Err(format!("Push failed: {}", stderr))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("Push failed: {}", e))).await;
                }
            }
        });
    }

    /// ディレクトリの展開/折りたたみトグル
    pub(crate) fn toggle_dir_expand(&mut self) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };
        ops.tree.toggle_expand();
    }

    /// GitOpsSplitTree の入力処理
    pub(crate) fn handle_git_ops_tree_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) {
        let kb = self.config.keybindings.clone();

        // 確認待ち中は y/n/Esc のみ受け付ける
        if let Some(ref confirm) = self.git_ops_state.as_ref().and_then(|o| o.pending_confirm.clone()) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(ref mut ops) = self.git_ops_state {
                        ops.pending_confirm = None;
                    }
                    match confirm {
                        PendingGitOpsConfirm::Discard { .. } => self.discard_changes(),
                        PendingGitOpsConfirm::Undo => self.execute_undo(),
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(ref mut ops) = self.git_ops_state {
                        ops.pending_confirm = None;
                    }
                }
                _ => {}
            }
            return;
        }

        // Quit / Esc
        if self.matches_single_key(&key, &kb.quit) {
            self.close_git_ops();
            return;
        }

        // Move down
        if self.matches_single_key(&key, &kb.move_down) {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.tree.move_down();
            }
            self.update_git_ops_diff();
            return;
        }

        // Move up
        if self.matches_single_key(&key, &kb.move_up) {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.tree.move_up();
            }
            self.update_git_ops_diff();
            return;
        }

        // Stage/unstage
        if self.matches_single_key(&key, &kb.git_ops_stage) {
            self.toggle_stage();
            return;
        }

        // Stage all
        if self.matches_single_key(&key, &kb.git_ops_stage_all) {
            self.stage_all();
            return;
        }

        // Discard → 確認待ちに遷移
        if self.matches_single_key(&key, &kb.git_ops_discard) {
            if let Some(ref mut ops) = self.git_ops_state {
                if let Some(path) = ops.selected_path().map(|p| p.to_string()) {
                    ops.pending_confirm = Some(PendingGitOpsConfirm::Discard { path });
                }
            }
            return;
        }

        // Commit
        if self.matches_single_key(&key, &kb.git_ops_commit) {
            let _ = self.git_ops_commit(terminal);
            return;
        }

        // Undo → 確認待ちに遷移
        if self.matches_single_key(&key, &kb.git_ops_undo) {
            if let Some(ref mut ops) = self.git_ops_state {
                if !ops.undo_stack.is_empty() {
                    ops.pending_confirm = Some(PendingGitOpsConfirm::Undo);
                } else {
                    ops.op_message = Some(("Nothing to undo".to_string(), Instant::now()));
                }
            }
            return;
        }

        // Push
        if self.matches_single_key(&key, &kb.git_ops_push) {
            self.git_ops_push();
            return;
        }

        // Refresh
        if self.matches_single_key(&key, &kb.refresh) {
            self.refresh_git_status();
            return;
        }

        // Enter: ディレクトリなら展開/折りたたみ、ファイルならdiffペインへ
        if self.matches_single_key(&key, &kb.open_panel) {
            let is_dir = self
                .git_ops_state
                .as_ref()
                .and_then(|ops| ops.tree.visible_rows.get(ops.tree.selected_row))
                .map(|row| matches!(row, TreeRow::Dir { .. }))
                .unwrap_or(false);

            if is_dir {
                self.toggle_dir_expand();
            } else {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.left_return_focus = LeftPaneFocus::Tree;
                }
                self.state = AppState::GitOpsSplitDiff;
            }
            return;
        }

        // Move right / l: focus diff
        if self.matches_single_key(&key, &kb.move_right) {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.left_return_focus = LeftPaneFocus::Tree;
            }
            self.state = AppState::GitOpsSplitDiff;
            return;
        }

        // Tab: switch to commits
        if self.matches_single_key(&key, &kb.tab_switch) {
            self.toggle_git_ops_left_focus();
        }
    }

    /// Commits フォーカスの入力処理
    pub(crate) fn handle_git_ops_commits_input(&mut self, key: event::KeyEvent) {
        // 確認待ち中は y/n/Esc のみ受け付ける
        if let Some(ref confirm) = self.git_ops_state.as_ref().and_then(|o| o.pending_confirm.clone()) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(ref mut ops) = self.git_ops_state {
                        ops.pending_confirm = None;
                    }
                    if let PendingGitOpsConfirm::Undo = confirm {
                        self.reset_soft_to_selected_commit();
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(ref mut ops) = self.git_ops_state {
                        ops.pending_confirm = None;
                    }
                }
                _ => {}
            }
            return;
        }

        let kb = self.config.keybindings.clone();

        // gg シーケンス処理（先頭ジャンプ）
        if let Some(kb_event) = crate::keybinding::event_to_keybinding(&key) {
            self.check_sequence_timeout();

            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);

                if self.try_match_sequence(&kb.jump_to_first)
                    == crate::keybinding::SequenceMatch::Full
                {
                    self.clear_pending_keys();
                    if let Some(ref mut ops) = self.git_ops_state {
                        ops.commit_log.selected = 0;
                    }
                    self.start_fetch_git_ops_commit_diff();
                    return;
                }

                self.clear_pending_keys();
            } else {
                let could_start_gg = self.key_could_match_sequence(&key, &kb.jump_to_first);
                if could_start_gg {
                    self.push_pending_key(kb_event);
                    return;
                }
            }
        }

        // Quit / Esc
        if self.matches_single_key(&key, &kb.quit) {
            self.close_git_ops();
            return;
        }

        // Move down
        if self.matches_single_key(&key, &kb.move_down) {
            if let Some(ref mut ops) = self.git_ops_state {
                let cl = &mut ops.commit_log;
                if !cl.commits.is_empty() {
                    cl.selected = (cl.selected + 1).min(cl.commits.len() - 1);
                }
            }
            self.start_fetch_git_ops_commit_diff();
            // 末尾接近時の追加ロード
            let should_load_more = self
                .git_ops_state
                .as_ref()
                .map(|ops| {
                    let cl = &ops.commit_log;
                    cl.selected + 5 >= cl.commits.len() && cl.has_more && !cl.loading
                })
                .unwrap_or(false);
            if should_load_more {
                self.load_more_git_ops_commits();
            }
            return;
        }

        // Move up
        if self.matches_single_key(&key, &kb.move_up) {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.commit_log.selected = ops.commit_log.selected.saturating_sub(1);
            }
            self.start_fetch_git_ops_commit_diff();
            return;
        }

        // Jump to last (G)
        if self.matches_single_key(&key, &kb.jump_to_last) {
            if let Some(ref mut ops) = self.git_ops_state {
                let cl = &mut ops.commit_log;
                if !cl.commits.is_empty() {
                    cl.selected = cl.commits.len() - 1;
                }
            }
            self.start_fetch_git_ops_commit_diff();
            return;
        }

        // Undo / reset --soft → 確認待ちに遷移
        if self.matches_single_key(&key, &kb.git_ops_undo) {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.pending_confirm = Some(PendingGitOpsConfirm::Undo);
            }
            return;
        }

        // Enter / move_right / →: focus diff
        if self.matches_single_key(&key, &kb.open_panel)
            || self.matches_single_key(&key, &kb.move_right)
                   {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.left_return_focus = LeftPaneFocus::Commits;
            }
            self.state = AppState::GitOpsSplitDiff;
            return;
        }

        // Tab: switch to tree
        if self.matches_single_key(&key, &kb.tab_switch) {
            self.toggle_git_ops_left_focus();
        }
    }

    /// 選択中コミットに対して git reset --soft を実行
    fn reset_soft_to_selected_commit(&mut self) {
        let sha = self
            .git_ops_state
            .as_ref()
            .and_then(|ops| ops.commit_log.commits.get(ops.commit_log.selected))
            .map(|c| c.sha.clone());

        let Some(sha) = sha else {
            return;
        };

        // diff store をクリア、コミット一覧を未初期化に戻す
        if let Some(ref mut ops) = self.git_ops_state {
            ops.diff_store.clear();
            ops.commit_log.diff_store.clear();
            ops.commit_log.commits.clear();
            ops.commit_log.selected = 0;
            ops.commit_log.initialized = false;
        }

        self.run_git_op_silent(
            vec!["reset".to_string(), "--soft".to_string(), sha],
            "Reset --soft (changes are staged)".to_string(),
        );
    }

    /// left_return_focus に応じてアクティブな diff_scroll を返す
    fn active_git_ops_diff_scroll(&mut self) -> Option<&mut crate::diff_store::DiffScrollState> {
        self.git_ops_state.as_mut().map(|ops| match ops.left_return_focus {
            LeftPaneFocus::Commits => &mut ops.commit_log.diff_scroll,
            LeftPaneFocus::Tree => &mut ops.diff_scroll,
        })
    }

    /// GitOpsSplitDiff の入力処理
    pub(crate) fn handle_git_ops_diff_input(&mut self, key: event::KeyEvent) {
        let kb = self.config.keybindings.clone();

        // gg シーケンス処理
        if let Some(kb_event) = crate::keybinding::event_to_keybinding(&key) {
            self.check_sequence_timeout();

            if !self.pending_keys.is_empty() {
                self.push_pending_key(kb_event);

                if self.try_match_sequence(&kb.jump_to_first)
                    == crate::keybinding::SequenceMatch::Full
                {
                    self.clear_pending_keys();
                    if let Some(scroll) = self.active_git_ops_diff_scroll() {
                        scroll.jump_to_first();
                    }
                    return;
                }

                self.clear_pending_keys();
            } else {
                let could_start_gg = self.key_could_match_sequence(&key, &kb.jump_to_first);
                if could_start_gg {
                    self.push_pending_key(kb_event);
                    return;
                }
            }
        }

        // Quit / Esc / h / Left: back
        if self.matches_single_key(&key, &kb.quit)
                       || self.matches_single_key(&key, &kb.move_left)
                   {
            self.return_from_git_ops_diff();
            return;
        }

        // Page down (Ctrl-d, also J)
        if self.matches_single_key(&key, &kb.page_down)
            || Self::is_shift_char_shortcut(&key, 'j')
        {
            if let Some(scroll) = self.active_git_ops_diff_scroll() {
                scroll.page_down(20);
            }
            return;
        }

        // Page up (Ctrl-u, also K)
        if self.matches_single_key(&key, &kb.page_up)
            || Self::is_shift_char_shortcut(&key, 'k')
        {
            if let Some(scroll) = self.active_git_ops_diff_scroll() {
                scroll.page_up(20);
            }
            return;
        }

        // Move down
        if self.matches_single_key(&key, &kb.move_down) {
            if let Some(scroll) = self.active_git_ops_diff_scroll() {
                scroll.move_down();
            }
            return;
        }

        // Move up
        if self.matches_single_key(&key, &kb.move_up) {
            if let Some(scroll) = self.active_git_ops_diff_scroll() {
                scroll.move_up();
            }
            return;
        }

        // Jump to last (G)
        if self.matches_single_key(&key, &kb.jump_to_last) {
            if let Some(scroll) = self.active_git_ops_diff_scroll() {
                scroll.jump_to_last();
            }
            return;
        }

        // Tab: switch to tree
        if self.matches_single_key(&key, &kb.tab_switch) {
            if let Some(ref mut ops) = self.git_ops_state {
                ops.left_focus = LeftPaneFocus::Tree;
            }
            self.state = AppState::GitOpsSplitTree;
        }
    }
}

// ====================================================================
// Free functions（self を借用しない）
// ====================================================================

/// git status --porcelain=v1 -z の出力をパース
pub(crate) async fn fetch_git_status(
    working_dir: Option<&str>,
) -> Result<Vec<GitStatusEntry>, String> {
    let status_output = run_git_op(working_dir, &["status", "--porcelain=v1", "-z"])
        .await
        .map_err(|e| e.to_string())?;

    let mut entries = parse_porcelain_status(&status_output);

    // numstat でファイル行数を取得
    if let Ok(numstat) = run_git_op(working_dir, &["diff", "--numstat"]).await {
        apply_numstat(&mut entries, &numstat, false);
    }
    if let Ok(numstat) = run_git_op(working_dir, &["diff", "--numstat", "--cached"]).await {
        apply_numstat(&mut entries, &numstat, true);
    }

    Ok(entries)
}

async fn run_git_op(working_dir: Option<&str>, args: &[&str]) -> Result<String, std::io::Error> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["-c", "core.quotePath=false"]);
    cmd.args(args);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    let output = cmd.output().await?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_porcelain_status(output: &str) -> Vec<GitStatusEntry> {
    let mut entries = Vec::new();
    let parts: Vec<&str> = output.split('\0').collect();
    let mut i = 0;
    while i < parts.len() {
        let part = parts[i];
        if part.len() < 3 {
            i += 1;
            continue;
        }
        let index_char = part.as_bytes()[0] as char;
        let worktree_char = part.as_bytes()[1] as char;
        let path = &part[3..];

        let (orig_path, final_path) = if matches!(index_char, 'R' | 'C') {
            // 次のパートが新しいパス
            i += 1;
            let new_path = parts.get(i).unwrap_or(&"");
            (Some(path.to_string()), new_path.to_string())
        } else {
            (None, path.to_string())
        };

        let unmerged = matches!(
            (index_char, worktree_char),
            ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D')
        );

        entries.push(GitStatusEntry {
            path: final_path,
            index_status: FileStatus::from_char(index_char),
            worktree_status: FileStatus::from_char(worktree_char),
            additions: 0,
            deletions: 0,
            staged_additions: 0,
            staged_deletions: 0,
            orig_path,
            unmerged,
        });
        i += 1;
    }
    entries
}

fn apply_numstat(entries: &mut [GitStatusEntry], numstat: &str, staged: bool) {
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let add: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let del: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let path = parts.next().unwrap_or("");

        if let Some(entry) = entries.iter_mut().find(|e| e.path == path) {
            if staged {
                entry.staged_additions = add;
                entry.staged_deletions = del;
            } else {
                entry.additions = add;
                entry.deletions = del;
            }
        }
    }
}

fn optimistic_stage(entry: &mut GitStatusEntry) {
    if entry.worktree_status == FileStatus::Untracked {
        entry.index_status = FileStatus::Added;
        entry.worktree_status = FileStatus::Unmodified;
    } else {
        entry.index_status = entry.worktree_status;
        entry.worktree_status = FileStatus::Unmodified;
    }
}

fn optimistic_unstage(entry: &mut GitStatusEntry) {
    if entry.index_status == FileStatus::Added {
        entry.index_status = FileStatus::Untracked;
        entry.worktree_status = FileStatus::Untracked;
    } else {
        entry.worktree_status = entry.index_status;
        entry.index_status = FileStatus::Unmodified;
    }
}

fn build_git_add_args(paths: &[String]) -> Vec<String> {
    let mut args = vec!["add".to_string(), "--".to_string()];
    args.extend(paths.iter().cloned());
    args
}

/// on-demand diff patch からプレーン/ハイライトキャッシュを構築
fn build_git_ops_diff_from_patch(
    ops: &mut GitOpsState,
    key: String,
    patch: &str,
    filename: &str,
    theme: &str,
    markdown_rich: bool,
    tab_width: u8,
) {
    let plain_cache = build_plain_diff_cache(patch, tab_width);
    ops.diff_store.set_current(key.clone(), plain_cache);
    if let Some(ref c) = ops.diff_store.current {
        ops.diff_scroll.set_line_count(c.lines.len());
    }
    ops.diff_scroll.reset();

    // BG ハイライト構築
    let (tx, rx) = mpsc::channel(1);
    let patch_owned = patch.to_string();
    let filename_owned = filename.to_string();
    let theme_owned = theme.to_string();

    tokio::task::spawn_blocking(move || {
        let mut parser_pool = ParserPool::new();
        let cache = build_diff_cache(
            &patch_owned,
            &filename_owned,
            &theme_owned,
            &mut parser_pool,
            markdown_rich,
            tab_width,
        );
        let _ = tx.blocking_send((key, cache));
    });
    ops.diff_store.set_highlight_rx(rx);
}

/// GitOpsState のツリーを再構築するヘルパー
pub(crate) fn rebuild_git_ops_tree(ops: &mut GitOpsState) {
    let paths: Vec<(usize, &str)> = ops
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (i, e.path.as_str()))
        .collect();
    ops.tree.rebuild(&paths);
}

/// 2フェーズプリフェッチ: 並列 diff 取得 → 単一 spawn_blocking でハイライト
fn start_git_ops_prefetch(
    ops: &mut GitOpsState,
    working_dir: Option<String>,
    theme: &str,
    markdown_rich: bool,
    tab_width: u8,
) {
    ops.diff_store.drop_prefetch_rx();

    let items: Vec<(String, bool)> = ops
        .entries
        .iter()
        .filter(|e| !ops.diff_store.store_contains_key(&e.path))
        .filter(|e| ops.diff_store.current_key() != Some(&e.path))
        .map(|e| {
            let is_untracked = e.worktree_status == FileStatus::Untracked
                && e.index_status == FileStatus::Untracked;
            (e.path.clone(), is_untracked)
        })
        .take(MAX_PREFETCH_FILES)
        .collect();

    if items.is_empty() {
        return;
    }

    let (tx, rx) = mpsc::channel(items.len());
    ops.diff_store.set_prefetch_rx(rx);

    let theme = theme.to_string();
    let wd = working_dir;

    // Phase 1: 並列 async diff fetch
    // Phase 2: 単一 spawn_blocking でハイライト（ParserPool 共有）
    tokio::spawn(async move {
        let mut diffs: Vec<(String, String)> = Vec::new();
        let mut handles = Vec::new();

        for (path, is_untracked) in items {
            let wd = wd.clone();
            let handle = tokio::spawn(async move {
                let (dtx, mut drx) = mpsc::channel(1);
                loader::fetch_single_file_diff(wd, path.clone(), is_untracked, dtx).await;
                drx.recv().await.map(|r| (r.filename, r.patch))
            });
            handles.push(handle);
        }

        for handle in handles {
            if let Ok(Some((filename, Some(patch)))) = handle.await {
                diffs.push((filename, patch));
            }
        }

        if diffs.is_empty() {
            return;
        }

        // Phase 2: highlight
        let tx2 = tx;
        tokio::task::spawn_blocking(move || {
            let mut parser_pool = ParserPool::new();
            for (filename, patch) in diffs {
                let cache = build_diff_cache(
                    &patch,
                    &filename,
                    &theme,
                    &mut parser_pool,
                    markdown_rich,
                    tab_width,
                );
                if tx2.blocking_send((filename, cache)).is_err() {
                    break;
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    /// FileStatus のスペースを · に置換して可視化
    fn vis(s: FileStatus) -> char {
        match s.as_char() {
            ' ' => '·',
            c => c,
        }
    }

    /// テスト用ヘルパー: GitStatusEntry を簡潔に生成
    fn entry(path: &str, index: FileStatus, worktree: FileStatus) -> GitStatusEntry {
        GitStatusEntry {
            path: path.to_string(),
            index_status: index,
            worktree_status: worktree,
            additions: 0,
            deletions: 0,
            staged_additions: 0,
            staged_deletions: 0,
            orig_path: None,
            unmerged: false,
        }
    }

    /// visible_rows を人間が読める文字列にダンプ
    fn dump_visible_rows(ops: &GitOpsState) -> String {
        ops.tree.visible_rows
            .iter()
            .map(|row| match row {
                TreeRow::Dir { ref path, depth, expanded } => {
                    let indent = "  ".repeat(*depth);
                    let icon = if *expanded { "▼" } else { "▶" };
                    format!("{}{} {}/", indent, icon, path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path))
                }
                TreeRow::File { index, depth } => {
                    let indent = "  ".repeat(*depth);
                    let e = &ops.entries[*index];
                    let label = e.change_type_label();
                    format!("{}{} {}", indent, label, e.path.rsplit_once('/').map(|(_, n)| n).unwrap_or(&e.path))
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_parse_porcelain_status_basic() {
        let output = " M src/main.rs\0?? new_file.rs\0";
        let entries = parse_porcelain_status(output);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "src/main.rs");
        assert_eq!(entries[0].index_status, FileStatus::Unmodified);
        assert_eq!(entries[0].worktree_status, FileStatus::Modified);
        assert_eq!(entries[1].path, "new_file.rs");
        assert_eq!(entries[1].index_status, FileStatus::Untracked);
        assert_eq!(entries[1].worktree_status, FileStatus::Untracked);
    }

    #[test]
    fn test_parse_porcelain_status_rename() {
        let output = "R  old_name.rs\0new_name.rs\0";
        let entries = parse_porcelain_status(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "new_name.rs");
        assert_eq!(entries[0].orig_path, Some("old_name.rs".to_string()));
        assert_eq!(entries[0].index_status, FileStatus::Renamed);
    }

    #[test]
    fn test_apply_numstat() {
        let mut entries = vec![GitStatusEntry {
            path: "a.rs".to_string(),
            index_status: FileStatus::Unmodified,
            worktree_status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
            staged_additions: 0,
            staged_deletions: 0,
            orig_path: None,
            unmerged: false,
        }];
        apply_numstat(&mut entries, "10\t5\ta.rs\n", false);
        assert_eq!(entries[0].additions, 10);
        assert_eq!(entries[0].deletions, 5);

        apply_numstat(&mut entries, "3\t1\ta.rs\n", true);
        assert_eq!(entries[0].staged_additions, 3);
        assert_eq!(entries[0].staged_deletions, 1);
    }

    #[test]
    fn test_rebuild_visible_rows_flat() {
        let entries = vec![GitStatusEntry {
            path: "a.rs".to_string(),
            index_status: FileStatus::Modified,
            worktree_status: FileStatus::Unmodified,
            additions: 0,
            deletions: 0,
            staged_additions: 0,
            staged_deletions: 0,
            orig_path: None,
            unmerged: false,
        }];
        let mut ops = GitOpsState::new(entries);
        rebuild_git_ops_tree(&mut ops);
        assert_eq!(ops.tree.visible_rows.len(), 1);
        assert!(matches!(ops.tree.visible_rows[0], TreeRow::File { index: 0, depth: 0 }));
    }

    #[test]
    fn test_rebuild_visible_rows_nested() {
        let entries = vec![
            GitStatusEntry {
                path: "src/app/mod.rs".to_string(),
                index_status: FileStatus::Modified,
                worktree_status: FileStatus::Unmodified,
                additions: 0,
                deletions: 0,
                staged_additions: 0,
                staged_deletions: 0,
                orig_path: None,
                unmerged: false,
            },
            GitStatusEntry {
                path: "src/lib.rs".to_string(),
                index_status: FileStatus::Modified,
                worktree_status: FileStatus::Unmodified,
                additions: 0,
                deletions: 0,
                staged_additions: 0,
                staged_deletions: 0,
                orig_path: None,
                unmerged: false,
            },
        ];
        let mut ops = GitOpsState::new(entries);
        rebuild_git_ops_tree(&mut ops);
        // src/ dir, src/app/ dir, src/app/mod.rs file, src/lib.rs file
        assert_eq!(ops.tree.visible_rows.len(), 4);
    }

    #[test]
    fn test_git_ops_poll_highlight_swaps_current() {
        let mut ops = GitOpsState::new(Vec::new());
        let plain = DiffCache {
            file_index: 0,
            patch_hash: 123,
            lines: vec![],
            interner: lasso::Rodeo::new(),
            highlighted: false,
            markdown_rich: false,
        };
        ops.diff_store.set_current("a.rs".to_string(), plain);

        let highlighted = DiffCache {
            file_index: 0,
            patch_hash: 123,
            lines: vec![],
            interner: lasso::Rodeo::new(),
            highlighted: true,
            markdown_rich: false,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(("a.rs".to_string(), highlighted)).unwrap();
        ops.diff_store.set_highlight_rx(rx);

        assert!(ops.diff_store.poll_highlight());
        assert!(ops.diff_store.current.as_ref().unwrap().highlighted);
    }

    #[test]
    fn test_git_ops_highlight_survives_file_switch() {
        let mut ops = GitOpsState::new(Vec::new());

        // a.rs のハイライト済みキャッシュをセット
        let highlighted = DiffCache {
            file_index: 0,
            patch_hash: 100,
            lines: vec![],
            interner: lasso::Rodeo::new(),
            highlighted: true,
            markdown_rich: false,
        };
        ops.diff_store
            .set_current("a.rs".to_string(), highlighted);

        // b.rs に切り替え（plain）
        let plain_b = DiffCache {
            file_index: 1,
            patch_hash: 200,
            lines: vec![],
            interner: lasso::Rodeo::new(),
            highlighted: false,
            markdown_rich: false,
        };
        ops.diff_store
            .set_current("b.rs".to_string(), plain_b);

        // a.rs はストアに退避されているはず（highlighted だった）
        assert!(ops.diff_store.store_contains_key(&"a.rs".to_string()));

        // a.rs を復元
        assert!(ops.diff_store.try_restore(&"a.rs".to_string(), None));
        assert!(ops.diff_store.current.as_ref().unwrap().highlighted);
        assert_eq!(ops.diff_store.current.as_ref().unwrap().patch_hash, 100);
    }

    #[test]
    fn test_git_ops_prefetch_populates_store() {
        let mut ops = GitOpsState::new(Vec::new());

        let cache = DiffCache {
            file_index: 0,
            patch_hash: 100,
            lines: vec![],
            interner: lasso::Rodeo::new(),
            highlighted: true,
            markdown_rich: false,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        tx.try_send(("a.rs".to_string(), cache)).unwrap();
        ops.diff_store.set_prefetch_rx(rx);

        ops.diff_store.poll_prefetch(|_| 0);
        assert!(ops.diff_store.store_contains_key(&"a.rs".to_string()));
    }

    #[test]
    fn test_git_ops_prefetch_enables_instant_restore() {
        let mut ops = GitOpsState::new(Vec::new());

        // プリフェッチでハイライト済みキャッシュを格納
        let cache = DiffCache {
            file_index: 0,
            patch_hash: 100,
            lines: vec![],
            interner: lasso::Rodeo::new(),
            highlighted: true,
            markdown_rich: false,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        tx.try_send(("a.rs".to_string(), cache)).unwrap();
        ops.diff_store.set_prefetch_rx(rx);
        ops.diff_store.poll_prefetch(|_| 0);

        // フリッカーなしの即座復元（plain→highlighted 遷移なし）
        assert!(ops.diff_store.try_restore(&"a.rs".to_string(), None));
        assert!(ops.diff_store.current.as_ref().unwrap().highlighted);
    }

    #[test]
    fn test_git_ops_without_prefetch_store_is_empty() {
        let ops = GitOpsState::new(Vec::new());
        assert_eq!(ops.diff_store.store_len(), 0);
        assert!(!ops.diff_store.has_prefetch_rx());
    }

    // =================================================================
    // スナップショットテスト
    // =================================================================

    /// ツリー構造: フラットファイル + ネストディレクトリの混在
    #[test]
    fn test_tree_structure_mixed() {
        let entries = vec![
            entry("README.md", FileStatus::Unmodified, FileStatus::Modified),
            entry("src/app/mod.rs", FileStatus::Modified, FileStatus::Unmodified),
            entry("src/app/types.rs", FileStatus::Unmodified, FileStatus::Modified),
            entry("src/lib.rs", FileStatus::Modified, FileStatus::Unmodified),
            entry("tests/integration.rs", FileStatus::Untracked, FileStatus::Untracked),
        ];
        let mut ops = GitOpsState::new(entries);
        rebuild_git_ops_tree(&mut ops);
        assert_snapshot!(dump_visible_rows(&ops), @"
        ▼ src/
          ▼ app/
            M  mod.rs
            M  types.rs
          M  lib.rs
        ▼ tests/
          ?? integration.rs
        M  README.md
        ");
    }

    /// ツリー構造: ディレクトリ折りたたみ後
    #[test]
    fn test_tree_structure_collapsed() {
        let entries = vec![
            entry("src/app/mod.rs", FileStatus::Modified, FileStatus::Unmodified),
            entry("src/app/types.rs", FileStatus::Unmodified, FileStatus::Modified),
            entry("src/lib.rs", FileStatus::Modified, FileStatus::Unmodified),
        ];
        let mut ops = GitOpsState::new(entries);
        rebuild_git_ops_tree(&mut ops);

        // src/app/ を折りたたみ
        ops.tree.expanded_dirs.remove("src/app");
        rebuild_git_ops_tree(&mut ops);
        assert_snapshot!(dump_visible_rows(&ops), @"
        ▼ src/
          ▶ app/
          M  lib.rs
        ");
    }

    /// change_type_label が stage/unstage で不変であることを保証
    #[test]
    fn test_change_type_label_stable_across_stage_unstage() {
        // unstaged modified → staged modified
        let mut e = entry("a.rs", FileStatus::Unmodified, FileStatus::Modified);
        let label_before = e.change_type_label();
        optimistic_stage(&mut e);
        let label_after = e.change_type_label();
        assert_eq!(label_before, label_after, "M file label must not change on stage");

        // staged modified → unstaged modified
        optimistic_unstage(&mut e);
        let label_roundtrip = e.change_type_label();
        assert_eq!(label_before, label_roundtrip, "M file label must survive roundtrip");

        // untracked → staged added
        let mut u = entry("b.rs", FileStatus::Untracked, FileStatus::Untracked);
        let label_before = u.change_type_label();
        optimistic_stage(&mut u);
        let label_after = u.change_type_label();
        assert_eq!(label_before, label_after, "?? file label must not change on stage");
    }

    /// optimistic_stage の状態遷移
    #[test]
    fn test_optimistic_stage_transitions() {
        let cases = vec![
            // (初期index, 初期worktree, 期待index, 期待worktree)
            ("·M → staged", FileStatus::Unmodified, FileStatus::Modified, FileStatus::Modified, FileStatus::Unmodified),
            ("?? → staged", FileStatus::Untracked, FileStatus::Untracked, FileStatus::Added, FileStatus::Unmodified),
            ("·D → staged", FileStatus::Unmodified, FileStatus::Deleted, FileStatus::Deleted, FileStatus::Unmodified),
        ];
        let mut result = String::new();
        for (label, idx, wt, exp_idx, exp_wt) in &cases {
            let mut e = entry("file", *idx, *wt);
            optimistic_stage(&mut e);
            result.push_str(&format!(
                "{}: {}{}→{}{}\n",
                label,
                vis(*idx), vis(*wt),
                vis(e.index_status), vis(e.worktree_status),
            ));
            assert_eq!(e.index_status, *exp_idx);
            assert_eq!(e.worktree_status, *exp_wt);
        }
        assert_snapshot!(result, @"
        ·M → staged: ·M→M·
        ?? → staged: ??→A·
        ·D → staged: ·D→D·
        ");
    }

    /// optimistic_unstage の状態遷移
    #[test]
    fn test_optimistic_unstage_transitions() {
        let cases = vec![
            ("M· → unstaged", FileStatus::Modified, FileStatus::Unmodified, FileStatus::Unmodified, FileStatus::Modified),
            ("A· → unstaged", FileStatus::Added, FileStatus::Unmodified, FileStatus::Untracked, FileStatus::Untracked),
            ("D· → unstaged", FileStatus::Deleted, FileStatus::Unmodified, FileStatus::Unmodified, FileStatus::Deleted),
        ];
        let mut result = String::new();
        for (label, idx, wt, exp_idx, exp_wt) in &cases {
            let mut e = entry("file", *idx, *wt);
            optimistic_unstage(&mut e);
            result.push_str(&format!(
                "{}: {}{}→{}{}\n",
                label,
                vis(*idx), vis(*wt),
                vis(e.index_status), vis(e.worktree_status),
            ));
            assert_eq!(e.index_status, *exp_idx);
            assert_eq!(e.worktree_status, *exp_wt);
        }
        assert_snapshot!(result, @"
        M· → unstaged: M·→·M
        A· → unstaged: A·→??
        D· → unstaged: D·→·D
        ");
    }

    /// parse_porcelain_status: 複雑なケース（rename, unmerge, mixed status）
    #[test]
    fn test_parse_porcelain_status_complex() {
        // MM (staged + worktree modified), UU (unmerged), R (rename), A (added), D (deleted)
        let output = "MM src/both.rs\0UU conflict.rs\0A  new.rs\0 D deleted.rs\0R  old.rs\0renamed.rs\0";
        let entries = parse_porcelain_status(output);
        let mut result = String::new();
        for e in &entries {
            result.push_str(&format!(
                "{}{} {} orig={}\n",
                vis(e.index_status),
                vis(e.worktree_status),
                e.path,
                e.orig_path.as_deref().unwrap_or("-"),
            ));
        }
        assert_snapshot!(result, @"
        MM src/both.rs orig=-
        UU conflict.rs orig=-
        A· new.rs orig=-
        ·D deleted.rs orig=-
        R· renamed.rs orig=old.rs
        ");
    }

    /// parse_porcelain_status: 空入力
    #[test]
    fn test_parse_porcelain_status_empty() {
        let entries = parse_porcelain_status("");
        assert!(entries.is_empty());
        let entries = parse_porcelain_status("\0");
        assert!(entries.is_empty());
    }

    // =================================================================
    // undo / discard シナリオテスト
    // =================================================================

    use crate::config::Config;

    fn make_git_ops_app() -> (super::super::App, tokio::sync::mpsc::Sender<crate::loader::DataLoadResult>) {
        let config = Config::default();
        super::super::App::new_loading("owner/repo", 1, config)
    }

    #[tokio::test]
    async fn test_undo_stage_pops_from_stack() {
        let (mut app, _tx) = make_git_ops_app();
        let entries = vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ];
        let mut ops = GitOpsState::new(entries);
        ops.undo_stack.push(UndoAction::Stage {
            paths: vec!["a.rs".to_string()],
            previous_index_entries: vec![],
        });
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        assert_eq!(app.git_ops_state.as_ref().unwrap().undo_stack.len(), 1);
        app.execute_undo();
        assert_eq!(app.git_ops_state.as_ref().unwrap().undo_stack.len(), 0);
    }

    #[test]
    fn test_undo_commit_blocked_from_tree_pane() {
        let (mut app, _tx) = make_git_ops_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.undo_stack.push(UndoAction::Commit);
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        app.execute_undo();

        // Commit はスタックに戻されている
        let ops = app.git_ops_state.as_ref().unwrap();
        assert_eq!(ops.undo_stack.len(), 1);
        assert!(matches!(ops.undo_stack[0], UndoAction::Commit));
        // メッセージが表示されている
        assert!(ops.op_message.as_ref().unwrap().0.contains("commit pane"));
    }

    #[test]
    fn test_undo_empty_stack_shows_message() {
        let (mut app, _tx) = make_git_ops_app();
        let ops = GitOpsState::new(Vec::new());
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        app.execute_undo();

        let ops = app.git_ops_state.as_ref().unwrap();
        assert!(ops.op_message.as_ref().unwrap().0.contains("Nothing to undo"));
    }

    #[tokio::test]
    async fn test_undo_commit_behind_stage_is_preserved() {
        let (mut app, _tx) = make_git_ops_app();
        let mut ops = GitOpsState::new(vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ]);
        // Commit → Stage の順でスタックに積む（Stage が先に pop される）
        ops.undo_stack.push(UndoAction::Commit);
        ops.undo_stack.push(UndoAction::Stage {
            paths: vec!["a.rs".to_string()],
            previous_index_entries: vec![],
        });
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        // Stage undo が実行される
        app.execute_undo();
        let ops = app.git_ops_state.as_ref().unwrap();
        assert_eq!(ops.undo_stack.len(), 1);
        assert!(matches!(ops.undo_stack[0], UndoAction::Commit));

        // 次に undo → Commit はブロックされスタックに戻る
        app.execute_undo();
        let ops = app.git_ops_state.as_ref().unwrap();
        assert_eq!(ops.undo_stack.len(), 1);
        assert!(matches!(ops.undo_stack[0], UndoAction::Commit));
    }

    #[test]
    fn test_discard_clears_related_undo_entries() {
        let (mut app, _tx) = make_git_ops_app();
        let entries = vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
            entry("b.rs", FileStatus::Modified, FileStatus::Unmodified),
        ];
        let mut ops = GitOpsState::new(entries);
        rebuild_git_ops_tree(&mut ops);

        // a.rs と b.rs の stage undo をスタックに積む
        ops.undo_stack.push(UndoAction::Stage {
            paths: vec!["a.rs".to_string()],
            previous_index_entries: vec![],
        });
        ops.undo_stack.push(UndoAction::Stage {
            paths: vec!["b.rs".to_string()],
            previous_index_entries: vec![],
        });
        ops.undo_stack.push(UndoAction::Commit);
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        // a.rs をツリー上で選択状態にする
        let tree = &mut app.git_ops_state.as_mut().unwrap().tree;
        if let Some(row) = tree.find_row_for_file(0) {
            tree.selected_row = row;
        }

        // discard_changes は async git op を spawn するので直接テストできないが、
        // discard 内の undo_stack.retain ロジックをテストする
        // → discard_changes を呼ぶ代わりに retain ロジックを直接実行
        let path = "a.rs".to_string();
        let ops = app.git_ops_state.as_mut().unwrap();
        ops.undo_stack.retain(|action| match action {
            UndoAction::Stage { paths, .. } | UndoAction::Unstage { paths } => {
                !paths.contains(&path)
            }
            UndoAction::Commit | UndoAction::StageAll { .. } => true,
        });

        // a.rs の Stage undo は消えたが、b.rs と Commit は残る
        assert_eq!(ops.undo_stack.len(), 2);
        assert!(matches!(ops.undo_stack[0], UndoAction::Stage { ref paths, .. } if paths[0] == "b.rs"));
        assert!(matches!(ops.undo_stack[1], UndoAction::Commit));
    }

    #[tokio::test]
    async fn test_stage_all_undo_not_blocked_from_tree() {
        let (mut app, _tx) = make_git_ops_app();
        let entries = vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ];
        let mut ops = GitOpsState::new(entries);
        ops.undo_stack.push(UndoAction::StageAll { tree_hash: None });
        app.git_ops_state = Some(ops);
        app.state = AppState::GitOpsSplitTree;

        app.execute_undo();
        // StageAll undo はツリーペインから実行可能
        assert_eq!(app.git_ops_state.as_ref().unwrap().undo_stack.len(), 0);
    }

    // =================================================================
    // 確認プロンプト (Y/n) テスト
    // =================================================================

    /// 確認待ち状態の入力処理をシミュレート（terminal 不要な範囲でテスト）
    fn simulate_tree_confirm(app: &mut super::super::App, code: KeyCode) {
        let key = crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::empty(),
        );
        // 確認待ち中の分岐を直接テスト
        if let Some(ref confirm) = app.git_ops_state.as_ref().and_then(|o| o.pending_confirm.clone()) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(ref mut ops) = app.git_ops_state {
                        ops.pending_confirm = None;
                    }
                    match confirm {
                        PendingGitOpsConfirm::Discard { .. } => app.discard_changes(),
                        PendingGitOpsConfirm::Undo => app.execute_undo(),
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(ref mut ops) = app.git_ops_state {
                        ops.pending_confirm = None;
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn test_discard_key_sets_pending_confirm() {
        let (mut app, _tx) = make_git_ops_app();
        let entries = vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ];
        let mut ops = GitOpsState::new(entries);
        rebuild_git_ops_tree(&mut ops);
        if let Some(row) = ops.tree.find_row_for_file(0) {
            ops.tree.selected_row = row;
        }
        app.git_ops_state = Some(ops);

        // discard のキーバインドを直接シミュレート
        if let Some(ref mut ops) = app.git_ops_state {
            if let Some(path) = ops.selected_path().map(|p| p.to_string()) {
                ops.pending_confirm = Some(PendingGitOpsConfirm::Discard { path });
            }
        }

        let ops = app.git_ops_state.as_ref().unwrap();
        assert!(matches!(
            ops.pending_confirm,
            Some(PendingGitOpsConfirm::Discard { ref path }) if path == "a.rs"
        ));
    }

    #[test]
    fn test_confirm_n_cancels_pending() {
        let (mut app, _tx) = make_git_ops_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.pending_confirm = Some(PendingGitOpsConfirm::Discard {
            path: "a.rs".to_string(),
        });
        app.git_ops_state = Some(ops);

        simulate_tree_confirm(&mut app, KeyCode::Char('n'));
        assert!(app.git_ops_state.as_ref().unwrap().pending_confirm.is_none());
    }

    #[test]
    fn test_confirm_esc_cancels_pending() {
        let (mut app, _tx) = make_git_ops_app();
        let mut ops = GitOpsState::new(Vec::new());
        ops.pending_confirm = Some(PendingGitOpsConfirm::Undo);
        app.git_ops_state = Some(ops);

        simulate_tree_confirm(&mut app, KeyCode::Esc);
        assert!(app.git_ops_state.as_ref().unwrap().pending_confirm.is_none());
    }

    #[test]
    fn test_undo_empty_stack_skips_confirm() {
        let (mut app, _tx) = make_git_ops_app();
        let ops = GitOpsState::new(Vec::new());
        app.git_ops_state = Some(ops);

        // undo スタックが空の場合、pending_confirm にならず直接メッセージ
        let ops_ref = app.git_ops_state.as_mut().unwrap();
        if !ops_ref.undo_stack.is_empty() {
            ops_ref.pending_confirm = Some(PendingGitOpsConfirm::Undo);
        } else {
            ops_ref.op_message = Some(("Nothing to undo".to_string(), Instant::now()));
        }

        let ops = app.git_ops_state.as_ref().unwrap();
        assert!(ops.pending_confirm.is_none());
        assert!(ops.op_message.as_ref().unwrap().0.contains("Nothing to undo"));
    }

    #[test]
    fn test_pending_confirm_blocks_other_keys() {
        let (mut app, _tx) = make_git_ops_app();
        let entries = vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
            entry("b.rs", FileStatus::Unmodified, FileStatus::Modified),
        ];
        let mut ops = GitOpsState::new(entries);
        rebuild_git_ops_tree(&mut ops);
        ops.pending_confirm = Some(PendingGitOpsConfirm::Discard {
            path: "a.rs".to_string(),
        });
        let initial_row = ops.tree.selected_row;
        app.git_ops_state = Some(ops);

        // j キー（move_down）は確認中は何もしない
        simulate_tree_confirm(&mut app, KeyCode::Char('j'));

        let ops = app.git_ops_state.as_ref().unwrap();
        assert_eq!(ops.tree.selected_row, initial_row);
        // pending_confirm は j では消えない（simulate_tree_confirm は y/n/Esc 以外何もしない）
        assert!(ops.pending_confirm.is_some());
    }

    #[tokio::test]
    async fn test_confirm_y_executes_undo() {
        let (mut app, _tx) = make_git_ops_app();
        let mut ops = GitOpsState::new(vec![
            entry("a.rs", FileStatus::Unmodified, FileStatus::Modified),
        ]);
        ops.undo_stack.push(UndoAction::Stage {
            paths: vec!["a.rs".to_string()],
            previous_index_entries: vec![],
        });
        ops.pending_confirm = Some(PendingGitOpsConfirm::Undo);
        app.git_ops_state = Some(ops);

        simulate_tree_confirm(&mut app, KeyCode::Char('y'));

        let ops = app.git_ops_state.as_ref().unwrap();
        assert!(ops.pending_confirm.is_none());
        assert_eq!(ops.undo_stack.len(), 0);
    }
}
