use anyhow::Result;
use crossterm::event::{self, KeyCode, KeyModifiers};
use tokio::sync::mpsc;

use super::types::{CachedShellLine, ShellCommandResult, ShellPhase, ShellState};
use super::App;

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

impl App {
    pub(crate) fn enter_shell_command_mode(&mut self) {
        self.shell_state = Some(ShellState {
            input: String::new(),
            cursor: 0,
            phase: ShellPhase::Input,
            scroll_offset: 0,
        });
    }

    pub(crate) fn handle_shell_input(&mut self, key: event::KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.shell_state = None;
            }
            KeyCode::Enter => {
                self.start_shell_command();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ref mut shell) = self.shell_state {
                    shell.input.clear();
                    shell.cursor = 0;
                }
            }
            KeyCode::Char(c) => {
                if let Some(ref mut shell) = self.shell_state {
                    let byte_pos = shell
                        .input
                        .char_indices()
                        .nth(shell.cursor)
                        .map(|(i, _)| i)
                        .unwrap_or(shell.input.len());
                    shell.input.insert(byte_pos, c);
                    shell.cursor += 1;
                }
            }
            KeyCode::Backspace => {
                if let Some(ref mut shell) = self.shell_state {
                    if shell.cursor > 0 {
                        shell.cursor -= 1;
                        let byte_pos = shell
                            .input
                            .char_indices()
                            .nth(shell.cursor)
                            .map(|(i, _)| i)
                            .unwrap_or(shell.input.len());
                        let next_byte = shell
                            .input
                            .char_indices()
                            .nth(shell.cursor + 1)
                            .map(|(i, _)| i)
                            .unwrap_or(shell.input.len());
                        shell.input.replace_range(byte_pos..next_byte, "");
                    }
                }
            }
            KeyCode::Delete => {
                if let Some(ref mut shell) = self.shell_state {
                    let max = shell.input.chars().count();
                    if shell.cursor < max {
                        let byte_pos = shell
                            .input
                            .char_indices()
                            .nth(shell.cursor)
                            .map(|(i, _)| i)
                            .unwrap_or(shell.input.len());
                        let next_byte = shell
                            .input
                            .char_indices()
                            .nth(shell.cursor + 1)
                            .map(|(i, _)| i)
                            .unwrap_or(shell.input.len());
                        shell.input.replace_range(byte_pos..next_byte, "");
                    }
                }
            }
            KeyCode::Left => {
                if let Some(ref mut shell) = self.shell_state {
                    shell.cursor = shell.cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let Some(ref mut shell) = self.shell_state {
                    let max = shell.input.chars().count();
                    if shell.cursor < max {
                        shell.cursor += 1;
                    }
                }
            }
            KeyCode::Home => {
                if let Some(ref mut shell) = self.shell_state {
                    shell.cursor = 0;
                }
            }
            KeyCode::End => {
                if let Some(ref mut shell) = self.shell_state {
                    shell.cursor = shell.input.chars().count();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn start_shell_command(&mut self) {
        let command = match self.shell_state.as_ref() {
            Some(shell) if !shell.input.trim().is_empty() => shell.input.clone(),
            _ => {
                self.shell_state = None;
                return;
            }
        };

        if let Some(ref mut shell) = self.shell_state {
            shell.phase = ShellPhase::Running;
        }

        let working_dir = self.working_dir.clone().unwrap_or_else(|| ".".to_string());
        let (tx, rx) = mpsc::channel(1);
        self.shell_result_receiver = Some(rx);

        #[cfg(target_os = "windows")]
        let (shell_bin, flag) = ("cmd", "/C");
        #[cfg(not(target_os = "windows"))]
        let (shell_bin, flag) = ("sh", "-c");

        let handle = tokio::spawn(async move {
            let result = tokio::process::Command::new(shell_bin)
                .arg(flag)
                .arg(&command)
                .current_dir(&working_dir)
                .output()
                .await;

            let shell_result = match result {
                Ok(output) => {
                    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                    truncate_at_char_boundary(&mut stdout, MAX_OUTPUT_BYTES);
                    truncate_at_char_boundary(&mut stderr, MAX_OUTPUT_BYTES);

                    let (cached_lines, total_lines) =
                        build_cached_lines(&stdout, &stderr);

                    ShellCommandResult {
                        command,
                        stdout,
                        stderr,
                        exit_code: output.status.code(),
                        cached_lines,
                        total_lines,
                    }
                }
                Err(e) => {
                    let stderr = format!("Failed to execute: {}", e);
                    let (cached_lines, total_lines) =
                        build_cached_lines("", &stderr);
                    ShellCommandResult {
                        command,
                        stdout: String::new(),
                        stderr,
                        exit_code: None,
                        cached_lines,
                        total_lines,
                    }
                }
            };
            let _ = tx.send(shell_result).await;
        });
        self.shell_abort_handle = Some(handle.abort_handle());
    }

    pub(crate) fn handle_shell_output(&mut self, key: event::KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.shell_state = None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.shell_scroll_by(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.shell_scroll_by(-1);
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.shell_scroll_by(10);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.shell_scroll_by(-10);
            }
            KeyCode::Char('g') => {
                if let Some(ref mut shell) = self.shell_state {
                    shell.scroll_offset = 0;
                }
            }
            KeyCode::Char('G') => {
                if let Some(ref mut shell) = self.shell_state {
                    if let ShellPhase::Done(ref result) = shell.phase {
                        shell.scroll_offset = result.total_lines.saturating_sub(1);
                    }
                }
            }
            _ => {}
        }
    }

    fn shell_scroll_by(&mut self, delta: i32) {
        let Some(ref mut shell) = self.shell_state else {
            return;
        };
        let ShellPhase::Done(ref result) = shell.phase else {
            return;
        };
        let max = result.total_lines.saturating_sub(1);
        if delta > 0 {
            shell.scroll_offset = shell
                .scroll_offset
                .saturating_add(delta as usize)
                .min(max);
        } else {
            shell.scroll_offset = shell.scroll_offset.saturating_sub((-delta) as usize);
        }
    }

    pub(crate) fn poll_shell_result(&mut self) {
        let Some(ref mut rx) = self.shell_result_receiver else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                if let Some(ref mut shell) = self.shell_state {
                    shell.scroll_offset = 0;
                    shell.phase = ShellPhase::Done(result);
                }
                self.shell_result_receiver = None;
                self.shell_abort_handle = None;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                self.shell_result_receiver = None;
                self.shell_abort_handle = None;
            }
        }
    }

    pub(crate) fn cancel_shell_command(&mut self) {
        if let Some(handle) = self.shell_abort_handle.take() {
            handle.abort();
        }
        self.shell_state = None;
        self.shell_result_receiver = None;
        self.cmt.submission_result = Some((false, "Shell command cancelled".to_string()));
        self.cmt.submission_result_time = Some(std::time::Instant::now());
    }
}

fn truncate_at_char_boundary(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("\n... (output truncated)");
}

fn build_cached_lines(stdout: &str, stderr: &str) -> (Vec<CachedShellLine>, usize) {
    let mut lines = Vec::new();
    for line in stdout.lines() {
        lines.push(CachedShellLine {
            text: line.to_string(),
            is_stderr: false,
        });
    }
    if !stderr.is_empty() {
        if !stdout.is_empty() {
            lines.push(CachedShellLine {
                text: "--- stderr ---".to_string(),
                is_stderr: true,
            });
        }
        for line in stderr.lines() {
            lines.push(CachedShellLine {
                text: line.to_string(),
                is_stderr: true,
            });
        }
    }
    if lines.is_empty() {
        lines.push(CachedShellLine {
            text: "(no output)".to_string(),
            is_stderr: false,
        });
    }
    let total = lines.len();
    (lines, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn make_ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn test_enter_shell_command_mode() {
        let mut app = App::new_for_test();
        let original_state = app.state;
        app.enter_shell_command_mode();

        assert!(app.shell_state.is_some());
        let shell = app.shell_state.as_ref().unwrap();
        assert!(shell.input.is_empty());
        assert_eq!(shell.cursor, 0);
        assert!(matches!(shell.phase, ShellPhase::Input));
        assert_eq!(app.state, original_state);
    }

    #[test]
    fn test_shell_input_char_insertion() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('l')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('s')))
            .unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "ls");
        assert_eq!(shell.cursor, 2);
    }

    #[test]
    fn test_shell_input_backspace() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('a')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('b')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('c')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Backspace))
            .unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "ab");
        assert_eq!(shell.cursor, 2);
    }

    #[test]
    fn test_shell_input_delete() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('a')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('b')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('c')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Home)).unwrap();
        app.handle_shell_input(make_key(KeyCode::Delete)).unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "bc");
        assert_eq!(shell.cursor, 0);
    }

    #[test]
    fn test_shell_input_cursor_movement() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('a')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('b')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('c')))
            .unwrap();

        app.handle_shell_input(make_key(KeyCode::Left)).unwrap();
        assert_eq!(app.shell_state.as_ref().unwrap().cursor, 2);

        app.handle_shell_input(make_key(KeyCode::Home)).unwrap();
        assert_eq!(app.shell_state.as_ref().unwrap().cursor, 0);

        app.handle_shell_input(make_key(KeyCode::Right)).unwrap();
        assert_eq!(app.shell_state.as_ref().unwrap().cursor, 1);

        app.handle_shell_input(make_key(KeyCode::End)).unwrap();
        assert_eq!(app.shell_state.as_ref().unwrap().cursor, 3);
    }

    #[test]
    fn test_shell_input_ctrl_u_clear() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('a')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('b')))
            .unwrap();
        app.handle_shell_input(make_ctrl_key(KeyCode::Char('u')))
            .unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert!(shell.input.is_empty());
        assert_eq!(shell.cursor, 0);
    }

    #[test]
    fn test_shell_input_esc_cancels() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('x')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Esc)).unwrap();

        assert!(app.shell_state.is_none());
    }

    #[test]
    fn test_shell_input_empty_enter_closes() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Enter)).unwrap();

        assert!(app.shell_state.is_none());
    }

    #[test]
    fn test_shell_input_unicode() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('日')))
            .unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('本')))
            .unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "日本");
        assert_eq!(shell.cursor, 2);

        app.handle_shell_input(make_key(KeyCode::Backspace))
            .unwrap();
        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "日");
        assert_eq!(shell.cursor, 1);
    }

    #[test]
    fn test_truncate_at_char_boundary_ascii() {
        let mut s = "hello world".to_string();
        truncate_at_char_boundary(&mut s, 5);
        assert_eq!(s, "hello\n... (output truncated)");
    }

    #[test]
    fn test_truncate_at_char_boundary_multibyte() {
        let mut s = "あいうえお".to_string(); // 15 bytes (3 bytes per char)
        truncate_at_char_boundary(&mut s, 7); // mid-char boundary
        assert_eq!(s, "あい\n... (output truncated)"); // backs up to byte 6
    }

    #[test]
    fn test_truncate_at_char_boundary_no_truncation() {
        let mut s = "short".to_string();
        truncate_at_char_boundary(&mut s, 100);
        assert_eq!(s, "short");
    }

    #[test]
    fn test_build_cached_lines_stdout_only() {
        let (lines, total) = build_cached_lines("line1\nline2", "");
        assert_eq!(total, 2);
        assert!(!lines[0].is_stderr);
        assert_eq!(lines[0].text, "line1");
    }

    #[test]
    fn test_build_cached_lines_both() {
        let (lines, total) = build_cached_lines("out", "err");
        assert_eq!(total, 3); // out + separator + err
        assert!(!lines[0].is_stderr);
        assert!(lines[1].is_stderr); // separator
        assert!(lines[2].is_stderr);
    }

    #[test]
    fn test_build_cached_lines_empty() {
        let (lines, total) = build_cached_lines("", "");
        assert_eq!(total, 1);
        assert_eq!(lines[0].text, "(no output)");
    }

    #[test]
    fn test_shell_scroll_clamped() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        let result = ShellCommandResult {
            command: "test".to_string(),
            stdout: "a\nb\nc".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            cached_lines: vec![
                CachedShellLine { text: "a".into(), is_stderr: false },
                CachedShellLine { text: "b".into(), is_stderr: false },
                CachedShellLine { text: "c".into(), is_stderr: false },
            ],
            total_lines: 3,
        };
        if let Some(ref mut shell) = app.shell_state {
            shell.phase = ShellPhase::Done(result);
        }

        // Scroll past end should clamp to max (total_lines - 1 = 2)
        for _ in 0..10 {
            app.handle_shell_output(make_key(KeyCode::Char('j')));
        }
        assert_eq!(app.shell_state.as_ref().unwrap().scroll_offset, 2);
    }
}
