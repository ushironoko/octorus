//! A malformed `theme` value must not stop startup.
//!
//! `Config::load_from_paths` is the result a caller can read back. That is the
//! theme that survived, and every unrelated key. The `or` process is what the
//! user sees at startup. Its stderr carries the warning, and the
//! alternate-screen sequence means config loading returned.

use assert_cmd::cargo::cargo_bin_cmd;
use octorus::config::{Config, ThemePreset};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
const ALTERNATE_SCREEN_CSI: &str = "\u{1b}[?1049";

const INTEGER_TOML: &str = "theme = 5\n\n[diff]\ntheme = \"Nord\"\n";
const INTEGER_WARNING: &str = "theme: expected a table, got integer \"5\" (using default)";

const STRING_TOML: &str = "theme = \"Dracula\"\n\n[diff]\ntheme = \"Nord\"\n";
const STRING_WARNING: &str = "theme: expected a table, got string \"Dracula\"; did you mean [diff] theme = \"...\"? (using default)";

const COLORS_TOML: &str = "[diff]\ntheme = \"Nord\"\n\n[theme]\ncolors = \"red\"\n";
const COLORS_WARNING: &str = "theme.colors: expected a table, got string \"red\" (using default)";

enum Placement {
    Global,
    Local,
    Both,
}

struct Session {
    _dir: tempfile::TempDir,
    config: Config,
    stdout: String,
    stderr: String,
    global_path: PathBuf,
    local_path: PathBuf,
}

fn start(global_toml: Option<&str>, local_toml: Option<&str>) -> Session {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cwd = dir.path().to_path_buf();
    let home = cwd.join("home");
    let config_home = cwd.join("config");
    let cache_home = cwd.join("cache");
    for path in [&home, &config_home, &cache_home] {
        fs::create_dir_all(path).expect("create isolated directory");
    }

    let global_path = config_home.join("octorus").join("config.toml");
    let local_path = cwd.join(".octorus").join("config.toml");
    write_optional(&global_path, global_toml);
    write_optional(&local_path, local_toml);

    let output = cargo_bin_cmd!("or")
        .arg("--browse")
        .current_dir(&cwd)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .timeout(std::time::Duration::from_secs(3))
        .output()
        .expect("run or");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let config = Config::load_from_paths(&global_path, &local_path, cwd).unwrap_or_else(|error| {
        panic!(
            "config load should succeed for a malformed theme: {error:#}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )
    });

    Session {
        _dir: dir,
        stdout,
        stderr,
        config,
        global_path,
        local_path,
    }
}

fn write_optional(path: &Path, body: Option<&str>) {
    let Some(body) = body else {
        return;
    };
    fs::create_dir_all(path.parent().expect("config path has a parent"))
        .expect("create config dir");
    fs::write(path, body).expect("write config");
}

fn warning_line(path: &Path, body: &str) -> String {
    format!("Warning: {}: {body}", path.display())
}

fn warning_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.starts_with("Warning:"))
        .map(str::to_string)
        .collect()
}

fn assert_startup(session: &Session) {
    assert!(
        session.stdout.contains(ALTERNATE_SCREEN_CSI),
        "startup did not reach the TUI\nstdout:\n{}\nstderr:\n{}",
        session.stdout,
        session.stderr
    );
    assert!(
        !session.stderr.contains("Failed to deserialize"),
        "config deserialization stopped startup\nstderr:\n{}",
        session.stderr
    );
    assert!(
        !session.stderr.contains("panicked"),
        "startup panicked\nstderr:\n{}",
        session.stderr
    );
}

fn assert_shape(placement: Placement, toml: &str, warning_body: &str) {
    let (global, local) = match placement {
        Placement::Global => (Some(toml), None),
        Placement::Local => (None, Some(toml)),
        Placement::Both => (Some(toml), Some(toml)),
    };
    let session = start(global, local);
    assert_startup(&session);
    assert!(
        session.config.theme.is_untouched(),
        "theme should fall back to the default, got {:?}",
        session.config.theme
    );
    assert_eq!(session.config.diff.theme, "Nord");

    let mut expected = Vec::new();
    if global.is_some() {
        expected.push(warning_line(&session.global_path, warning_body));
    }
    if local.is_some() {
        expected.push(warning_line(&session.local_path, warning_body));
    }
    assert_eq!(
        warning_lines(&session.stderr),
        expected,
        "stderr:\n{}",
        session.stderr
    );

    let overrides = match placement {
        Placement::Global => HashSet::new(),
        Placement::Local | Placement::Both => HashSet::from([String::from("diff.theme")]),
    };
    assert_eq!(session.config.local_overrides, overrides);
}

#[test]
fn integer_theme_in_the_global_file_falls_back_and_warns() {
    assert_shape(Placement::Global, INTEGER_TOML, INTEGER_WARNING);
}

#[test]
fn integer_theme_in_the_local_file_falls_back_and_warns() {
    assert_shape(Placement::Local, INTEGER_TOML, INTEGER_WARNING);
}

#[test]
fn integer_theme_in_both_files_falls_back_and_warns() {
    assert_shape(Placement::Both, INTEGER_TOML, INTEGER_WARNING);
}

#[test]
fn string_theme_in_the_global_file_hints_at_diff_theme() {
    assert_shape(Placement::Global, STRING_TOML, STRING_WARNING);
}

#[test]
fn string_theme_in_the_local_file_hints_at_diff_theme() {
    assert_shape(Placement::Local, STRING_TOML, STRING_WARNING);
}

#[test]
fn string_theme_in_both_files_hints_at_diff_theme() {
    assert_shape(Placement::Both, STRING_TOML, STRING_WARNING);
}

#[test]
fn string_colors_in_the_global_file_falls_back_and_warns() {
    assert_shape(Placement::Global, COLORS_TOML, COLORS_WARNING);
}

#[test]
fn string_colors_in_the_local_file_falls_back_and_warns() {
    assert_shape(Placement::Local, COLORS_TOML, COLORS_WARNING);
}

#[test]
fn string_colors_in_both_files_falls_back_and_warns() {
    assert_shape(Placement::Both, COLORS_TOML, COLORS_WARNING);
}

#[test]
fn string_theme_warning_escapes_control_characters() {
    let toml = "theme = \"\\u001b[2J\"\n\n[diff]\ntheme = \"Nord\"\n";
    let session = start(Some(toml), None);
    assert_startup(&session);
    assert!(session.config.theme.is_untouched());
    assert_eq!(session.config.diff.theme, "Nord");

    let warnings = warning_lines(&session.stderr);
    assert_eq!(
        warnings,
        vec![warning_line(
            &session.global_path,
            "theme: expected a table, got string \"\\u{1b}[2J\"; did you mean [diff] theme = \"...\"? (using default)"
        )],
        "stderr:\n{}",
        session.stderr
    );
    assert!(
        warnings.iter().all(|line| !line.contains('\u{1b}')),
        "warning wrote a raw escape: {warnings:?}"
    );
}

#[test]
fn bad_local_theme_keeps_the_global_theme_and_other_keys() {
    let global = "\
[diff]
theme = \"Nord\"

[theme]
preset = \"light\"

[theme.colors]
accent = \"green\"
";
    let local = "\
theme = \"Dracula\"

[diff]
tab_width = 2
";
    let session = start(Some(global), Some(local));
    assert_startup(&session);
    assert_eq!(session.config.theme.preset, ThemePreset::Light);
    assert_eq!(
        session.config.theme.colors.accent,
        Some(ratatui::style::Color::Green)
    );
    assert!(session.config.theme.colors.muted.is_none());
    assert_eq!(session.config.diff.theme, "Nord");
    assert_eq!(session.config.diff.tab_width, 2);
    assert_eq!(
        session.config.local_overrides,
        HashSet::from([String::from("diff.tab_width")])
    );
    assert_eq!(
        warning_lines(&session.stderr),
        vec![warning_line(
            &session.local_path,
            "theme: expected a table, got string \"Dracula\"; did you mean [diff] theme = \"...\"? (using global)"
        )],
        "stderr:\n{}",
        session.stderr
    );
}

#[test]
fn bad_global_theme_keeps_the_local_theme_and_other_keys() {
    let global = "\
theme = \"Dracula\"

[diff]
theme = \"Nord\"
";
    let local = "\
[diff]
tab_width = 8

[theme]
preset = \"ansi16\"

[theme.colors]
info = \"yellow\"
";
    let session = start(Some(global), Some(local));
    assert_startup(&session);
    assert_eq!(session.config.theme.preset, ThemePreset::Ansi16);
    assert_eq!(
        session.config.theme.colors.info,
        Some(ratatui::style::Color::Yellow)
    );
    assert_eq!(session.config.diff.theme, "Nord");
    assert_eq!(session.config.diff.tab_width, 8);
    assert_eq!(
        session.config.local_overrides,
        HashSet::from([
            String::from("diff.tab_width"),
            String::from("theme.preset"),
            String::from("theme.colors"),
        ])
    );
    assert_eq!(
        warning_lines(&session.stderr),
        vec![warning_line(
            &session.global_path,
            "theme: expected a table, got string \"Dracula\"; did you mean [diff] theme = \"...\"? (using default)"
        )],
        "stderr:\n{}",
        session.stderr
    );
}

#[test]
fn string_colors_keep_a_valid_preset_and_the_global_color() {
    let global = "\
[diff]
theme = \"Nord\"

[theme]
preset = \"dark\"

[theme.colors]
accent = \"green\"
";
    let local = "\
[diff]
tab_width = 2

[theme]
preset = \"light\"
colors = \"red\"
";
    let session = start(Some(global), Some(local));
    assert_startup(&session);
    assert_eq!(session.config.theme.preset, ThemePreset::Light);
    assert_eq!(
        session.config.theme.colors.accent,
        Some(ratatui::style::Color::Green)
    );
    assert_eq!(session.config.diff.theme, "Nord");
    assert_eq!(session.config.diff.tab_width, 2);
    assert_eq!(
        session.config.local_overrides,
        HashSet::from([String::from("diff.tab_width"), String::from("theme.preset"),])
    );
    assert_eq!(
        warning_lines(&session.stderr),
        vec![warning_line(
            &session.local_path,
            "theme.colors: expected a table, got string \"red\" (using global)"
        )],
        "stderr:\n{}",
        session.stderr
    );
}

#[test]
fn unknown_theme_keys_stay_silent_and_startup_continues() {
    let global = "\
[diff]
theme = \"Nord\"

[theme]
preset = \"light\"
extra = true

[theme.colors]
accent = \"blue\"
bogus = \"nope\"
";
    let session = start(Some(global), None);
    assert_startup(&session);
    assert_eq!(session.config.theme.preset, ThemePreset::Light);
    assert_eq!(
        session.config.theme.colors.accent,
        Some(ratatui::style::Color::Blue)
    );
    assert_eq!(session.config.diff.theme, "Nord");
    assert!(
        warning_lines(&session.stderr).is_empty(),
        "stderr:\n{}",
        session.stderr
    );
}
