use anyhow::Result;
use crossterm::event::{self, KeyCode, KeyModifiers};
use tokio::sync::mpsc;

use super::types::{ShellCommandResult, ShellPhase, ShellState};
use super::App;

const MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1MB

impl App {
    pub(crate) fn enter_shell_command_mode(&mut self) {
        self.shell_state = Some(ShellState {
            input: String::new(),
            cursor: 0,
            phase: ShellPhase::Input,
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

        tokio::spawn(async move {
            #[cfg(target_os = "windows")]
            let (shell_bin, flag) = ("cmd", "/C");
            #[cfg(not(target_os = "windows"))]
            let (shell_bin, flag) = ("sh", "-c");

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
                    if stdout.len() > MAX_OUTPUT_BYTES {
                        stdout.truncate(MAX_OUTPUT_BYTES);
                        stdout.push_str("\n... (output truncated)");
                    }
                    if stderr.len() > MAX_OUTPUT_BYTES {
                        stderr.truncate(MAX_OUTPUT_BYTES);
                        stderr.push_str("\n... (output truncated)");
                    }
                    ShellCommandResult {
                        command,
                        stdout,
                        stderr,
                        exit_code: output.status.code(),
                        scroll_offset: 0,
                    }
                }
                Err(e) => ShellCommandResult {
                    command,
                    stdout: String::new(),
                    stderr: format!("Failed to execute: {}", e),
                    exit_code: None,
                    scroll_offset: 0,
                },
            };
            let _ = tx.send(shell_result).await;
        });
    }

    pub(crate) fn handle_shell_output(&mut self, key: event::KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.shell_state = None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ShellState {
                    phase: ShellPhase::Done(ref mut result),
                    ..
                }) = self.shell_state
                {
                    result.scroll_offset = result.scroll_offset.saturating_add(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ShellState {
                    phase: ShellPhase::Done(ref mut result),
                    ..
                }) = self.shell_state
                {
                    result.scroll_offset = result.scroll_offset.saturating_sub(1);
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ShellState {
                    phase: ShellPhase::Done(ref mut result),
                    ..
                }) = self.shell_state
                {
                    result.scroll_offset = result.scroll_offset.saturating_add(10);
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ShellState {
                    phase: ShellPhase::Done(ref mut result),
                    ..
                }) = self.shell_state
                {
                    result.scroll_offset = result.scroll_offset.saturating_sub(10);
                }
            }
            KeyCode::Char('g') => {
                if let Some(ShellState {
                    phase: ShellPhase::Done(ref mut result),
                    ..
                }) = self.shell_state
                {
                    result.scroll_offset = 0;
                }
            }
            KeyCode::Char('G') => {
                if let Some(ShellState {
                    phase: ShellPhase::Done(ref mut result),
                    ..
                }) = self.shell_state
                {
                    let total_lines = result.stdout.lines().count()
                        + if result.stderr.is_empty() {
                            0
                        } else {
                            result.stderr.lines().count() + 1
                        };
                    result.scroll_offset = total_lines.saturating_sub(1);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn poll_shell_result(&mut self) {
        let Some(ref mut rx) = self.shell_result_receiver else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                if let Some(ref mut shell) = self.shell_state {
                    shell.phase = ShellPhase::Done(result);
                }
                self.shell_result_receiver = None;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                self.shell_result_receiver = None;
            }
        }
    }

    pub(crate) fn cancel_shell_command(&mut self) {
        self.shell_state = None;
        self.shell_result_receiver = None;
    }
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
        // app.state should not change (overlay pattern)
        assert_eq!(app.state, original_state);
    }

    #[test]
    fn test_shell_input_char_insertion() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('l'))).unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('s'))).unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "ls");
        assert_eq!(shell.cursor, 2);
    }

    #[test]
    fn test_shell_input_backspace() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('a'))).unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('b'))).unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('c'))).unwrap();
        app.handle_shell_input(make_key(KeyCode::Backspace)).unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "ab");
        assert_eq!(shell.cursor, 2);
    }

    #[test]
    fn test_shell_input_cursor_movement() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('a'))).unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('b'))).unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('c'))).unwrap();

        // Left
        app.handle_shell_input(make_key(KeyCode::Left)).unwrap();
        assert_eq!(app.shell_state.as_ref().unwrap().cursor, 2);

        // Home
        app.handle_shell_input(make_key(KeyCode::Home)).unwrap();
        assert_eq!(app.shell_state.as_ref().unwrap().cursor, 0);

        // Right
        app.handle_shell_input(make_key(KeyCode::Right)).unwrap();
        assert_eq!(app.shell_state.as_ref().unwrap().cursor, 1);

        // End
        app.handle_shell_input(make_key(KeyCode::End)).unwrap();
        assert_eq!(app.shell_state.as_ref().unwrap().cursor, 3);
    }

    #[test]
    fn test_shell_input_ctrl_u_clear() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('a'))).unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('b'))).unwrap();
        app.handle_shell_input(make_ctrl_key(KeyCode::Char('u'))).unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert!(shell.input.is_empty());
        assert_eq!(shell.cursor, 0);
    }

    #[test]
    fn test_shell_input_esc_cancels() {
        let mut app = App::new_for_test();
        app.enter_shell_command_mode();

        app.handle_shell_input(make_key(KeyCode::Char('x'))).unwrap();
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

        app.handle_shell_input(make_key(KeyCode::Char('日'))).unwrap();
        app.handle_shell_input(make_key(KeyCode::Char('本'))).unwrap();

        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "日本");
        assert_eq!(shell.cursor, 2);

        app.handle_shell_input(make_key(KeyCode::Backspace)).unwrap();
        let shell = app.shell_state.as_ref().unwrap();
        assert_eq!(shell.input, "日");
        assert_eq!(shell.cursor, 1);
    }
}
