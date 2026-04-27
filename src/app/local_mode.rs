use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use notify::Watcher;

use crate::cache::PrCacheKey;

use super::types::*;
use super::App;

impl App {
    pub(crate) fn toggle_markdown_rich(&mut self) {
        self.markdown_rich = !self.markdown_rich;

        // 現在のファイルがmarkdownならキャッシュを無効化
        let current_is_md = self
            .files()
            .get(self.selected_file)
            .map(|f| crate::language::is_markdown_ext_from_filename(&f.filename))
            .unwrap_or(false);

        if current_is_md {
            self.diff_store.clear_current();
        }

        // ストア内のmarkdownファイルのキャッシュのみ無効化
        let markdown_rich = self.markdown_rich;
        self.diff_store
            .invalidate_if(|_k, cache| cache.markdown_rich != markdown_rich);

        self.pr_description_cache = None;

        // プリフェッチも停止（markdown_richフラグが変わったため再構築が必要）
        self.diff_store.drop_prefetch_rx();
    }

    /// L キー: 現在のモードを閉じ、反対のモードをクリーン起動する
    ///
    /// スナップショットの保存・復元は行わない。
    /// 毎回クリーンな状態で起動するため、状態の不整合が発生しない。
    pub(crate) fn toggle_local_mode(&mut self) {
        // フォアグラウンド Rally 中はブロック
        if matches!(self.state, AppState::AiRally) {
            self.cmt.submission_result =
                Some((false, "Cannot toggle mode during AI Rally".to_string()));
            self.cmt.submission_result_time = Some(Instant::now());
            return;
        }

        if self.local_mode {
            // Local → PR: 切替可能かチェック（reset_view_state の前）
            let has_valid_repo = self.repo.contains('/');
            let has_original_pr = self.original_pr_number.filter(|&n| n != 0).is_some();

            if !has_valid_repo && !has_original_pr {
                // repo がダミー値（"local" 等）で復帰先PRもない → 切替不可
                self.cmt.submission_result = Some((false, "No PR to return to".to_string()));
                self.cmt.submission_result_time = Some(Instant::now());
                return;
            }

            self.deactivate_watcher();
            self.reset_view_state();
            self.local_mode = false;

            if let Some(pr) = self.original_pr_number.filter(|&n| n != 0) {
                // CLI --pr 指定あり → そのPRを開く
                self.pr_number = Some(pr);
                self.state = AppState::FileList;
                self.update_data_receiver_origin(pr);
                self.restore_data_from_cache();
            } else {
                // PR指定なし → PR一覧を開く
                self.pr_number = None;
                self.state = AppState::PullRequestList;
                self.started_from_pr_list = true;
                self.data_state = DataState::Loading;
                self.reload_pr_list();
            }

            self.cmt.submission_result = Some((true, "Switched to PR mode".to_string()));
        } else {
            // PR → Local: PR状態を破棄 → Local モードをクリーン起動
            self.reset_view_state();
            self.local_mode = true;
            self.pr_number = Some(0);
            self.state = AppState::FileList;
            self.update_data_receiver_origin(0);

            // SessionCache にローカルデータがあれば復元
            let cache_key = PrCacheKey {
                repo: self.repo.clone(),
                pr_number: 0,
            };
            if let Some(cached) = self.session_cache.get_pr_data(&cache_key) {
                self.data_state = DataState::Loaded {
                    pr: cached.pr.clone(),
                    files: cached.files.clone(),
                };
                self.diff_scroll
                    .set_line_count(Self::calc_diff_line_count(&cached.files, 0));
                self.start_prefetch_all_files();
            } else {
                self.data_state = DataState::Loading;
            }

            self.activate_watcher();
            self.retry_load();

            self.cmt.submission_result = Some((true, "Switched to Local mode".to_string()));
        }

        self.cmt.submission_result_time = Some(Instant::now());
    }

    /// ビュー状態を全リセット（モード切替の共通前処理）
    fn reset_view_state(&mut self) {
        self.selected_file = 0;
        self.file_list_scroll_offset = 0;
        self.diff_scroll.reset();
        self.diff_store.clear();
        self.file_list_filter = None;
        self.cmt.review_comments = None;
        self.cmt.local_comment_meta.clear();
        self.cmt.discussion_comments = None;
        self.cmt.comment_receiver = None;
        self.cmt.discussion_comment_receiver = None;
        self.cmt.comment_submit_receiver = None;
        self.mark_viewed_receiver = None;
        self.batch_diff_receiver = None;
        self.lazy_diff_receiver = None;
        self.lazy_diff_pending_file = None;
        self.cmt.comment_submitting = false;
        self.cmt.comments_loading = false;
        self.cmt.discussion_comments_loading = false;
    }

    /// data_receiver の origin_pr を更新（channel 自体は再作成しない）
    pub(crate) fn update_data_receiver_origin(&mut self, pr_number: u32) {
        if let Some((ref mut origin, _)) = self.data_receiver {
            *origin = pr_number;
        }
    }

    /// SessionCache からデータを復元し、ない場合は Loading + retry_load
    pub(crate) fn restore_data_from_cache(&mut self) {
        let pr_number = self.pr_number.unwrap_or(0);
        let cache_key = PrCacheKey {
            repo: self.repo.clone(),
            pr_number,
        };
        if let Some(cached) = self.session_cache.get_pr_data(&cache_key) {
            self.data_state = DataState::Loaded {
                pr: cached.pr.clone(),
                files: cached.files.clone(),
            };
            self.diff_scroll.line_count = Self::calc_diff_line_count(&cached.files, self.selected_file);
            self.start_prefetch_all_files();
        } else {
            self.data_state = DataState::Loading;
        }
        // 常にバックグラウンドで最新データを取得
        self.retry_load();
    }

    /// ローカルブランチのベースブランチを検出
    pub(crate) fn detect_local_base_branch(working_dir: Option<&str>) -> Option<String> {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["rev-parse", "--abbrev-ref", "@{upstream}"]);
        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }
        if let Ok(output) = cmd.output() {
            if output.status.success() {
                let upstream = String::from_utf8_lossy(&output.stdout).trim().to_string();
                // "origin/main" → "main"
                if let Some(branch) = upstream.strip_prefix("origin/") {
                    return Some(branch.to_string());
                }
                return Some(upstream);
            }
        }

        // Fallback: origin/main or origin/master が存在するか確認
        for candidate in &["main", "master"] {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["rev-parse", "--verify", &format!("origin/{}", candidate)]);
            if let Some(dir) = working_dir {
                cmd.current_dir(dir);
            }
            if let Ok(output) = cmd.output() {
                if output.status.success() {
                    return Some(candidate.to_string());
                }
            }
        }

        None
    }

    /// ファイルウォッチャーを有効化（初回は作成、2回目以降は active フラグを ON）
    pub(crate) fn activate_watcher(&mut self) {
        if let Some(ref handle) = self.watcher_handle {
            handle.active.store(true, Ordering::Release);
            return;
        }

        // retry_sender が必要
        let Some(ref retry_sender) = self.retry_sender else {
            return;
        };

        let refresh_pending = self
            .refresh_pending
            .get_or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone();

        let watch_dir = self.working_dir.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string())
        });

        let active = Arc::new(AtomicBool::new(true));
        let active_clone = active.clone();
        let refresh_tx = retry_sender.clone();

        let thread = std::thread::spawn(move || {
            let callback = move |result: notify::Result<notify::Event>| {
                if !active_clone.load(Ordering::Acquire) {
                    return;
                }

                let Ok(event) = result else {
                    return;
                };

                let dominated_by_git = event
                    .paths
                    .iter()
                    .all(|p| p.components().any(|c| c.as_os_str() == ".git"));
                let is_access = matches!(event.kind, notify::EventKind::Access(_));

                if !is_access && !dominated_by_git && !refresh_pending.swap(true, Ordering::AcqRel)
                {
                    let _ = refresh_tx.try_send(RefreshRequest::LocalRefresh);
                }
            };

            let Ok(mut watcher) =
                notify::RecommendedWatcher::new(callback, notify::Config::default())
            else {
                return;
            };

            let _ = watcher.watch(
                std::path::Path::new(&watch_dir),
                notify::RecursiveMode::Recursive,
            );

            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        });

        self.watcher_handle = Some(WatcherHandle {
            active,
            _thread: thread,
        });
    }

    /// ファイルウォッチャーを無効化（active フラグを OFF）
    pub(crate) fn deactivate_watcher(&mut self) {
        if let Some(ref handle) = self.watcher_handle {
            handle.active.store(false, Ordering::Release);
        }
    }

    pub(crate) fn toggle_auto_focus(&mut self) {
        self.local_auto_focus = !self.local_auto_focus;
        let msg = if self.local_auto_focus {
            "Auto-focus: ON"
        } else {
            "Auto-focus: OFF"
        };
        self.cmt.submission_result = Some((true, msg.to_string()));
        self.cmt.submission_result_time = Some(Instant::now());
    }
}
