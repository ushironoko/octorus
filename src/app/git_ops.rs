use crossterm::event::{self, KeyCode, KeyModifiers};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::diff_store::MAX_PREFETCH_FILES;
use crate::loader;
use crate::syntax::ParserPool;
use crate::ui::diff_view::{build_diff_cache, build_plain_diff_cache};

use super::types::*;
use super::{App, AppState};

impl App {
    /// GitOps 画面を開く
    pub(crate) fn open_git_ops(&mut self) {
        let caller_state = self.state;
        let mut ops = GitOpsState::new(Vec::new());
        ops.return_state = caller_state;
        self.git_ops_state = Some(ops);
        self.state = AppState::GitOpsSplitTree;
        self.refresh_git_status();
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
                        rebuild_visible_rows(ops);
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
            let selected = ops.selected_index;
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

        let selected = ops.visible_rows.get(ops.selected_index);

        match selected {
            Some(TreeRow::File(idx, _)) => {
                let Some(entry) = ops.entries.get(*idx) else {
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
            Some(TreeRow::Dir(dir_path, _, _)) => {
                // 配下の全ファイルを収集
                let prefix = format!("{}/", dir_path);
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

    /// u: undo
    fn execute_undo(&mut self) {
        let action = {
            let Some(ref mut ops) = self.git_ops_state else {
                return;
            };
            match ops.undo_stack.pop() {
                Some(action) => action,
                None => {
                    ops.op_message = Some(("Nothing to undo".to_string(), Instant::now()));
                    return;
                }
            }
        };

        match action {
            UndoAction::Commit => {
                self.run_git_op_silent(
                    vec![
                        "reset".to_string(),
                        "--soft".to_string(),
                        "HEAD~1".to_string(),
                    ],
                    "Undo commit (changes are staged)".to_string(),
                );
                self.retry_load();
            }
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

    /// ディレクトリの展開/折りたたみトグル
    pub(crate) fn toggle_dir_expand(&mut self) {
        let Some(ref mut ops) = self.git_ops_state else {
            return;
        };

        if let Some(TreeRow::Dir(ref path, _, _)) = ops.visible_rows.get(ops.selected_index) {
            let path = path.clone();
            if ops.expanded_dirs.contains(&path) {
                ops.expanded_dirs.remove(&path);
            } else {
                ops.expanded_dirs.insert(path);
            }
            rebuild_visible_rows(ops);
        }
    }

    /// GitOpsSplitTree の入力処理
    pub(crate) fn handle_git_ops_tree_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.close_git_ops();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ref mut ops) = self.git_ops_state {
                    if !ops.visible_rows.is_empty() {
                        ops.selected_index =
                            (ops.selected_index + 1).min(ops.visible_rows.len() - 1);
                    }
                }
                self.update_git_ops_diff();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.selected_index = ops.selected_index.saturating_sub(1);
                }
                self.update_git_ops_diff();
            }
            KeyCode::Char(' ') => {
                // Space: stage/unstage（ディレクトリの場合は配下を一括stage/unstage）
                self.toggle_stage();
            }
            KeyCode::Char('s') => {
                self.stage_all();
            }
            KeyCode::Char('d') => {
                self.discard_changes();
            }
            KeyCode::Char('c') => {
                let _ = self.git_ops_commit(terminal);
            }
            KeyCode::Char('u') => {
                self.execute_undo();
            }
            KeyCode::Char('R') => {
                self.refresh_git_status();
            }
            KeyCode::Enter => {
                // Enter: ディレクトリなら展開/折りたたみ、ファイルならdiffペインへ
                let is_dir = self
                    .git_ops_state
                    .as_ref()
                    .and_then(|ops| ops.visible_rows.get(ops.selected_index))
                    .map(|row| matches!(row, TreeRow::Dir(..)))
                    .unwrap_or(false);

                if is_dir {
                    self.toggle_dir_expand();
                } else {
                    self.state = AppState::GitOpsSplitDiff;
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.state = AppState::GitOpsSplitDiff;
            }
            KeyCode::Tab => {
                if self.state == AppState::GitOpsSplitTree {
                    self.state = AppState::GitOpsSplitDiff;
                } else {
                    self.state = AppState::GitOpsSplitTree;
                }
            }
            _ => {}
        }
    }

    /// GitOpsSplitDiff の入力処理
    pub(crate) fn handle_git_ops_diff_input(&mut self, key: event::KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => {
                self.state = AppState::GitOpsSplitTree;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.diff_scroll.move_down();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.diff_scroll.move_up();
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.diff_scroll.page_down(20);
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.diff_scroll.page_up(20);
                }
            }
            KeyCode::Char('g') => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.diff_scroll.jump_to_first();
                }
            }
            KeyCode::Char('G') => {
                if let Some(ref mut ops) = self.git_ops_state {
                    ops.diff_scroll.jump_to_last();
                }
            }
            KeyCode::Tab => {
                self.state = AppState::GitOpsSplitTree;
            }
            _ => {}
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

/// ツリービューの可視行を再構築
pub(crate) fn rebuild_visible_rows(ops: &mut GitOpsState) {
    ops.visible_rows.clear();

    // ディレクトリパスを収集
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in &ops.entries {
        let parts: Vec<&str> = entry.path.split('/').collect();
        let mut current = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i < parts.len() - 1 {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(part);
                dirs.insert(current.clone());
            }
        }
    }

    // 初回ビルド時は全ディレクトリを展開
    if ops.expanded_dirs.is_empty() && !dirs.is_empty() {
        ops.expanded_dirs = dirs.iter().cloned().collect();
    }

    // ソート済みインデックス
    let mut sorted_indices: Vec<usize> = (0..ops.entries.len()).collect();
    sorted_indices.sort_by(|a, b| ops.entries[*a].path.cmp(&ops.entries[*b].path));

    let mut added_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

    for &idx in &sorted_indices {
        let path = &ops.entries[idx].path;
        let parts: Vec<&str> = path.split('/').collect();

        // 親ディレクトリ行を追加
        let mut current = String::new();
        for (depth, part) in parts.iter().enumerate() {
            if depth < parts.len() - 1 {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(part);

                if !added_dirs.contains(&current) {
                    let parent = if depth == 0 {
                        None
                    } else {
                        current.rsplit_once('/').map(|(p, _)| p.to_string())
                    };

                    let parent_expanded = parent
                        .as_ref()
                        .map(|p| ops.expanded_dirs.contains(p))
                        .unwrap_or(true);

                    if parent_expanded {
                        let is_expanded = ops.expanded_dirs.contains(&current);
                        ops.visible_rows
                            .push(TreeRow::Dir(current.clone(), depth, is_expanded));
                        added_dirs.insert(current.clone());
                    }
                }
            }
        }

        // ファイル行を追加（親ディレクトリが展開中の場合のみ）
        let parent_dir = if parts.len() > 1 {
            Some(parts[..parts.len() - 1].join("/"))
        } else {
            None
        };

        let visible = parent_dir
            .as_ref()
            .map(|p| ops.expanded_dirs.contains(p))
            .unwrap_or(true);

        if visible {
            let depth = parts.len() - 1;
            ops.visible_rows.push(TreeRow::File(idx, depth));
        }
    }

    // selected_index をクランプ
    if !ops.visible_rows.is_empty() && ops.selected_index >= ops.visible_rows.len() {
        ops.selected_index = ops.visible_rows.len() - 1;
    }
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
        rebuild_visible_rows(&mut ops);
        assert_eq!(ops.visible_rows.len(), 1);
        assert!(matches!(ops.visible_rows[0], TreeRow::File(0, 0)));
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
        rebuild_visible_rows(&mut ops);
        // src/ dir, src/app/ dir, src/app/mod.rs file, src/lib.rs file
        assert_eq!(ops.visible_rows.len(), 4);
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
}
