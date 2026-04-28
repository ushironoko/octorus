use std::cell::Cell;

use crossterm::event::{self, KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthChar;

use crate::keybinding::{event_to_keybinding, KeySequence, SequenceMatch, SequenceState};

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

        // Emacs-style readline bindings. Handled before the generic Char arm
        // so Ctrl-<key> doesn't fall through and get inserted as plain text.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                match c {
                    'a' => self.cursor_col = 0,
                    'e' => self.cursor_col = self.current_line_len(),
                    'b' => self.move_left(),
                    'f' => self.move_right(),
                    'p' => self.move_up(),
                    'n' => self.move_down(),
                    'd' => self.delete(),
                    'h' => self.backspace(),
                    'k' => self.kill_to_line_end(),
                    'u' => self.kill_to_line_start(),
                    'w' => self.delete_word_backward(),
                    // Unhandled Ctrl combos are swallowed rather than inserted
                    // as raw characters.
                    _ => {}
                }
                self.adjust_scroll();
                return TextAreaAction::Continue;
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
        self.visible_height.set(visible_height);
        // visible_height が変わった可能性があるのでスクロールを再調整
        // （ターミナルリサイズで高さが縮小した場合、stale な scroll_offset でカーソルが画面外になる）
        self.adjust_scroll();

        let scroll_offset = self.scroll_offset.get();

        // styled_lines がある場合は Line<'static> をそのまま clone（Cow の共有で済む）、
        // ない場合はプレーンテキストから構築
        let text: Vec<Line<'static>> = if let Some(styled) = styled_lines {
            styled
                .iter()
                .skip(scroll_offset)
                .take(visible_height)
                .cloned()
                .collect()
        } else {
            self.lines
                .iter()
                .skip(scroll_offset)
                .take(visible_height)
                .map(|l| Line::from(l.to_owned()))
                .collect()
        };

        let placeholder_style = Style::default().fg(Color::DarkGray);
        let display_text: Vec<Line<'static>> = if self.is_empty() {
            vec![Line::styled(placeholder.to_owned(), placeholder_style)]
        } else {
            text
        };

        let paragraph =
            Paragraph::new(display_text).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(paragraph, area);

        // カーソル表示（CJK文字の表示幅を考慮）
        let cursor_x = area.x + 1 + self.cursor_display_width() as u16;
        let cursor_y = area.y + 1 + (self.cursor_row.saturating_sub(scroll_offset)) as u16;
        if cursor_y < area.y + area.height.saturating_sub(1) {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
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

    /// Kill from cursor to end of current line (emacs Ctrl-K). Joins with
    /// the next line when the cursor is already at end-of-line.
    fn kill_to_line_end(&mut self) {
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            let line = &mut self.lines[self.cursor_row];
            let byte_idx = char_to_byte_index(line, self.cursor_col);
            line.truncate(byte_idx);
        } else if self.cursor_row + 1 < self.lines.len() {
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        }
    }

    /// Kill from start of current line up to cursor (emacs Ctrl-U).
    fn kill_to_line_start(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        let line = &mut self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        line.drain(..byte_idx);
        self.cursor_col = 0;
    }

    /// Delete the word before the cursor (emacs Ctrl-W). Skips trailing
    /// whitespace, then deletes back to the previous whitespace boundary.
    /// At column 0 falls back to a regular backspace (joins previous line).
    fn delete_word_backward(&mut self) {
        if self.cursor_col == 0 {
            self.backspace();
            return;
        }
        let chars: Vec<char> = self.lines[self.cursor_row].chars().collect();
        let mut i = self.cursor_col;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let line = &mut self.lines[self.cursor_row];
        let start_byte = char_to_byte_index(line, i);
        let end_byte = char_to_byte_index(line, self.cursor_col);
        line.drain(start_byte..end_byte);
        self.cursor_col = i;
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
        // スクロール調整: カーソルが見えるように（render()で設定された実際の可視高さを使用）
        let visible_height = self.visible_height.get();
        let scroll_offset = self.scroll_offset.get();
        if self.cursor_row < scroll_offset {
            self.scroll_offset.set(self.cursor_row);
        }
        if self.cursor_row >= scroll_offset + visible_height {
            self.scroll_offset
                .set(self.cursor_row.saturating_sub(visible_height) + 1);
        }
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
    fn test_emacs_ctrl_a_jumps_to_line_start() {
        let mut ta = TextArea::new();
        ta.set_content("hello world");
        ta.input(key_event(KeyCode::End));
        assert_eq!(ta.cursor_col, 11);
        ta.input(ctrl_key_event(KeyCode::Char('a')));
        assert_eq!(ta.cursor_col, 0);
        assert_eq!(ta.content(), "hello world");
    }

    #[test]
    fn test_emacs_ctrl_e_jumps_to_line_end() {
        let mut ta = TextArea::new();
        ta.set_content("hello world");
        ta.input(ctrl_key_event(KeyCode::Char('e')));
        assert_eq!(ta.cursor_col, 11);
    }

    #[test]
    fn test_emacs_ctrl_b_f_move_chars() {
        let mut ta = TextArea::new();
        ta.set_content("abc");
        ta.input(ctrl_key_event(KeyCode::Char('e')));
        ta.input(ctrl_key_event(KeyCode::Char('b')));
        assert_eq!(ta.cursor_col, 2);
        ta.input(ctrl_key_event(KeyCode::Char('f')));
        assert_eq!(ta.cursor_col, 3);
    }

    #[test]
    fn test_emacs_ctrl_p_n_move_lines() {
        let mut ta = TextArea::new();
        ta.set_content("a\nb\nc");
        ta.input(ctrl_key_event(KeyCode::Char('n')));
        assert_eq!(ta.cursor_row, 1);
        ta.input(ctrl_key_event(KeyCode::Char('p')));
        assert_eq!(ta.cursor_row, 0);
    }

    #[test]
    fn test_emacs_ctrl_d_deletes_forward() {
        let mut ta = TextArea::new();
        ta.set_content("abc");
        ta.input(ctrl_key_event(KeyCode::Char('d')));
        assert_eq!(ta.content(), "bc");
    }

    #[test]
    fn test_emacs_ctrl_h_backspaces() {
        let mut ta = TextArea::new();
        ta.input(key_event(KeyCode::Char('a')));
        ta.input(key_event(KeyCode::Char('b')));
        ta.input(ctrl_key_event(KeyCode::Char('h')));
        assert_eq!(ta.content(), "a");
    }

    #[test]
    fn test_emacs_ctrl_k_kills_to_line_end() {
        let mut ta = TextArea::new();
        ta.set_content("hello world");
        for _ in 0..5 {
            ta.input(key_event(KeyCode::Right));
        }
        ta.input(ctrl_key_event(KeyCode::Char('k')));
        assert_eq!(ta.content(), "hello");
    }

    #[test]
    fn test_emacs_ctrl_k_at_eol_joins_next_line() {
        let mut ta = TextArea::new();
        ta.set_content("ab\ncd");
        ta.input(key_event(KeyCode::End));
        ta.input(ctrl_key_event(KeyCode::Char('k')));
        assert_eq!(ta.content(), "abcd");
    }

    #[test]
    fn test_emacs_ctrl_u_kills_to_line_start() {
        let mut ta = TextArea::new();
        ta.set_content("hello world");
        for _ in 0..6 {
            ta.input(key_event(KeyCode::Right));
        }
        ta.input(ctrl_key_event(KeyCode::Char('u')));
        assert_eq!(ta.content(), "world");
        assert_eq!(ta.cursor_col, 0);
    }

    #[test]
    fn test_emacs_ctrl_w_deletes_previous_word() {
        let mut ta = TextArea::new();
        ta.set_content("hello world foo");
        ta.input(ctrl_key_event(KeyCode::Char('e')));
        ta.input(ctrl_key_event(KeyCode::Char('w')));
        assert_eq!(ta.content(), "hello world ");
        ta.input(ctrl_key_event(KeyCode::Char('w')));
        assert_eq!(ta.content(), "hello ");
    }

    #[test]
    fn test_emacs_ctrl_w_at_line_start_joins() {
        let mut ta = TextArea::new();
        ta.set_content("ab\ncd");
        ta.input(key_event(KeyCode::Down));
        assert_eq!(ta.cursor_row, 1);
        assert_eq!(ta.cursor_col, 0);
        ta.input(ctrl_key_event(KeyCode::Char('w')));
        assert_eq!(ta.content(), "abcd");
    }

    #[test]
    fn test_emacs_ctrl_d_at_eol_joins_lines() {
        let mut ta = TextArea::new();
        ta.set_content("ab\ncd");
        ta.input(key_event(KeyCode::End));
        assert_eq!(ta.cursor_col, 2);
        ta.input(ctrl_key_event(KeyCode::Char('d')));
        assert_eq!(ta.content(), "abcd");
        assert_eq!(ta.cursor_row, 0);
        assert_eq!(ta.cursor_col, 2);
    }

    #[test]
    fn test_emacs_ctrl_k_at_end_of_buffer_is_noop() {
        let mut ta = TextArea::new();
        ta.set_content("abc");
        ta.input(key_event(KeyCode::End));
        ta.input(ctrl_key_event(KeyCode::Char('k')));
        assert_eq!(ta.content(), "abc");
        assert_eq!(ta.cursor_col, 3);
    }

    #[test]
    fn test_emacs_ctrl_w_handles_consecutive_whitespace() {
        // Readline-style Ctrl-W (matches bash unix-word-rubout): from inside
        // a word, delete only the word; from inside trailing whitespace,
        // delete the whitespace then back to the previous word boundary.
        let mut ta = TextArea::new();
        ta.set_content("foo   bar");
        ta.input(ctrl_key_event(KeyCode::Char('e')));
        // First C-w from end-of-line: cursor sits past 'r', no trailing
        // whitespace to skip, deletes only "bar".
        ta.input(ctrl_key_event(KeyCode::Char('w')));
        assert_eq!(ta.content(), "foo   ");
        assert_eq!(ta.cursor_col, 6);
        // Second C-w: cursor sits in trailing whitespace, skip the spaces
        // then delete the word "foo".
        ta.input(ctrl_key_event(KeyCode::Char('w')));
        assert_eq!(ta.content(), "");
    }

    #[test]
    fn test_emacs_ctrl_w_treats_punctuation_as_part_of_word() {
        // Readline-style semantics (matches bash unix-word-rubout): only
        // whitespace bounds a word, so Ctrl-W on a dotted path takes the
        // entire path. Diverges intentionally from GNU emacs
        // backward-kill-word, which would only delete "baz".
        let mut ta = TextArea::new();
        ta.set_content("hello foo.bar.baz");
        ta.input(ctrl_key_event(KeyCode::Char('e')));
        ta.input(ctrl_key_event(KeyCode::Char('w')));
        assert_eq!(ta.content(), "hello ");
    }

    #[test]
    fn test_emacs_ctrl_w_with_multibyte_word() {
        let mut ta = TextArea::new();
        ta.set_content("hello あいう");
        ta.input(ctrl_key_event(KeyCode::Char('e')));
        // cursor_col should be 9 (5 ascii + 1 space + 3 CJK chars).
        assert_eq!(ta.cursor_col, 9);
        ta.input(ctrl_key_event(KeyCode::Char('w')));
        assert_eq!(ta.content(), "hello ");
        assert_eq!(ta.cursor_col, 6);
    }

    #[test]
    fn test_default_ctrl_s_still_submits_after_emacs_handler() {
        // Regression guard: the new Ctrl-handler must not swallow the default
        // Ctrl-S submit binding. check_single_key_submit fires first.
        let mut ta = TextArea::new();
        let action = ta.input(ctrl_key_event(KeyCode::Char('s')));
        assert!(matches!(action, TextAreaAction::Submit));
        // And Ctrl-S did not also insert 's' as a side effect.
        assert_eq!(ta.content(), "");
    }

    #[test]
    fn test_emacs_ctrl_a_returns_to_logical_line_start_when_wrapped() {
        // Ctrl-A is a logical-line operation, not a visual-row operation.
        // With wrapping enabled (commit 1), a long line spans multiple display
        // rows; Ctrl-A must still snap to col 0 of the logical line.
        let mut ta = TextArea::new();
        ta.set_content("abcdefghijklmnopqrstuvwxyz"); // 26 chars
        ta.input(key_event(KeyCode::End));
        assert_eq!(ta.cursor_col, 26);
        ta.input(ctrl_key_event(KeyCode::Char('a')));
        assert_eq!(ta.cursor_col, 0);
        assert_eq!(ta.cursor_row, 0);
    }

    #[test]
    fn test_ctrl_unhandled_does_not_insert() {
        let mut ta = TextArea::new();
        // Ctrl-X is not bound; previously fell through to insert_char and
        // would have inserted 'x'. After the emacs binding refactor it is
        // swallowed.
        ta.input(ctrl_key_event(KeyCode::Char('x')));
        assert_eq!(ta.content(), "");
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
}
