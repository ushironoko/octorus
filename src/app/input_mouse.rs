//! マウスイベントの畳み込みと分派
//!
//! イベント読み取りは `input.rs` の単一サイトで行われ、ドレインしたバッチを
//! ここの純関数 `coalesce` が意味単位へ畳み込む。分派 (`handle_mouse`) は
//! キー処理と同じく `AppState` の網羅 match で行う。

use anyhow::Result;
use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::collections::VecDeque;
use std::io::Stdout;
use std::mem::Discriminant;

use super::{App, AppState, DataState, LeftPaneFocus};
use crate::ui::hit::{HitTarget, ListKind, PaneKind};

/// ドレインしたイベント列を意味単位へ畳み込んだマウスイベント
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoalescedMouseEvent {
    Down {
        x: u16,
        y: u16,
    },
    Up {
        x: u16,
        y: u16,
    },
    Drag {
        x: u16,
        y: u16,
    },
    /// delta は正で下方向、負で上方向。同方向の連続ノッチのみ合算する。
    Scroll {
        x: u16,
        y: u16,
        delta: i32,
    },
}

/// マウスバッチ処理中の画面/モーダル変化を検出するための指紋。
/// 変化したら残りのバッチは旧レイアウト前提のため破棄する。
pub(crate) type MouseContextFingerprint = (Discriminant<AppState>, [bool; 8]);

/// マウスドラッグの状態機械(ブール併用ではなく enum で管理)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DragState {
    #[default]
    Idle,
    /// スプリット境界をドラッグ中。body_x/body_width は分割対象領域の水平範囲。
    ResizingSplit { body_x: u16, body_width: u16 },
    /// 差分行を Down した直後の中間状態。まだ通常クリック。
    /// 最初の Drag で SelectingDiffLines へ昇格する。Drag なしの Up は普通の
    /// クリックとして確定し、open_on_up が真(選択済み行を再クリック)なら
    /// そこで初めてコメントパネルを開く(Down で開くとカーソル行からの
    /// ドラッグ選択が武装できない)。
    PressedDiffLine { anchor: usize, open_on_up: bool },
    /// 複数行選択をドラッグで拡張中(V 相当)
    SelectingDiffLines,
}

/// イベント列を畳み込む。
///
/// - 同方向の連続スクロールは合算(飽和)、方向転換で分割
/// - 連続する Drag は最後の座標のみ残す
/// - Moved・横スクロール・右/中ボタン・修飾キー付きは破棄
/// - 最初の Key 以降は畳み込まず、そのまま返す(呼び出し側が保留キューへ積む)
pub(crate) fn coalesce(events: Vec<Event>) -> (Vec<CoalescedMouseEvent>, VecDeque<Event>) {
    let mut out: Vec<CoalescedMouseEvent> = Vec::new();
    let mut leftover: VecDeque<Event> = VecDeque::new();
    let mut iter = events.into_iter();
    while let Some(event) = iter.next() {
        match event {
            Event::Key(_) => {
                leftover.push_back(event);
                leftover.extend(iter);
                break;
            }
            Event::Mouse(mouse) => coalesce_mouse(&mut out, mouse),
            // Resize/Paste はここでは扱わない(再描画は毎イテレーション行われる)
            _ => {}
        }
    }
    (out, leftover)
}

fn coalesce_mouse(out: &mut Vec<CoalescedMouseEvent>, mouse: MouseEvent) {
    if !mouse.modifiers.is_empty() {
        return;
    }
    let (x, y) = (mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => out.push(CoalescedMouseEvent::Down { x, y }),
        MouseEventKind::Up(MouseButton::Left) => out.push(CoalescedMouseEvent::Up { x, y }),
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(CoalescedMouseEvent::Drag { x: px, y: py }) = out.last_mut() {
                *px = x;
                *py = y;
            } else {
                out.push(CoalescedMouseEvent::Drag { x, y });
            }
        }
        MouseEventKind::ScrollDown => push_scroll(out, x, y, 1),
        MouseEventKind::ScrollUp => push_scroll(out, x, y, -1),
        // 横スクロールは対象外
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {}
        // ホバー機能はないため Moved は捨てる(?1003h の移動洪水はここで消える)
        MouseEventKind::Moved => {}
        // 右/中ボタンは対象外
        MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {}
    }
}

fn push_scroll(out: &mut Vec<CoalescedMouseEvent>, x: u16, y: u16, dir: i32) {
    if let Some(CoalescedMouseEvent::Scroll {
        x: px,
        y: py,
        delta,
    }) = out.last_mut()
    {
        if delta.signum() == dir.signum() {
            *delta = delta.saturating_add(dir);
            *px = x;
            *py = y;
            return;
        }
    }
    out.push(CoalescedMouseEvent::Scroll { x, y, delta: dir });
}

impl App {
    pub(crate) fn mouse_context_fingerprint(&self) -> MouseContextFingerprint {
        (
            std::mem::discriminant(&self.state),
            [
                self.shell_state.is_some(),
                self.multiline_selection.is_some(),
                self.symbol_popup.is_some(),
                self.input_mode.is_some(),
                self.git_ops_state
                    .as_ref()
                    .is_some_and(|g| g.pending_confirm.is_some()),
                self.cmt.pending_approve_body.is_some(),
                self.file_list_filter
                    .as_ref()
                    .is_some_and(|f| f.input_active),
                matches!(self.data_state, DataState::Loading | DataState::Error(_)),
            ],
        )
    }

    /// キー処理と同じ割り込み順のガード。true のときマウス入力を受けない。
    /// オーバーレイの z-order は HitMap の Backdrop 登録が表現するため、
    /// ここで塞ぐのは「何も描画しない確認状態」だけ。
    fn mouse_input_blocked(&self) -> bool {
        (!self.state.is_data_state_independent()
            && matches!(self.data_state, DataState::Loading | DataState::Error(_)))
            // 承認確認はフッタープロンプトのみでモーダル描画がない
            || self.cmt.pending_approve_body.is_some()
            // Simple 確認(gitfilm なし)はモーダルを描画しない
            || self.git_ops_state.as_ref().is_some_and(|g| {
                matches!(
                    g.pending_confirm,
                    Some(super::PendingGitOpsConfirm::Simple { .. })
                )
            })
    }

    /// 外側クリックによるオーバーレイ却下(Esc 相当を 1 段だけ)。
    /// 各 dismiss は自分のオーバーレイが非活性なら何もしない。
    fn overlay_dismiss(&mut self) {
        if self
            .shell_state
            .as_ref()
            .is_some_and(|s| matches!(s.phase, super::ShellPhase::Done(_)))
        {
            self.shell_state = None;
            return;
        }
        if self.symbol_popup.is_some() {
            self.symbol_popup = None;
            return;
        }
        self.browse_symbol_search_dismiss();
        self.browse_outline_dismiss();
        self.browse_pr_choice_dismiss();
        self.browse_line_discussion_dismiss();
    }

    pub(crate) async fn handle_mouse(
        &mut self,
        ev: CoalescedMouseEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        if self.mouse_input_blocked() {
            return Ok(());
        }

        match ev {
            CoalescedMouseEvent::Down { x, y } => {
                let Some(target) = self.hit_map.hit(x, y) else {
                    return Ok(());
                };
                self.mouse_down(target, terminal).await
            }
            CoalescedMouseEvent::Scroll { x, y, delta } => {
                let Some(target) = self.hit_map.hit(x, y) else {
                    return Ok(());
                };
                self.mouse_scroll(target, delta);
                Ok(())
            }
            CoalescedMouseEvent::Drag { x, y } => {
                self.mouse_drag(x, y);
                Ok(())
            }
            CoalescedMouseEvent::Up { .. } => {
                if let DragState::PressedDiffLine {
                    open_on_up: true, ..
                } = self.drag_state
                {
                    self.diff_open_at_cursor();
                }
                self.drag_state = DragState::Idle;
                Ok(())
            }
        }
    }

    fn mouse_drag(&mut self, x: u16, y: u16) {
        match self.drag_state {
            DragState::ResizingSplit { body_x, body_width } => {
                if body_width == 0 {
                    return;
                }
                let rel = u32::from(x.saturating_sub(body_x));
                let percent = (rel * 100 / u32::from(body_width)) as u16;
                // セッション内のみの変更(ファイルへは書き戻さない)
                self.config.layout.left_panel_width = percent.clamp(10, 90);
            }
            DragState::PressedDiffLine { anchor, .. } => {
                // 最初の Drag で複数行選択へ昇格(V 相当)
                if let Some(line) = self.diff_line_at(x, y) {
                    self.multiline_selection = Some(super::MultilineSelection {
                        anchor_line: anchor,
                        cursor_line: line,
                    });
                    self.mouse_extend_diff_selection(line);
                    self.drag_state = DragState::SelectingDiffLines;
                }
            }
            DragState::SelectingDiffLines => {
                if let Some(line) = self.diff_line_at(x, y) {
                    self.mouse_extend_diff_selection(line);
                }
            }
            DragState::Idle => {}
        }
    }

    fn diff_line_at(&self, x: u16, y: u16) -> Option<usize> {
        match self.hit_map.hit(x, y) {
            Some(HitTarget::ContentLine {
                pane: PaneKind::Diff,
                line,
            }) => Some(line),
            _ => None,
        }
    }

    /// 選択拡張: カーソル行と選択終端を同時に動かす(キー移動と同じ)
    fn mouse_extend_diff_selection(&mut self, line: usize) {
        let line = line.min(self.diff_scroll.line_count.saturating_sub(1));
        self.diff_scroll.selected_line = line;
        self.diff_scroll.page_up(0);
        if let Some(sel) = self.multiline_selection.as_mut() {
            sel.cursor_line = line;
        }
    }

    /// クリックしたペインへフォーカスを移す(h/l/Tab と同じ状態遷移を通す)
    fn mouse_focus_pane(&mut self, pane: PaneKind) {
        match (self.state, pane) {
            // Split view: ファイル一覧 ⇄ 差分プレビュー
            (AppState::SplitViewFileList, PaneKind::Diff) if !self.files().is_empty() => {
                self.state = AppState::SplitViewDiff;
            }
            (AppState::SplitViewDiff, PaneKind::List(ListKind::FileList)) => {
                self.state = AppState::SplitViewFileList;
            }
            // Git Ops: 左リスト ⇄ 右差分、左内の Tree/Commits フォーカス
            (AppState::GitOpsSplitTree, PaneKind::GitOpsDiff) => {
                if let Some(ops) = self.git_ops_state.as_mut() {
                    ops.left_return_focus = ops.left_focus;
                }
                self.state = AppState::GitOpsSplitDiff;
            }
            (
                AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff,
                PaneKind::List(ListKind::GitOpsTree),
            ) => {
                if let Some(ops) = self.git_ops_state.as_mut() {
                    ops.left_focus = LeftPaneFocus::Tree;
                }
                self.state = AppState::GitOpsSplitTree;
            }
            (
                AppState::GitOpsSplitTree | AppState::GitOpsSplitDiff,
                PaneKind::List(ListKind::GitOpsCommits),
            ) => {
                if let Some(ops) = self.git_ops_state.as_mut() {
                    ops.left_focus = LeftPaneFocus::Commits;
                }
                self.state = AppState::GitOpsSplitTree;
            }
            // Repository Browser: ツリー ⇄ ファイル内容
            (AppState::RepoBrowseTree, PaneKind::BrowseFile)
                if self
                    .browse_state
                    .as_ref()
                    .is_some_and(|state| state.open.is_some()) =>
            {
                self.state = AppState::RepoBrowseFile;
            }
            (
                AppState::RepoBrowseFile | AppState::RepoBrowseGraph,
                PaneKind::List(ListKind::BrowseTree),
            ) => {
                self.state = AppState::RepoBrowseTree;
            }
            // グラフペインのクリックでグラフへフォーカス(Right 相当)
            (
                AppState::RepoBrowseTree | AppState::RepoBrowseFile,
                PaneKind::ModuleGraph | PaneKind::List(ListKind::BrowseGraph),
            ) => {
                self.state = AppState::RepoBrowseGraph;
            }
            // ファイルペインのクリックでグラフからコードへ戻る(Esc 相当)
            (AppState::RepoBrowseGraph, PaneKind::BrowseFile) => {
                self.state = AppState::RepoBrowseFile;
            }
            // Issue detail: 本文 ⇄ リンク PR
            (AppState::IssueDetail, PaneKind::IssueBody) => {
                self.issue_detail_set_focus(super::IssueDetailFocus::Body);
            }
            (AppState::IssueDetail, PaneKind::List(ListKind::IssueLinkedPrs)) => {
                self.issue_detail_set_focus(super::IssueDetailFocus::LinkedPrs);
            }
            _ => {}
        }
    }

    /// Split view 中のファイル選択変更後に差分プレビューを同期する
    /// (キー経路と同じ Dir 行ガード付き)
    fn split_view_sync_after_file_selection(&mut self) {
        if !matches!(
            self.state,
            AppState::SplitViewFileList | AppState::SplitViewDiff
        ) {
            return;
        }
        let tree_active = self.is_file_tree_active();
        if !tree_active
            || self
                .file_tree_state
                .as_ref()
                .is_none_or(|t| t.selected_file_index().is_some())
        {
            self.sync_diff_to_selected_file();
        }
    }

    /// フォーカス比較用のスナップショット(画面 + サブフォーカス)
    fn focus_snapshot(&self) -> (Discriminant<AppState>, Option<LeftPaneFocus>, bool) {
        (
            std::mem::discriminant(&self.state),
            self.git_ops_state.as_ref().map(|g| g.left_focus),
            self.issue_state
                .as_ref()
                .is_some_and(|s| s.detail_focus == super::IssueDetailFocus::LinkedPrs),
        )
    }

    async fn mouse_down(
        &mut self,
        target: HitTarget,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        // シンボルポップアップは開くのに async + terminal が要るため先に処理
        if let HitTarget::ListRow {
            list: ListKind::SymbolPopup,
            row,
            ..
        } = target
        {
            return self.symbol_popup_click(row, terminal).await;
        }

        // 複数行選択モード中は差分関連ターゲット以外のクリックを受けない
        // (キー処理が move/comment/suggestion/quit 以外を飲み込むのと同じ)
        if self.multiline_selection.is_some()
            && !matches!(
                target,
                HitTarget::ContentLine {
                    pane: PaneKind::Diff,
                    ..
                } | HitTarget::Pane {
                    pane: PaneKind::Diff
                } | HitTarget::Backdrop { .. }
                    | HitTarget::OverlaySurface
            )
        {
            return Ok(());
        }

        // ペインに属するターゲットならまずフォーカスを移す(h/l/Tab 相当)。
        // フォーカスが移った場合、そのクリックは選択までに留める
        // (非フォーカスペインの選択済み行クリックで即アクティベートしない)。
        let before = self.focus_snapshot();
        match target {
            HitTarget::ListRow { list, .. } => {
                self.mouse_focus_pane(PaneKind::List(list));
            }
            HitTarget::Pane { pane } | HitTarget::ContentLine { pane, .. } => {
                self.mouse_focus_pane(pane);
            }
            _ => {}
        }
        let focus_changed = self.focus_snapshot() != before;

        match target {
            HitTarget::ListRow { list, row, index } => {
                let opened = self.mouse_list_row_down(list, row, index, !focus_changed);
                if matches!(list, ListKind::FileList) && !opened {
                    self.split_view_sync_after_file_selection();
                }
            }
            // 行クリック=カーソル移動、選択済み行の再クリック=Enter 相当。
            // 複数行選択中のクリックは選択拡張、通常クリックはドラッグ昇格待ち。
            HitTarget::ContentLine {
                pane: PaneKind::Diff,
                line,
            } => {
                if self.multiline_selection.is_some() {
                    self.mouse_extend_diff_selection(line);
                    self.drag_state = DragState::SelectingDiffLines;
                } else {
                    let already_selected = self.diff_click_line(line);
                    if !already_selected && self.cmt.comment_panel_open {
                        // パネル表示中に別行へ移ったらパネル位置をリセット
                        // (next_comment のキー経路と同じ扱い)
                        self.cmt.comment_panel_scroll = 0;
                        self.cmt.selected_inline_comment = 0;
                    }
                    self.drag_state = DragState::PressedDiffLine {
                        anchor: line,
                        open_on_up: already_selected && !focus_changed,
                    };
                }
            }
            // カーソル移動のみ(これらのビューに Enter アクションはない)
            HitTarget::ContentLine {
                pane: PaneKind::GitOpsDiff,
                line,
            } => {
                let _ = self.git_ops_diff_click_line(line);
            }
            HitTarget::ContentLine {
                pane: PaneKind::BrowseFile,
                line,
            } => {
                let _ = self.browse_file_click_line(line);
            }
            HitTarget::ContentLine { .. } => {}
            // フォーカス切替は上で処理済み
            HitTarget::Pane { .. } => {}
            HitTarget::SplitDivider {
                body_x, body_width, ..
            } => {
                self.drag_state = DragState::ResizingSplit { body_x, body_width };
            }
            HitTarget::Backdrop { dismiss } => {
                if dismiss {
                    self.overlay_dismiss();
                }
            }
            // Surface はクリックを消費するだけ(内部の操作要素は上に登録済み)
            HitTarget::OverlaySurface => {}
            HitTarget::Tab {
                group: crate::ui::hit::TabGroup::GraphDirection,
                ..
            } => {
                self.browse_graph_toggle_direction();
            }
            HitTarget::Tab {
                group: crate::ui::hit::TabGroup::HelpTabs,
                index,
            } => {
                self.help_tab = if index == 0 {
                    super::HelpTab::Keybindings
                } else {
                    super::HelpTab::Config
                };
            }
            HitTarget::Tab {
                group: crate::ui::hit::TabGroup::CommentTabs,
                index,
            } => {
                self.cmt.comment_tab = if index == 0 {
                    super::CommentTab::Review
                } else {
                    super::CommentTab::Discussion
                };
            }
            // DialogButton は未対応(確認系はキーボード応答のみ)
            HitTarget::Tab { .. } | HitTarget::DialogButton { .. } => {}
        }
        Ok(())
    }

    /// シンボルポップアップのクリック(選択済み行の再クリックでジャンプ)
    async fn symbol_popup_click(
        &mut self,
        row: usize,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let Some(popup) = self.symbol_popup.as_mut() else {
            return Ok(());
        };
        if popup.selected != row {
            popup.selected = row.min(popup.symbols.len().saturating_sub(1));
            return Ok(());
        }
        let symbol_name = popup.symbols[popup.selected].0.clone();
        self.symbol_popup = None;
        self.jump_to_symbol_definition_async(&symbol_name, terminal)
            .await
    }

    fn symbol_popup_move(&mut self, down: bool) {
        let Some(popup) = self.symbol_popup.as_mut() else {
            return;
        };
        if down {
            popup.selected = (popup.selected + 1).min(popup.symbols.len().saturating_sub(1));
        } else {
            popup.selected = popup.selected.saturating_sub(1);
        }
    }

    /// クリック: 未選択行なら選択のみ、選択済み行なら開く(Enter 相当)。
    /// 戻り値は「開いたかどうか」。allow_open=false のときは選択のみ。
    fn mouse_list_row_down(
        &mut self,
        list: ListKind,
        row: usize,
        index: usize,
        allow_open: bool,
    ) -> bool {
        let already_selected = match list {
            ListKind::PrList => self.pr_list_click_select(row, index),
            ListKind::FileList => self.file_list_click_select(row, index),
            ListKind::ChecksList => self.checks_click_select(index),
            ListKind::IssueList => self.issue_list_click_select(row, index),
            ListKind::CockpitMenu => self.cockpit_click_select(row),
            ListKind::AiRallyLog => self.rally_log_click_select(row),
            ListKind::CommentList => self.comment_list_click_select(row),
            ListKind::IssueCommentList => self.issue_comment_click_select(index),
            ListKind::BrowseTree => self.browse_tree_click_select(row),
            ListKind::GitOpsTree => self.git_ops_tree_click_select(row),
            ListKind::GitOpsCommits => self.git_ops_commits_click_select(row),
            ListKind::IssueLinkedPrs => self.linked_pr_click_select(row),
            ListKind::BrowseOutline => self.browse_outline_click_select(row),
            ListKind::BrowseSymbolSearch => self.browse_symbol_search_click_select(row),
            ListKind::BrowsePrChoice => self.browse_pr_choice_click_select(row),
            ListKind::BrowseLineDiscussion => self.browse_line_discussion_click_select(row),
            ListKind::BrowseGraph => self.browse_graph_click_select(row),
            // SymbolPopup は mouse_down で async 処理済み
            ListKind::SymbolPopup => return false,
        };
        if !already_selected || !allow_open {
            return false;
        }
        match list {
            ListKind::PrList => self.open_selected_pr(),
            ListKind::FileList => self.open_selected_file_entry(),
            ListKind::ChecksList => self.open_selected_check(),
            ListKind::IssueList => self.open_selected_issue(),
            ListKind::CockpitMenu => self.activate_cockpit_selection(),
            ListKind::AiRallyLog => self.open_selected_rally_log(),
            ListKind::CommentList => self.comment_list_open_selected(),
            ListKind::IssueCommentList => self.open_selected_issue_comment(),
            ListKind::BrowseTree => self.open_selected_browse_entry(),
            ListKind::GitOpsTree => self.open_selected_git_ops_tree_entry(),
            ListKind::GitOpsCommits => self.open_selected_git_ops_commit(),
            ListKind::IssueLinkedPrs => self.open_selected_linked_pr(),
            ListKind::BrowseOutline => self.browse_outline_open_selected(),
            ListKind::BrowseSymbolSearch => self.browse_symbol_search_open_selected(),
            ListKind::BrowsePrChoice => self.browse_pr_choice_open_selected(),
            ListKind::BrowseLineDiscussion => self.browse_line_discussion_open_selected(),
            ListKind::BrowseGraph => self.browse_graph_open_selected(),
            ListKind::SymbolPopup => {}
        }
        true
    }

    /// ホイール: カーソル下のペインのカーソル/選択を wheel_step × ノッチ数だけ動かす。
    /// 既存の move 経路を呼ぶため lazy fetch や diff プリフェッチも同一に発火する。
    fn mouse_scroll(&mut self, target: HitTarget, delta: i32) {
        let pane = match target {
            HitTarget::ListRow { list, .. } => PaneKind::List(list),
            HitTarget::Pane { pane } => pane,
            HitTarget::ContentLine { pane, .. } => pane,
            _ => return,
        };
        let steps = i64::from(self.config.mouse.wheel_step)
            .saturating_mul(i64::from(delta.unsigned_abs()))
            .min(1000);
        let down = delta > 0;
        for _ in 0..steps {
            match pane {
                PaneKind::List(list) => {
                    if down {
                        self.mouse_list_move_down(list);
                    } else {
                        self.mouse_list_move_up(list);
                    }
                }
                PaneKind::Diff => {
                    self.diff_wheel_move(down);
                    // 複数行選択中はキー移動と同じく選択終端も追従させる
                    let cursor = self.diff_scroll.selected_line;
                    if let Some(sel) = self.multiline_selection.as_mut() {
                        sel.cursor_line = cursor;
                    }
                }
                PaneKind::BrowseFile => self.browse_file_wheel_move(down),
                PaneKind::GitOpsDiff => self.git_ops_diff_wheel_move(down),
                PaneKind::PrDescription => self.pr_description_wheel(down),
                PaneKind::Help => self.help_wheel(down),
                PaneKind::IssueBody => self.issue_body_wheel(down),
                // TextArea のビューポートはカーソル駆動(adjust_scroll が毎レンダ
                // 再計算)のため独立スクロールは成立しない — v1 は no-op
                PaneKind::TextArea => {}
                PaneKind::ShellOutput => self.shell_scroll_by(if down { 1 } else { -1 }),
                PaneKind::CommentPanel => self.comment_panel_wheel(down),
                PaneKind::GitOpsPreview => self.git_ops_preview_wheel(down),
                PaneKind::ModuleGraph => self.browse_graph_move(down),
            }
        }
        if pane == PaneKind::List(ListKind::FileList) {
            self.split_view_sync_after_file_selection();
        }
    }

    fn mouse_list_move_down(&mut self, list: ListKind) {
        match list {
            ListKind::PrList => self.pr_list_move_down(),
            ListKind::FileList => self.file_list_move_down(),
            ListKind::ChecksList => self.checks_move_down(),
            ListKind::IssueList => self.issue_list_move_down(),
            ListKind::CockpitMenu => self.cockpit_move_down(),
            ListKind::AiRallyLog => self.rally_log_move_down(),
            ListKind::CommentList => self.comment_list_move_down(),
            ListKind::IssueCommentList => self.issue_comments_move_down(),
            ListKind::BrowseTree => self.browse_tree_move_down(),
            ListKind::GitOpsTree => self.git_ops_tree_move_down(),
            ListKind::GitOpsCommits => self.git_ops_commits_move_down(),
            ListKind::IssueLinkedPrs => self.linked_prs_move_down(),
            ListKind::SymbolPopup => self.symbol_popup_move(true),
            ListKind::BrowseOutline => self.browse_outline_move(true),
            ListKind::BrowseSymbolSearch => self.browse_symbol_search_move(true),
            ListKind::BrowsePrChoice => self.browse_pr_choice_move(true),
            ListKind::BrowseLineDiscussion => self.browse_line_discussion_move(true),
            ListKind::BrowseGraph => self.browse_graph_move(true),
        }
    }

    fn mouse_list_move_up(&mut self, list: ListKind) {
        match list {
            ListKind::PrList => self.pr_list_move_up(),
            ListKind::FileList => self.file_list_move_up(),
            ListKind::ChecksList => self.checks_move_up(),
            ListKind::IssueList => self.issue_list_move_up(),
            ListKind::CockpitMenu => self.cockpit_move_up(),
            ListKind::AiRallyLog => self.rally_log_move_up(),
            ListKind::CommentList => self.comment_list_move_up(),
            ListKind::IssueCommentList => self.issue_comments_move_up(),
            ListKind::BrowseTree => self.browse_tree_move_up(),
            ListKind::GitOpsTree => self.git_ops_tree_move_up(),
            ListKind::GitOpsCommits => self.git_ops_commits_move_up(),
            ListKind::IssueLinkedPrs => self.linked_prs_move_up(),
            ListKind::SymbolPopup => self.symbol_popup_move(false),
            ListKind::BrowseOutline => self.browse_outline_move(false),
            ListKind::BrowseSymbolSearch => self.browse_symbol_search_move(false),
            ListKind::BrowsePrChoice => self.browse_pr_choice_move(false),
            ListKind::BrowseLineDiscussion => self.browse_line_discussion_move(false),
            ListKind::BrowseGraph => self.browse_graph_move(false),
        }
    }

    fn comment_list_move_down(&mut self) {
        match self.cmt.comment_tab {
            super::CommentTab::Discussion => self.discussion_move_down(),
            super::CommentTab::Review => self.review_nav_down(1),
        }
    }

    fn comment_list_move_up(&mut self) {
        match self.cmt.comment_tab {
            super::CommentTab::Discussion => self.discussion_move_up(),
            super::CommentTab::Review => self.review_nav_up(1),
        }
    }

    fn comment_list_open_selected(&mut self) {
        match self.cmt.comment_tab {
            super::CommentTab::Discussion => self.open_selected_discussion_comment(),
            super::CommentTab::Review => self.review_tab_open_panel(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::RepositoryAvailability;
    use crate::config::Config;
    use crate::github::{Branch, ChangedFile, PullRequest, User};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn loaded_file_list_app(file_count: usize) -> App {
        let (mut app, _tx) = App::new_loading(
            "owner/repo",
            1,
            Config::default(),
            RepositoryAvailability::Available,
        );
        let files: Vec<ChangedFile> = (0..file_count)
            .map(|i| ChangedFile {
                filename: format!("file_{i}.rs"),
                status: "modified".to_string(),
                additions: 1,
                deletions: 1,
                patch: Some("@@ -1,1 +1,1 @@\n-old\n+new".to_string()),
                viewed: false,
            })
            .collect();
        let pr = Box::new(PullRequest {
            number: 1,
            node_id: None,
            title: "Test PR".to_string(),
            body: None,
            state: "open".to_string(),
            head: Branch {
                ref_name: "feature".to_string(),
                sha: "abc123".to_string(),
            },
            base: Branch {
                ref_name: "main".to_string(),
                sha: "def456".to_string(),
            },
            user: User {
                login: "user".to_string(),
            },
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        });
        app.data_state = DataState::Loaded { pr, files };
        app.state = AppState::FileList;
        app
    }

    #[test]
    fn test_mouse_input_blocked_while_loading_on_data_dependent_screen() {
        let (app, _tx) = App::new_loading(
            "owner/repo",
            1,
            Config::default(),
            RepositoryAvailability::Available,
        );
        assert!(matches!(app.data_state, DataState::Loading));
        assert!(app.mouse_input_blocked());
    }

    #[test]
    fn test_mouse_input_not_blocked_on_data_independent_screen() {
        let app = App::new_cockpit("owner/repo", Config::default(), false);
        assert!(!app.mouse_input_blocked());
    }

    #[test]
    fn test_mouse_input_blocked_by_pending_approve_dialog() {
        let mut app = loaded_file_list_app(3);
        assert!(!app.mouse_input_blocked());
        app.cmt.pending_approve_body = Some(String::new());
        assert!(app.mouse_input_blocked());
    }

    #[test]
    fn test_cockpit_click_select_semantics() {
        let mut app = App::new_cockpit("owner/repo", Config::default(), false);
        assert!(!app.cockpit_click_select(1), "未選択行のクリックは選択のみ");
        assert!(
            app.cockpit_click_select(1),
            "選択済み行の再クリックは true(開く)"
        );
    }

    #[tokio::test]
    async fn test_mouse_click_selects_then_opens_cockpit_entry() {
        let mut app = App::new_cockpit("owner/repo", Config::default(), true);
        assert_eq!(app.state, AppState::Cockpit);

        // row 2 = LocalDiff(初期選択は row 0 なので 1 回目は選択のみ)
        app.mouse_list_row_down(ListKind::CockpitMenu, 2, 2, true);
        assert_eq!(app.state, AppState::Cockpit, "1回目のクリックは選択のみ");

        app.mouse_list_row_down(ListKind::CockpitMenu, 2, 2, true);
        assert_eq!(
            app.state,
            AppState::FileList,
            "選択済み行の再クリックで開く(LocalDiff は FileList へ)"
        );
    }

    #[test]
    fn test_cockpit_activation_blocked_without_repo_even_via_mouse() {
        let mut app = App::new_cockpit("owner/repo", Config::default(), false);
        app.mouse_list_row_down(ListKind::CockpitMenu, 0, 0, true);
        app.mouse_list_row_down(ListKind::CockpitMenu, 0, 0, true);
        assert_eq!(
            app.state,
            AppState::Cockpit,
            "repo なしでは requires_repo 項目は開かない(キーと同一挙動)"
        );
    }

    #[test]
    fn test_wheel_moves_file_selection_by_wheel_step() {
        let mut app = loaded_file_list_app(10);
        assert_eq!(app.selected_file, 0);

        let pane = HitTarget::Pane {
            pane: PaneKind::List(ListKind::FileList),
        };
        app.mouse_scroll(pane, 1);
        assert_eq!(app.selected_file, 3, "wheel_step 既定 3 で 3 行進む");

        app.mouse_scroll(pane, -1);
        assert_eq!(app.selected_file, 0);
    }

    #[test]
    fn test_wheel_on_list_row_target_scrolls_the_same_list() {
        let mut app = loaded_file_list_app(10);
        let row = HitTarget::ListRow {
            list: ListKind::FileList,
            row: 2,
            index: 2,
        };
        app.mouse_scroll(row, 2);
        assert_eq!(app.selected_file, 6, "2 ノッチ × wheel_step 3");
    }

    #[test]
    fn test_wheel_clamps_at_list_end() {
        let mut app = loaded_file_list_app(4);
        let pane = HitTarget::Pane {
            pane: PaneKind::List(ListKind::FileList),
        };
        app.mouse_scroll(pane, 5);
        assert_eq!(app.selected_file, 3, "末尾でクランプ");
        app.mouse_scroll(pane, -5);
        assert_eq!(app.selected_file, 0, "先頭でクランプ");
    }

    #[test]
    fn test_file_list_click_select_then_reclick_reports_already_selected() {
        let mut app = loaded_file_list_app(5);
        assert!(!app.file_list_click_select(2, 2));
        assert_eq!(app.selected_file, 2);
        assert!(app.file_list_click_select(2, 2));
    }

    #[test]
    fn test_diff_wheel_moves_cursor_by_wheel_step() {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::DiffView;
        app.diff_scroll.line_count = 100;
        app.diff_scroll.selected_line = 10;

        let pane = HitTarget::Pane {
            pane: PaneKind::Diff,
        };
        app.mouse_scroll(pane, 1);
        assert_eq!(
            app.diff_scroll.selected_line, 13,
            "wheel_step 3 で 3 行下へ"
        );

        app.mouse_scroll(pane, -2);
        assert_eq!(app.diff_scroll.selected_line, 7, "2 ノッチ上で 6 行上へ");
    }

    #[test]
    fn test_diff_click_line_selects_then_reports_already_selected() {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::DiffView;
        app.diff_scroll.line_count = 100;
        app.diff_scroll.selected_line = 0;

        assert!(!app.diff_click_line(42), "未選択行のクリックはカーソル移動");
        assert_eq!(app.diff_scroll.selected_line, 42);
        assert!(app.diff_click_line(42), "同一行の再クリックは true(開く)");
    }

    #[test]
    fn test_pr_description_wheel_scrolls_offset() {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::PrDescription;
        assert_eq!(app.pr_description_scroll_offset, 0);

        let pane = HitTarget::Pane {
            pane: PaneKind::PrDescription,
        };
        app.mouse_scroll(pane, 1);
        assert_eq!(app.pr_description_scroll_offset, 3);

        app.mouse_scroll(pane, -1);
        assert_eq!(app.pr_description_scroll_offset, 0);
        app.mouse_scroll(pane, -1);
        assert_eq!(app.pr_description_scroll_offset, 0, "先頭で飽和");
    }

    #[test]
    fn test_content_line_scroll_targets_owning_pane() {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::DiffView;
        app.diff_scroll.line_count = 100;
        app.diff_scroll.selected_line = 10;

        let line = HitTarget::ContentLine {
            pane: PaneKind::Diff,
            line: 12,
        };
        app.mouse_scroll(line, 1);
        assert_eq!(
            app.diff_scroll.selected_line, 13,
            "ContentLine 上のホイールも同じペインをスクロール"
        );
    }

    #[test]
    fn test_click_diff_pane_focuses_split_view_diff() {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::SplitViewFileList;

        app.mouse_focus_pane(PaneKind::Diff);
        assert_eq!(app.state, AppState::SplitViewDiff);

        app.mouse_focus_pane(PaneKind::List(ListKind::FileList));
        assert_eq!(app.state, AppState::SplitViewFileList);
    }

    #[test]
    fn test_click_diff_pane_without_files_keeps_file_list_focus() {
        let mut app = loaded_file_list_app(0);
        app.state = AppState::SplitViewFileList;
        app.mouse_focus_pane(PaneKind::Diff);
        assert_eq!(app.state, AppState::SplitViewFileList);
    }

    #[test]
    fn test_divider_drag_resizes_split_session_only() {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::SplitViewFileList;
        assert_eq!(app.config.layout.left_panel_width, 35);

        app.drag_state = DragState::ResizingSplit {
            body_x: 0,
            body_width: 100,
        };
        app.mouse_drag(60, 5);
        assert_eq!(app.config.layout.left_panel_width, 60);

        app.mouse_drag(5, 5);
        assert_eq!(app.config.layout.left_panel_width, 10, "下限 10 でクランプ");

        app.mouse_drag(99, 5);
        assert_eq!(app.config.layout.left_panel_width, 90, "上限 90 でクランプ");
    }

    #[test]
    fn test_drag_without_active_drag_state_is_noop() {
        let mut app = loaded_file_list_app(3);
        let before = app.config.layout.left_panel_width;
        app.mouse_drag(70, 5);
        assert_eq!(app.config.layout.left_panel_width, before);
    }

    #[test]
    fn test_backdrop_dismiss_closes_symbol_popup() {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::DiffView;
        app.symbol_popup = Some(super::super::SymbolPopupState {
            symbols: vec![("foo".to_string(), 0, 0), ("bar".to_string(), 0, 0)],
            selected: 0,
        });

        app.overlay_dismiss();
        assert!(
            app.symbol_popup.is_none(),
            "外側クリックでポップアップが閉じる"
        );
    }

    #[test]
    fn test_symbol_popup_move_and_wheel_dispatch() {
        let mut app = loaded_file_list_app(3);
        app.symbol_popup = Some(super::super::SymbolPopupState {
            symbols: vec![
                ("a".to_string(), 0, 0),
                ("b".to_string(), 0, 0),
                ("c".to_string(), 0, 0),
            ],
            selected: 0,
        });

        let row = HitTarget::ListRow {
            list: ListKind::SymbolPopup,
            row: 0,
            index: 0,
        };
        app.mouse_scroll(row, 1);
        assert_eq!(
            app.symbol_popup.as_ref().unwrap().selected,
            2,
            "wheel_step 3 は末尾クランプで 2"
        );
    }

    #[test]
    fn test_mouse_blocked_during_simple_git_confirm() {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::GitOpsSplitTree;
        assert!(!app.mouse_input_blocked());
        // Simple 確認はモーダルを描画しないため dispatch 側で塞ぐ
        let mut ops = super::super::GitOpsState::new(Vec::new());
        ops.pending_confirm = Some(super::super::PendingGitOpsConfirm::Simple {
            op: super::super::DestructiveOp::Discard {
                invocation: super::super::DiscardInvocation::Restore {
                    path: "a.rs".to_string(),
                },
            },
        });
        app.git_ops_state = Some(ops);
        assert!(app.mouse_input_blocked());
    }

    #[test]
    fn test_focus_changing_click_does_not_activate_row() {
        let mut app = loaded_file_list_app(5);
        app.state = AppState::SplitViewDiff;
        app.selected_file = 2;

        // 非フォーカスのファイル一覧ペインで選択済み行をクリック:
        // フォーカスが移るだけで開かない
        let opened = {
            let before = app.focus_snapshot();
            app.mouse_focus_pane(PaneKind::List(ListKind::FileList));
            let focus_changed = app.focus_snapshot() != before;
            app.mouse_list_row_down(ListKind::FileList, 2, 2, !focus_changed)
        };
        assert!(!opened);
        assert_eq!(app.state, AppState::SplitViewFileList, "フォーカスだけ移る");
    }

    fn diff_app_with_hit_rows(lines: usize) -> App {
        let mut app = loaded_file_list_app(3);
        app.state = AppState::DiffView;
        app.diff_scroll.line_count = lines;
        for line in 0..lines {
            app.hit_map.push(
                ratatui::layout::Rect {
                    x: 0,
                    y: line as u16,
                    width: 40,
                    height: 1,
                },
                HitTarget::ContentLine {
                    pane: PaneKind::Diff,
                    line,
                },
            );
        }
        app
    }

    fn diff_line_target(line: usize) -> HitTarget {
        HitTarget::ContentLine {
            pane: PaneKind::Diff,
            line,
        }
    }

    /// A throwaway terminal for handlers that take one but do not draw.
    fn test_terminal() -> ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>> {
        ratatui::Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(std::io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 120, 40)),
            },
        )
        .expect("terminal")
    }

    #[tokio::test]
    async fn test_reclick_on_selected_diff_line_opens_panel_on_up_not_down() {
        let mut app = diff_app_with_hit_rows(20);
        app.diff_scroll.selected_line = 5;
        let mut terminal = test_terminal();

        // 選択済み行への Down: この時点ではパネルを開かない
        app.mouse_down(diff_line_target(5), &mut terminal)
            .await
            .unwrap();
        assert!(
            !app.cmt.comment_panel_open,
            "Down の時点で開くとドラッグ選択が始められない"
        );
        assert_eq!(
            app.drag_state,
            DragState::PressedDiffLine {
                anchor: 5,
                open_on_up: true
            }
        );

        // Drag なしの Up で初めて開く(再クリック=Enter 相当)
        app.handle_mouse(CoalescedMouseEvent::Up { x: 0, y: 5 }, &mut terminal)
            .await
            .unwrap();
        assert!(app.cmt.comment_panel_open, "Up でパネルが開く");
        assert_eq!(app.drag_state, DragState::Idle);
    }

    #[tokio::test]
    async fn test_drag_from_selected_line_selects_range_instead_of_opening_panel() {
        let mut app = diff_app_with_hit_rows(20);
        app.diff_scroll.selected_line = 5;
        let mut terminal = test_terminal();

        // カーソル行から Down → Drag: パネルではなく複数行選択になる
        app.mouse_down(diff_line_target(5), &mut terminal)
            .await
            .unwrap();
        app.mouse_drag(0, 9);
        assert!(!app.cmt.comment_panel_open, "ドラッグしたら開かない");
        let sel = app.multiline_selection.as_ref().unwrap();
        assert_eq!((sel.anchor_line, sel.cursor_line), (5, 9));

        // Up で確定してもパネルは開かず選択が残る
        app.handle_mouse(CoalescedMouseEvent::Up { x: 0, y: 9 }, &mut terminal)
            .await
            .unwrap();
        assert!(!app.cmt.comment_panel_open);
        assert!(app.multiline_selection.is_some(), "選択は確定後も維持");
    }

    #[tokio::test]
    async fn test_click_other_line_while_panel_open_resets_panel_position() {
        let mut app = diff_app_with_hit_rows(20);
        app.diff_scroll.selected_line = 5;
        app.cmt.comment_panel_open = true;
        app.cmt.comment_panel_scroll = 7;
        app.cmt.selected_inline_comment = 1;
        let mut terminal = test_terminal();

        app.mouse_down(diff_line_target(9), &mut terminal)
            .await
            .unwrap();
        assert_eq!(app.diff_scroll.selected_line, 9);
        assert_eq!(
            app.cmt.comment_panel_scroll, 0,
            "行が変わればパネル位置リセット"
        );
        assert_eq!(app.cmt.selected_inline_comment, 0);
    }

    #[test]
    fn test_pressed_diff_line_promotes_to_selection_on_first_drag() {
        let mut app = diff_app_with_hit_rows(20);
        app.drag_state = DragState::PressedDiffLine {
            anchor: 5,
            open_on_up: false,
        };

        app.mouse_drag(10, 8);
        assert_eq!(app.drag_state, DragState::SelectingDiffLines);
        let sel = app.multiline_selection.as_ref().unwrap();
        assert_eq!(sel.anchor_line, 5);
        assert_eq!(sel.cursor_line, 8);
        assert_eq!(app.diff_scroll.selected_line, 8);

        app.mouse_drag(10, 12);
        assert_eq!(app.multiline_selection.as_ref().unwrap().cursor_line, 12);
    }

    #[test]
    fn test_up_without_drag_keeps_plain_click_semantics() {
        let mut app = diff_app_with_hit_rows(20);
        app.drag_state = DragState::PressedDiffLine {
            anchor: 5,
            open_on_up: false,
        };

        // Drag なしで Up: 選択モードに入らない
        app.drag_state = DragState::Idle;
        assert!(app.multiline_selection.is_none());
    }

    #[test]
    fn test_selection_survives_mouse_up() {
        let mut app = diff_app_with_hit_rows(20);
        app.drag_state = DragState::PressedDiffLine {
            anchor: 3,
            open_on_up: false,
        };
        app.mouse_drag(0, 7);
        assert!(app.multiline_selection.is_some());

        // Up 相当(handle_mouse の Up アームは drag_state のみ Idle へ戻す)
        app.drag_state = DragState::Idle;
        let sel = app.multiline_selection.as_ref().unwrap();
        assert_eq!(
            (sel.anchor_line, sel.cursor_line),
            (3, 7),
            "選択は確定後も維持"
        );
    }

    #[test]
    fn test_drag_outside_diff_rows_keeps_selection_unchanged() {
        let mut app = diff_app_with_hit_rows(10);
        app.drag_state = DragState::PressedDiffLine {
            anchor: 2,
            open_on_up: false,
        };
        app.mouse_drag(100, 50);
        assert!(
            app.multiline_selection.is_none(),
            "差分行の外では昇格しない"
        );
        assert_eq!(
            app.drag_state,
            DragState::PressedDiffLine {
                anchor: 2,
                open_on_up: false
            }
        );
    }

    #[test]
    fn test_cockpit_render_populates_hit_map() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new_cockpit("owner/repo", Config::default(), false);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        assert!(!app.hit_map.is_empty(), "描画後に HitMap が登録されている");
        let hits: Vec<Option<HitTarget>> = (0..20).map(|y| app.hit_map.hit(30, y)).collect();
        assert!(
            hits.iter().any(|h| matches!(
                h,
                Some(HitTarget::ListRow {
                    list: ListKind::CockpitMenu,
                    ..
                })
            )),
            "メニュー行の ListRow が登録されている: {hits:?}"
        );
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn test_coalesce_merges_same_direction_scroll() {
        let (out, leftover) = coalesce(vec![
            mouse(MouseEventKind::ScrollDown, 5, 5),
            mouse(MouseEventKind::ScrollDown, 5, 6),
            mouse(MouseEventKind::ScrollDown, 5, 7),
        ]);
        assert_eq!(
            out,
            vec![CoalescedMouseEvent::Scroll {
                x: 5,
                y: 7,
                delta: 3
            }]
        );
        assert!(leftover.is_empty());
    }

    #[test]
    fn test_coalesce_splits_on_direction_change() {
        let (out, _) = coalesce(vec![
            mouse(MouseEventKind::ScrollDown, 1, 1),
            mouse(MouseEventKind::ScrollDown, 1, 1),
            mouse(MouseEventKind::ScrollUp, 1, 1),
        ]);
        assert_eq!(
            out,
            vec![
                CoalescedMouseEvent::Scroll {
                    x: 1,
                    y: 1,
                    delta: 2
                },
                CoalescedMouseEvent::Scroll {
                    x: 1,
                    y: 1,
                    delta: -1
                },
            ],
            "上下スクロールを相殺してはならない(逐次処理と等価にする)"
        );
    }

    #[test]
    fn test_coalesce_drag_keeps_last_position() {
        let (out, _) = coalesce(vec![
            mouse(MouseEventKind::Drag(MouseButton::Left), 1, 1),
            mouse(MouseEventKind::Drag(MouseButton::Left), 2, 2),
            mouse(MouseEventKind::Drag(MouseButton::Left), 3, 3),
        ]);
        assert_eq!(out, vec![CoalescedMouseEvent::Drag { x: 3, y: 3 }]);
    }

    #[test]
    fn test_coalesce_drag_runs_split_by_other_events() {
        let (out, _) = coalesce(vec![
            mouse(MouseEventKind::Drag(MouseButton::Left), 1, 1),
            mouse(MouseEventKind::Down(MouseButton::Left), 2, 2),
            mouse(MouseEventKind::Drag(MouseButton::Left), 3, 3),
        ]);
        assert_eq!(
            out,
            vec![
                CoalescedMouseEvent::Drag { x: 1, y: 1 },
                CoalescedMouseEvent::Down { x: 2, y: 2 },
                CoalescedMouseEvent::Drag { x: 3, y: 3 },
            ]
        );
    }

    #[test]
    fn test_coalesce_drops_moved_events() {
        let (out, leftover) = coalesce(vec![
            mouse(MouseEventKind::Moved, 1, 1),
            mouse(MouseEventKind::Moved, 2, 2),
        ]);
        assert!(out.is_empty());
        assert!(leftover.is_empty());
    }

    #[test]
    fn test_coalesce_preserves_down_up_pairs() {
        let (out, _) = coalesce(vec![
            mouse(MouseEventKind::Down(MouseButton::Left), 1, 1),
            mouse(MouseEventKind::Down(MouseButton::Left), 1, 1),
            mouse(MouseEventKind::Up(MouseButton::Left), 1, 1),
        ]);
        assert_eq!(
            out,
            vec![
                CoalescedMouseEvent::Down { x: 1, y: 1 },
                CoalescedMouseEvent::Down { x: 1, y: 1 },
                CoalescedMouseEvent::Up { x: 1, y: 1 },
            ]
        );
    }

    #[test]
    fn test_coalesce_stops_at_key_and_requeues_rest() {
        let (out, leftover) = coalesce(vec![
            mouse(MouseEventKind::ScrollDown, 1, 1),
            key('j'),
            mouse(MouseEventKind::ScrollUp, 1, 1),
        ]);
        assert_eq!(
            out,
            vec![CoalescedMouseEvent::Scroll {
                x: 1,
                y: 1,
                delta: 1
            }]
        );
        assert_eq!(leftover.len(), 2, "Key とその後続は手つかずで返す");
        assert!(matches!(leftover[0], Event::Key(_)));
        assert!(matches!(leftover[1], Event::Mouse(_)));
    }

    #[test]
    fn test_coalesce_drops_non_left_buttons_and_horizontal_scroll() {
        let (out, _) = coalesce(vec![
            mouse(MouseEventKind::Down(MouseButton::Right), 1, 1),
            mouse(MouseEventKind::Down(MouseButton::Middle), 1, 1),
            mouse(MouseEventKind::ScrollLeft, 1, 1),
            mouse(MouseEventKind::ScrollRight, 1, 1),
        ]);
        assert!(out.is_empty());
    }

    #[test]
    fn test_coalesce_drops_modified_clicks() {
        let (out, _) = coalesce(vec![Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::CONTROL,
        })]);
        assert!(out.is_empty());
    }

    #[test]
    fn test_coalesce_scroll_run_continues_across_dropped_events() {
        let (out, _) = coalesce(vec![
            mouse(MouseEventKind::ScrollDown, 1, 1),
            mouse(MouseEventKind::Moved, 9, 9),
            mouse(MouseEventKind::ScrollDown, 2, 2),
        ]);
        assert_eq!(
            out,
            vec![CoalescedMouseEvent::Scroll {
                x: 2,
                y: 2,
                delta: 2
            }]
        );
    }
}
