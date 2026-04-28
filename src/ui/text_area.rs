//! Multi-line text input widget used for reply / comment composition.
//!
//! ## Wrapping
//!
//! Cursor placement under wrap requires knowing exactly where the renderer
//! breaks each logical line. ratatui's `Wrap` is word-aware (textwrap-style)
//! and its break positions are not part of the public API, so cursor math
//! layered on top of it would either drift out of sync with what's rendered
//! or have to re-derive the same breaks via a direct `textwrap` dependency.
//!
//! Instead we pre-wrap at character boundaries with `wrap_line_to_rows` and
//! pass the already-wrapped lines to `Paragraph` with no `Wrap` set. The
//! same algorithm drives `display_rows_for_line`, so the cursor cell and
//! the rendered glyph cell are computed from a single source of truth.
//!
//! Char-boundary wrap is also a closer match to editing intuition:
//! characters flow past the right edge the way a raw terminal does, and
//! inserting a character can never re-flow the wrap point of an earlier
//! row the way word-aware wrap can.
//!
//! If word-aware wrap is ever wanted here, the right move is to depend on
//! `textwrap` directly and keep the "Paragraph receives pre-wrapped lines"
//! contract — re-enabling `ratatui::widgets::Wrap` would reintroduce the
//! cursor-vs-render divergence.

use std::cell::Cell;

use crossterm::event::{self, KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthChar;

use crate::keybinding::{event_to_keybinding, KeySequence, SequenceMatch, SequenceState};

/// Display rows occupied by `line` when wrapped at character boundaries to
/// `inner_width` display columns. Empty lines still take one row.
///
/// This must agree with [`wrap_line_to_rows`] — they encode the same
/// wrapping algorithm, one as a count and one as the actual split.
fn display_rows_for_line(line: &str, inner_width: usize) -> usize {
    let inner_width = inner_width.max(1);
    let width: usize = line
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    if width == 0 {
        1
    } else {
        width.div_ceil(inner_width)
    }
}

/// Wrap a styled line at character boundaries so each emitted row fits within
/// `inner_width` display columns. Span styles are preserved across splits.
/// An empty input yields a single empty row so blank lines still take a row.
fn wrap_line_to_rows(line: &Line<'static>, inner_width: usize) -> Vec<Line<'static>> {
    let inner_width = inner_width.max(1);
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut current_row: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;

    for span in &line.spans {
        let style = span.style;
        let mut buf = String::new();
        for c in span.content.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if current_width + cw > inner_width && current_width > 0 {
                if !buf.is_empty() {
                    current_row.push(Span::styled(std::mem::take(&mut buf), style));
                }
                rows.push(Line::from(std::mem::take(&mut current_row)));
                current_width = 0;
            }
            buf.push(c);
            current_width += cw;
        }
        if !buf.is_empty() {
            current_row.push(Span::styled(buf, style));
        }
    }
    if !current_row.is_empty() || rows.is_empty() {
        rows.push(Line::from(current_row));
    }
    rows
}

/// テキストエリアのキー入力結果
pub enum TextAreaAction {
    /// 通常の編集操作（継続）
    Continue,
    /// Submit key pressed
    Submit,
    /// Esc: キャンセル
    Cancel,
    /// Waiting for more keys in a sequence
    PendingSequence,
}

/// TUI内で動作するマルチラインテキスト入力ウィジェット
pub struct TextArea {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    /// スクロールオフセット（interior mutability: render時にvisible_height更新後に再調整するため）
    scroll_offset: Cell<usize>,
    /// 最後にレンダリングされた領域の可視行数（ボーダー除く）
    /// render()で実際のレンダリング領域から更新される（interior mutability）
    visible_height: Cell<usize>,
    /// Inner width of the last rendered area (excluding borders).
    /// Used to compute wrapped display-row counts (interior mutability).
    inner_width: Cell<usize>,
    /// Custom submit key binding (default: Ctrl+S)
    submit_key: Option<KeySequence>,
    /// State for tracking pending key sequences
    sequence_state: SequenceState,
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new()
    }
}

impl TextArea {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_offset: Cell::new(0),
            visible_height: Cell::new(1),
            inner_width: Cell::new(1),
            submit_key: None,
            sequence_state: SequenceState::new(),
        }
    }

    /// Create a TextArea with a custom submit key binding
    pub fn with_submit_key(submit_key: KeySequence) -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_offset: Cell::new(0),
            visible_height: Cell::new(1),
            inner_width: Cell::new(1),
            submit_key: Some(submit_key),
            sequence_state: SequenceState::new(),
        }
    }

    /// Set the submit key binding
    pub fn set_submit_key(&mut self, submit_key: KeySequence) {
        self.submit_key = Some(submit_key);
    }

    /// Get the submit key display string
    pub fn submit_key_display(&self) -> String {
        self.submit_key
            .as_ref()
            .map(|seq| seq.display())
            .unwrap_or_else(|| "Ctrl-s".to_string())
    }

    /// Check if the key matches the submit binding (for single-key bindings)
    /// Returns None if this is a multi-key sequence that needs sequence tracking
    fn check_single_key_submit(&self, key: &event::KeyEvent) -> Option<bool> {
        if let Some(ref submit_seq) = self.submit_key {
            if submit_seq.is_single() {
                if let Some(first) = submit_seq.first() {
                    return Some(first.matches(key));
                }
            }
            // Multi-key sequence - handled by sequence state
            return None;
        }
        // Default: Ctrl+S
        Some(key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL))
    }

    /// キー入力を処理し、アクションを返す
    pub fn input(&mut self, key: event::KeyEvent) -> TextAreaAction {
        // Check for timeout on pending sequence - if timed out, flush buffered keys
        if self.sequence_state.pending_since.is_some() {
            let timed_out = self
                .sequence_state
                .pending_since
                .is_some_and(|since| since.elapsed() > crate::keybinding::SEQUENCE_TIMEOUT);
            if timed_out {
                // Timeout - flush buffered keys as normal input, then process current key
                let buffered = std::mem::take(&mut self.sequence_state.pending_keys);
                self.sequence_state.pending_since = None;
                for pending_key in buffered {
                    self.insert_keybinding(&pending_key);
                }
            }
        }

        // Check for single-key submit binding first
        if let Some(is_submit) = self.check_single_key_submit(&key) {
            if is_submit {
                self.sequence_state.clear();
                return TextAreaAction::Submit;
            }
        } else {
            // Multi-key sequence handling
            if let Some(ref submit_seq) = self.submit_key {
                if let Some(keybinding) = event_to_keybinding(&key) {
                    self.sequence_state.push(keybinding);
                    match self.sequence_state.matches(submit_seq) {
                        SequenceMatch::Full => {
                            self.sequence_state.clear();
                            return TextAreaAction::Submit;
                        }
                        SequenceMatch::Partial => {
                            return TextAreaAction::PendingSequence;
                        }
                        SequenceMatch::None => {
                            // Not a match - flush buffered keys EXCEPT the current one
                            // (current key will be processed by the match key.code block below)
                            let mut buffered =
                                std::mem::take(&mut self.sequence_state.pending_keys);
                            self.sequence_state.pending_since = None;
                            // Remove the last key (current key) - it will be handled normally
                            buffered.pop();
                            for pending_key in buffered {
                                self.insert_keybinding(&pending_key);
                            }
                            // Fall through to process the current key normally
                        }
                    }
                }
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.sequence_state.clear();
                return TextAreaAction::Cancel;
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
            }
            // 通常の Enter は改行
            KeyCode::Enter => {
                self.insert_newline();
            }
            KeyCode::Backspace => {
                self.backspace();
            }
            KeyCode::Delete => {
                self.delete();
            }
            KeyCode::Left => {
                self.move_left();
            }
            KeyCode::Right => {
                self.move_right();
            }
            KeyCode::Up => {
                self.move_up();
            }
            KeyCode::Down => {
                self.move_down();
            }
            KeyCode::Home => {
                self.cursor_col = 0;
            }
            KeyCode::End => {
                self.cursor_col = self.current_line_len();
            }
            _ => {}
        }
        self.adjust_scroll();
        TextAreaAction::Continue
    }

    /// テキスト全体を返す
    pub fn content(&self) -> String {
        self.lines.join("\n")
    }

    /// テキストが空かどうか
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// 初期コンテンツを設定する（カーソル・スクロールをリセット）
    pub fn set_content(&mut self, content: &str) {
        self.lines = if content.is_empty() {
            vec![String::new()]
        } else {
            content.lines().map(String::from).collect()
        };
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_offset.set(0);
    }

    /// カーソルを末尾に移動する
    /// Note: スクロール調整は行わない。visible_height がまだデフォルト値（1）の場合に
    /// 過剰スクロールが発生するため、次回の render_with_title() で正しい visible_height を
    /// 設定した後に adjust_scroll() が呼ばれることに依存する。
    pub fn move_to_end(&mut self) {
        self.cursor_row = self.lines.len().saturating_sub(1);
        self.cursor_col = self.lines[self.cursor_row].chars().count();
        // scroll_offset をリセットして、render 時に正しく再計算させる
        self.scroll_offset.set(0);
    }

    /// テキストエリアをクリアする
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_offset.set(0);
    }

    /// テキストエリアをレンダリング（デフォルトタイトル・プレースホルダー）
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let title = format!("Reply ({}: submit, Esc: cancel)", self.submit_key_display());
        self.render_with_title(frame, area, &title, "Type your reply here...");
    }

    /// カスタムタイトルとプレースホルダーでレンダリング
    pub fn render_with_title(&self, frame: &mut Frame, area: Rect, title: &str, placeholder: &str) {
        self.render_inner(frame, area, title, placeholder, None);
    }

    /// シンタックスハイライト付きでレンダリング
    ///
    /// `styled_lines` は TextArea の全行に対応するハイライト済み `Line<'static>` のスライス。
    /// スクロール・可視範囲の切り出しは内部で行う。
    pub fn render_highlighted(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        placeholder: &str,
        styled_lines: &[Line<'static>],
    ) {
        self.render_inner(frame, area, title, placeholder, Some(styled_lines));
    }

    fn render_inner(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        placeholder: &str,
        styled_lines: Option<&[Line<'static>]>,
    ) {
        let visible_height = area.height.saturating_sub(2).max(1) as usize; // borders
        let inner_width = area.width.saturating_sub(2).max(1) as usize;
        self.visible_height.set(visible_height);
        self.inner_width.set(inner_width);
        // visible_height/inner_width may have changed; re-adjust scroll so the
        // cursor stays visible after a terminal resize.
        self.adjust_scroll();

        let scroll_offset = self.scroll_offset.get();

        // Pre-wrap lines at character boundaries so what we render matches
        // exactly what the cursor math (display_rows_for_line) predicts.
        let logical_lines: Vec<Line<'static>> = if let Some(styled) = styled_lines {
            styled.to_vec()
        } else {
            self.lines
                .iter()
                .map(|l| Line::from(l.to_owned()))
                .collect()
        };

        let placeholder_style = Style::default().fg(Color::DarkGray);
        let display_text: Vec<Line<'static>> = if self.is_empty() {
            vec![Line::styled(placeholder.to_owned(), placeholder_style)]
        } else {
            logical_lines
                .iter()
                .flat_map(|l| wrap_line_to_rows(l, inner_width))
                .skip(scroll_offset)
                .take(visible_height)
                .collect()
        };

        let paragraph =
            Paragraph::new(display_text).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(paragraph, area);

        // Cursor placement: account for CJK display width and word wrap.
        let cursor_disp_col = self.cursor_display_width();
        let cursor_abs_row = self.cursor_absolute_display_row();
        let cursor_x = area.x + 1 + (cursor_disp_col % inner_width) as u16;
        let cursor_y = area.y + 1 + (cursor_abs_row.saturating_sub(scroll_offset)) as u16;
        if cursor_y < area.y + area.height.saturating_sub(1) {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    /// Absolute display-row index of the cursor (cumulative from line 0,
    /// accounting for word wrap).
    fn cursor_absolute_display_row(&self) -> usize {
        let inner_width = self.inner_width.get().max(1);
        let preceding: usize = self.lines[..self.cursor_row]
            .iter()
            .map(|l| display_rows_for_line(l, inner_width))
            .sum();
        preceding + self.cursor_display_width() / inner_width
    }

    fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor_row];
        // byte index from char position
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        line.insert(byte_idx, c);
        self.cursor_col += 1;
    }

    /// Insert a keybinding as text (for flushing buffered sequence keys)
    fn insert_keybinding(&mut self, keybinding: &crate::keybinding::KeyBinding) {
        use crate::keybinding::{KeyCodeConfig, NamedKey};
        match keybinding.code {
            KeyCodeConfig::Char(c) => {
                // If shift is held, insert uppercase
                let ch = if keybinding.modifiers.shift {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                self.insert_char(ch);
            }
            KeyCodeConfig::Named(NamedKey::Enter) => {
                self.insert_newline();
            }
            KeyCodeConfig::Named(NamedKey::Tab) => {
                // Insert tab as spaces or tab character
                self.insert_char('\t');
            }
            // Other named keys (arrows, backspace, etc.) are not insertable text
            _ => {}
        }
    }

    fn insert_newline(&mut self) {
        let line = &self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        let rest = line[byte_idx..].to_string();
        self.lines[self.cursor_row] = line[..byte_idx].to_string();
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.lines.insert(self.cursor_row, rest);
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let byte_idx = char_to_byte_index(line, self.cursor_col);
            let prev_byte_idx = char_to_byte_index(line, self.cursor_col - 1);
            line.drain(prev_byte_idx..byte_idx);
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            // 行頭でBackspace: 前の行と結合
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = char_count(&self.lines[self.cursor_row]);
            self.lines[self.cursor_row].push_str(&current_line);
        }
    }

    fn delete(&mut self) {
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            let line = &mut self.lines[self.cursor_row];
            let byte_idx = char_to_byte_index(line, self.cursor_col);
            let next_byte_idx = char_to_byte_index(line, self.cursor_col + 1);
            line.drain(byte_idx..next_byte_idx);
        } else if self.cursor_row + 1 < self.lines.len() {
            // 行末でDelete: 次の行と結合
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        }
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
        }
    }

    fn move_right(&mut self) {
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.cursor_col.min(self.current_line_len());
        }
    }

    fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = self.cursor_col.min(self.current_line_len());
        }
    }

    fn current_line_len(&self) -> usize {
        char_count(&self.lines[self.cursor_row])
    }

    /// カーソル位置までの表示幅を計算する（CJK文字は2カラム幅）
    fn cursor_display_width(&self) -> usize {
        let line = &self.lines[self.cursor_row];
        line.chars()
            .take(self.cursor_col)
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum()
    }

    fn adjust_scroll(&self) {
        // Scroll adjustment in display-row space so wrapped lines are
        // accounted for and the cursor stays visible.
        let visible_height = self.visible_height.get();
        let inner_width = self.inner_width.get().max(1);
        let total_rows: usize = self
            .lines
            .iter()
            .map(|l| display_rows_for_line(l, inner_width))
            .sum();
        let max_scroll = total_rows.saturating_sub(visible_height);
        let cursor_row = self.cursor_absolute_display_row();
        let mut scroll_offset = self.scroll_offset.get();
        if cursor_row < scroll_offset {
            scroll_offset = cursor_row;
        } else if cursor_row >= scroll_offset + visible_height {
            scroll_offset = cursor_row + 1 - visible_height;
        }
        // Clamp so we never scroll past the end of content. This also
        // recovers from stale scroll values produced by input()-time
        // adjustment before the first render (inner_width defaults to 1).
        scroll_offset = scroll_offset.min(max_scroll);
        self.scroll_offset.set(scroll_offset);
    }
}

/// 文字数を数える（マルチバイト対応）
fn char_count(s: &str) -> usize {
    s.chars().count()
}

/// 文字インデックスからバイトインデックスへ変換
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl_key_event(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn test_new_text_area_is_empty() {
        let ta = TextArea::new();
        assert!(ta.is_empty());
        assert_eq!(ta.content(), "");
    }

    #[test]
    fn test_insert_char() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('h')));
        ta.input(key_event(KeyCode::Char('i')));
        assert_eq!(ta.content(), "hi");
        assert!(!ta.is_empty());
    }

    #[test]
    fn test_insert_newline() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Enter));
        ta.input(key_event(KeyCode::Char('b')));
        assert_eq!(ta.content(), "a\nb");
    }

    #[test]
    fn test_backspace() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Char('b')));
        ta.input(key_event(KeyCode::Backspace));
        assert_eq!(ta.content(), "a");
    }

    #[test]
    fn test_backspace_at_line_start_joins_lines() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Enter));
        ta.input(key_event(KeyCode::Char('b')));
        ta.input(key_event(KeyCode::Home));
        ta.input(key_event(KeyCode::Backspace));
        assert_eq!(ta.content(), "ab");
    }

    #[test]
    fn test_delete_joins_lines() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Enter));
        ta.input(key_event(KeyCode::Char('b')));
        // Move to end of first line
        ta.input(key_event(KeyCode::Up));
        ta.input(key_event(KeyCode::End));
        ta.input(key_event(KeyCode::Delete));
        assert_eq!(ta.content(), "ab");
    }

    #[test]
    fn test_cursor_movement() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Char('b')));
        ta.input(key_event(KeyCode::Char('c')));
        ta.input(key_event(KeyCode::Left));
        ta.input(key_event(KeyCode::Left));
        ta.input(key_event(KeyCode::Char('x')));
        assert_eq!(ta.content(), "axbc");
    }

    #[test]
    fn test_submit_action() {
        let mut ta = TextArea::new();
        let action = ta.input(ctrl_key_event(KeyCode::Char('s')));
        assert!(matches!(action, TextAreaAction::Submit));
    }

    #[test]
    fn test_enter_inserts_newline() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('a')));
        let action = ta.input(key_event(KeyCode::Enter)); // Ctrl なし
        assert!(matches!(action, TextAreaAction::Continue));
        ta.input(key_event(KeyCode::Char('b')));
        assert_eq!(ta.content(), "a\nb");
    }

    #[test]
    fn test_cancel_action() {
        let mut ta = TextArea::new();
        let action = ta.input(key_event(KeyCode::Esc));
        assert!(matches!(action, TextAreaAction::Cancel));
    }

    #[test]
    fn test_set_content() {
        let mut ta = TextArea::new();
        ta.set_content("line1\nline2");
        assert_eq!(ta.content(), "line1\nline2");
        assert_eq!(ta.cursor_row, 0);
        assert_eq!(ta.cursor_col, 0);
    }

    #[test]
    fn test_set_content_empty() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('x')));
        ta.set_content("");
        assert!(ta.is_empty());
        assert_eq!(ta.cursor_row, 0);
        assert_eq!(ta.cursor_col, 0);
    }

    #[test]
    fn test_clear() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Enter));
        ta.input(key_event(KeyCode::Char('b')));
        ta.clear();
        assert!(ta.is_empty());
        assert_eq!(ta.cursor_row, 0);
        assert_eq!(ta.cursor_col, 0);
    }

    #[test]
    fn test_multibyte_chars() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('あ')));
        ta.input(key_event(KeyCode::Char('い')));
        assert_eq!(ta.content(), "あい");
        ta.input(key_event(KeyCode::Backspace));
        assert_eq!(ta.content(), "あ");
    }

    #[test]
    fn test_cjk_cursor_display_width() {
        let mut ta = TextArea::new();
        // "あい" を入力 → cursor_col = 2, 表示幅 = 4
        ta.input(key_event(KeyCode::Char('あ')));
        ta.input(key_event(KeyCode::Char('い')));
        assert_eq!(ta.cursor_col, 2);
        assert_eq!(ta.cursor_display_width(), 4);
    }

    #[test]
    fn test_mixed_ascii_cjk_cursor_display_width() {
        let mut ta = TextArea::new();
        // "aあb" → cursor_col = 3, 表示幅 = 1+2+1 = 4
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Char('あ')));
        ta.input(key_event(KeyCode::Char('b')));
        assert_eq!(ta.cursor_col, 3);
        assert_eq!(ta.cursor_display_width(), 4);
    }

    #[test]
    fn test_ascii_only_cursor_display_width() {
        let mut ta = TextArea::new();
        // ASCII のみ → cursor_col と表示幅が一致
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Char('b')));
        ta.input(key_event(KeyCode::Char('c')));
        assert_eq!(ta.cursor_col, 3);
        assert_eq!(ta.cursor_display_width(), 3);
    }

    #[test]
    fn test_multikey_sequence_flush_on_mismatch() {
        use crate::keybinding::{KeyBinding, KeySequence};

        // Create textarea with "gg" as submit sequence
        let submit_seq = KeySequence::double(KeyBinding::char('g'), KeyBinding::char('g'));
        let mut ta = TextArea::with_submit_key(submit_seq);

        // Type 'g' - should return PendingSequence
        let action = ta.input(key_event(KeyCode::Char('g')));
        assert!(matches!(action, TextAreaAction::PendingSequence));
        assert_eq!(ta.content(), ""); // Not yet inserted

        // Type 'h' - different key, sequence breaks, both 'g' and 'h' should be inserted
        let action = ta.input(key_event(KeyCode::Char('h')));
        assert!(matches!(action, TextAreaAction::Continue));
        assert_eq!(ta.content(), "gh"); // Both characters inserted
    }

    #[test]
    fn test_multikey_sequence_full_match() {
        use crate::keybinding::{KeyBinding, KeySequence};

        // Create textarea with "gg" as submit sequence
        let submit_seq = KeySequence::double(KeyBinding::char('g'), KeyBinding::char('g'));
        let mut ta = TextArea::with_submit_key(submit_seq);

        // Type 'g' - should return PendingSequence
        let action = ta.input(key_event(KeyCode::Char('g')));
        assert!(matches!(action, TextAreaAction::PendingSequence));

        // Type 'g' again - should return Submit
        let action = ta.input(key_event(KeyCode::Char('g')));
        assert!(matches!(action, TextAreaAction::Submit));
        assert_eq!(ta.content(), ""); // Nothing inserted, it was a submit
    }

    #[test]
    fn test_multikey_sequence_allows_normal_typing_after_mismatch() {
        use crate::keybinding::{KeyBinding, KeySequence};

        let submit_seq = KeySequence::double(KeyBinding::char('g'), KeyBinding::char('g'));
        let mut ta = TextArea::with_submit_key(submit_seq);

        // Type some text normally
        ta.input(key_event(KeyCode::Char('h')));
        ta.input(key_event(KeyCode::Char('e')));
        ta.input(key_event(KeyCode::Char('l')));
        ta.input(key_event(KeyCode::Char('l')));
        ta.input(key_event(KeyCode::Char('o')));

        assert_eq!(ta.content(), "hello");

        // Now try 'g' followed by non-'g' - should insert both
        ta.input(key_event(KeyCode::Char('g')));
        ta.input(key_event(KeyCode::Char('o')));

        assert_eq!(ta.content(), "hellogo");
    }

    #[test]
    fn test_multikey_sequence_backspace_after_partial_match() {
        use crate::keybinding::{KeyBinding, KeySequence};

        // Test case: submit = "gg", type "g" then Backspace
        // Expected: 'g' is inserted, then Backspace removes it
        let submit_seq = KeySequence::double(KeyBinding::char('g'), KeyBinding::char('g'));
        let mut ta = TextArea::with_submit_key(submit_seq);

        // First type some text
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Char('b')));
        assert_eq!(ta.content(), "ab");

        // Type 'g' - should return PendingSequence
        let action = ta.input(key_event(KeyCode::Char('g')));
        assert!(matches!(action, TextAreaAction::PendingSequence));

        // Type Backspace - sequence breaks, 'g' should be inserted, then Backspace should work
        let action = ta.input(key_event(KeyCode::Backspace));
        assert!(matches!(action, TextAreaAction::Continue));
        // 'g' was inserted (from buffer flush), then Backspace removed it
        assert_eq!(ta.content(), "ab");
    }

    #[test]
    fn test_multikey_sequence_arrow_keys_after_partial_match() {
        use crate::keybinding::{KeyBinding, KeySequence};

        // Test case: submit = "gg", type "g" then Left arrow
        // Expected: 'g' is inserted, then cursor moves left
        let submit_seq = KeySequence::double(KeyBinding::char('g'), KeyBinding::char('g'));
        let mut ta = TextArea::with_submit_key(submit_seq);

        // Type some text
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Char('b')));
        assert_eq!(ta.content(), "ab");
        assert_eq!(ta.cursor_col, 2);

        // Type 'g' - should return PendingSequence
        let action = ta.input(key_event(KeyCode::Char('g')));
        assert!(matches!(action, TextAreaAction::PendingSequence));

        // Type Left arrow - sequence breaks, 'g' should be inserted, then cursor moves left
        let action = ta.input(key_event(KeyCode::Left));
        assert!(matches!(action, TextAreaAction::Continue));
        assert_eq!(ta.content(), "abg"); // 'g' was inserted
        assert_eq!(ta.cursor_col, 2); // cursor moved left from position 3 to 2
    }

    #[test]
    fn test_move_to_end_defers_scroll_to_render() {
        let mut ta = TextArea::new();
        // Insert 5 lines so cursor at end is beyond visible window
        ta.set_content("line1\nline2\nline3\nline4\nline5");
        assert_eq!(ta.scroll_offset.get(), 0);
        assert_eq!(ta.cursor_row, 0);

        ta.move_to_end();

        // Cursor should be at last line
        assert_eq!(ta.cursor_row, 4);
        assert_eq!(ta.cursor_col, 5);
        // scroll_offset is reset to 0 — actual scroll adjustment is deferred to render()
        // which sets the correct visible_height first
        assert_eq!(
            ta.scroll_offset.get(),
            0,
            "move_to_end should reset scroll_offset to 0, deferring adjustment to render"
        );

        // After render with real viewport, scroll should be correctly adjusted
        let backend = ratatui::backend::TestBackend::new(40, 5); // height 5 → 3 visible lines
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                ta.render_with_title(frame, area, "Test", "placeholder");
            })
            .unwrap();

        assert_eq!(ta.visible_height.get(), 3);
        // cursor_row=4, visible_height=3, so scroll_offset should be 2 (lines 2,3,4 visible)
        assert_eq!(
            ta.scroll_offset.get(),
            2,
            "render should correctly adjust scroll for cursor at end"
        );
    }

    #[test]
    fn test_render_adjusts_scroll_on_viewport_shrink() {
        let mut ta = TextArea::new();
        // Set content with 10 lines
        ta.set_content("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        // Simulate a tall viewport and render to set visible_height correctly
        ta.visible_height.set(8);
        // Move cursor to end (line 9, 0-indexed)
        ta.move_to_end();
        assert_eq!(ta.cursor_row, 9);
        // move_to_end() resets scroll_offset to 0, so render with tall viewport first
        let backend_tall = ratatui::backend::TestBackend::new(40, 10); // height 10 → 8 visible lines
        let mut terminal_tall = ratatui::Terminal::new(backend_tall).unwrap();
        terminal_tall
            .draw(|frame| {
                let area = frame.area();
                ta.render_with_title(frame, area, "Test", "placeholder");
            })
            .unwrap();
        // After render with height 8, scroll_offset should be 2 (lines 2..10 visible)
        assert_eq!(ta.scroll_offset.get(), 2);

        // Now simulate the terminal shrinking to 3 visible lines
        // Before the fix, render would use the stale scroll_offset=2 with new height=3,
        // showing lines 2,3,4 while cursor is at line 9 — off-screen.
        // After the fix, render calls adjust_scroll() after updating visible_height.
        let backend = ratatui::backend::TestBackend::new(40, 5); // height 5 → 3 visible lines (minus borders)
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                ta.render_with_title(frame, area, "Test", "placeholder");
            })
            .unwrap();

        // After render, scroll_offset must be corrected for the smaller viewport
        // cursor_row=9, visible_height=3, so scroll_offset should be 7 (lines 7,8,9 visible)
        assert_eq!(ta.visible_height.get(), 3);
        assert_eq!(
            ta.scroll_offset.get(),
            7,
            "scroll_offset should be re-adjusted when viewport shrinks during render"
        );
    }

    /// Read the visible (non-border) cells of a TestBackend buffer back as
    /// trimmed strings, one per row, so wrap behavior is asserted on the
    /// actual rendered grid rather than on internal state.
    fn read_inner_rows(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
        let buf = terminal.backend().buffer();
        let area = buf.area;
        (1..area.height - 1)
            .map(|y| {
                let mut s = String::new();
                let mut skip_next = false;
                for x in 1..area.width - 1 {
                    if skip_next {
                        skip_next = false;
                        continue;
                    }
                    let sym = buf[(x, y)].symbol();
                    s.push_str(sym);
                    let w: usize = sym
                        .chars()
                        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                        .sum();
                    if w == 2 {
                        skip_next = true;
                    }
                }
                s.trim_end().to_string()
            })
            .collect()
    }

    fn render_to(
        ta: &TextArea,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                ta.render_with_title(frame, area, "T", "");
            })
            .unwrap();
        terminal
    }

    #[test]
    fn test_long_line_wraps_at_inner_width() {
        // inner_width = 10 (width 12, minus 2 for borders).
        let mut ta = TextArea::new();
        ta.set_content("abcdefghijklmnop"); // 16 chars > inner_width 10
        let term = render_to(&ta, 12, 5);
        let rows = read_inner_rows(&term);
        assert_eq!(rows[0], "abcdefghij");
        assert_eq!(rows[1], "klmnop");
    }

    #[test]
    fn test_typing_past_inner_width_keeps_cursor_visible() {
        let mut ta = TextArea::new();
        // Type 25 chars into a 10-col-wide inner area, viewport tall enough
        // to hold all wrapped rows.
        for c in "abcdefghijklmnopqrstuvwxy".chars() {
            ta.input(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
        }
        let term = render_to(&ta, 12, 6); // visible_height = 4
        let rows = read_inner_rows(&term);
        assert_eq!(rows[0], "abcdefghij");
        assert_eq!(rows[1], "klmnopqrst");
        assert_eq!(rows[2], "uvwxy");
        // 3 wrapped rows fit in visible_height=4, so no scroll needed.
        assert_eq!(ta.scroll_offset.get(), 0);
    }

    #[test]
    fn test_typing_past_visible_height_scrolls() {
        let mut ta = TextArea::new();
        // 35 chars => 4 wrapped rows at inner_width=10. visible_height=2 means
        // the last 2 rows should be visible and scroll_offset should advance.
        for c in "abcdefghijklmnopqrstuvwxyzABCDEFGHI".chars() {
            ta.input(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
        }
        let term = render_to(&ta, 12, 4); // visible_height = 2
        assert_eq!(ta.visible_height.get(), 2);
        // cursor is on the 4th wrapped row (index 3); with visible_height=2
        // scroll_offset is 3+1-2 = 2.
        assert_eq!(ta.scroll_offset.get(), 2);
        let rows = read_inner_rows(&term);
        assert_eq!(rows[0], "uvwxyzABCD");
        assert_eq!(rows[1], "EFGHI");
    }

    #[test]
    fn test_wrap_handles_cjk_double_width() {
        let mut ta = TextArea::new();
        // Each あ is 2 display columns. 6 of them = 12 cols; with inner_width=8
        // the wrap should land after 4 chars (8 cols).
        ta.set_content("ああああああ");
        let term = render_to(&ta, 10, 4); // inner_width = 8
        let rows = read_inner_rows(&term);
        assert_eq!(rows[0], "ああああ");
        assert_eq!(rows[1], "ああ");
    }

    #[test]
    fn test_cursor_absolute_display_row_after_wrap() {
        let mut ta = TextArea::new();
        ta.set_content("abcdefghijklmnop"); // 16 chars
                                            // Render with inner_width=10 to populate inner_width Cell.
        let _ = render_to(&ta, 12, 4);
        // Move cursor to char 12 (on the second wrapped row, col 2).
        ta.cursor_col = 12;
        assert_eq!(ta.cursor_absolute_display_row(), 1);
    }

    #[test]
    fn test_input_before_render_does_not_panic() {
        // inner_width is initialized to 1 before any render. Verify that
        // typing into a fresh TextArea still works and yields sane state.
        let mut ta = TextArea::new();
        for c in "hi".chars() {
            ta.input(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
        }
        assert_eq!(ta.content(), "hi");
    }

    #[test]
    fn test_cursor_lands_on_typed_glyph_after_wrap() {
        // Core invariant of the wrap fix: after typing past the right edge
        // of the box, the terminal cursor must land on the cell holding the
        // most recently typed character — not on the row above it where it
        // would sit if cursor math used logical row index instead of
        // wrapped display row.
        let mut ta = TextArea::new();
        // 12 chars into a 12-wide box (inner_width = 10) wraps to row 2,
        // col 2 within the inner area.
        for c in "abcdefghijkl".chars() {
            ta.input(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
        }
        let mut term = render_to(&ta, 12, 5);
        let pos = term.get_cursor_position().unwrap();
        // border at x=0/y=0; first wrapped row at y=1, cursor on second
        // wrapped row at y=2. cursor_disp_col = 12, inner_width = 10, so
        // x within inner = 2; absolute x = 1 (border) + 2 = 3.
        assert_eq!(pos.x, 3);
        assert_eq!(pos.y, 2);
    }

    #[test]
    fn test_up_arrow_on_wrapped_line_does_not_move_logical_row() {
        // Up/Down navigate logical lines, not visual rows. With wrap, a
        // single logical line can span many visual rows; pressing Up at
        // the end of a wrapped line on row 0 must be a no-op, since there
        // is no logical line above it.
        let mut ta = TextArea::new();
        ta.set_content("abcdefghijklmnopqrstuvwxyz"); // wraps at inner=10
                                                      // Render so inner_width is set; place cursor at end of logical line.
        let _ = render_to(&ta, 12, 6);
        ta.input(key_event(KeyCode::End));
        assert_eq!(ta.cursor_row, 0);
        ta.input(key_event(KeyCode::Up));
        assert_eq!(ta.cursor_row, 0, "Up must not navigate visual rows");
    }

    #[test]
    fn test_wrap_preserves_span_styles_across_boundary() {
        // The styled-line render path is what suggestion-input uses for
        // syntax highlighting. Styles must survive the split when a single
        // span crosses a wrap boundary, and a span boundary that happens
        // to coincide with a wrap boundary must not corrupt either side.
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("aaaaaaaa".to_string(), red), // 8 cols, red
            Span::styled("bbbbbb".to_string(), blue),  // 6 cols, blue (wraps)
        ]);
        let rows = wrap_line_to_rows(&line, 10);
        assert_eq!(rows.len(), 2, "expected wrap into 2 rows");

        // Row 0: "aaaaaaaa" (red) + "bb" (blue) — fits 10 cols exactly.
        assert_eq!(rows[0].spans.len(), 2);
        assert_eq!(rows[0].spans[0].content, "aaaaaaaa");
        assert_eq!(rows[0].spans[0].style, red);
        assert_eq!(rows[0].spans[1].content, "bb");
        assert_eq!(rows[0].spans[1].style, blue);

        // Row 1: remaining 4 b's, blue.
        assert_eq!(rows[1].spans.len(), 1);
        assert_eq!(rows[1].spans[0].content, "bbbb");
        assert_eq!(rows[1].spans[0].style, blue);
    }

    #[test]
    fn test_empty_text_area_renders_placeholder() {
        let ta = TextArea::new();
        let term = render_to(&ta, 30, 4);
        let rows = read_inner_rows(&term);
        // render_to passes "" as placeholder, which means an empty visible
        // first row. Use a non-empty placeholder to check the path.
        assert!(rows[0].is_empty(), "empty placeholder yields empty row");

        let backend = ratatui::backend::TestBackend::new(30, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                ta.render_with_title(frame, area, "T", "Type your reply...");
            })
            .unwrap();
        let rows = read_inner_rows(&terminal);
        assert_eq!(rows[0], "Type your reply...");
    }
}
