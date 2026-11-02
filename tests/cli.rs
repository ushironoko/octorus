use assert_cmd::cargo::cargo_bin_cmd;
use octorus::app::{App, AppState, RepositoryAvailability};
use octorus::config::Config;
use predicates::prelude::*;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::path::Path;
use std::process::Output;

const HELP_BANNER_LINE: &str =
    "  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗";

/// The alternate-screen control sequence, without its final `h`/`l`.
///
/// `octorus::ui::setup_terminal` writes the enter form on the way into the TUI,
/// and the binary's terminal restore writes the leave form when start-up fails
/// for want of a tty. Nothing before the TUI writes either, so this is the one
/// positive signal the binary offers that a start-up flag got past repo
/// detection — `repo_detection_exit_writes_nothing_to_stdout` pins the contrast.
const ALTERNATE_SCREEN_CSI: &str = "\u{1b}[?1049";

/// Run `or <args>` in `cwd` with HOME and the XDG directories pointed inside it,
/// so the run cannot see the developer's config, cache or `gh` credentials.
///
/// The TUI never exits on its own, so the run is killed on a timeout; callers
/// read the output it produced before then rather than an exit status.
fn run_isolated(args: &[&str], cwd: &Path) -> Output {
    let home = cwd.join("home");
    let config_home = cwd.join("config");
    let cache_home = cwd.join("cache");
    for path in [&home, &config_home, &cache_home] {
        std::fs::create_dir_all(path).expect("create isolated environment directory");
    }

    cargo_bin_cmd!("or")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_CACHE_HOME", cache_home)
        .timeout(std::time::Duration::from_secs(3))
        .output()
        .expect("failed to execute")
}

/// Create a git repository with one commit and no remote.
///
/// A missing Git binary may be skipped locally, but CI is expected to provide
/// Git. Any command that starts and fails is a broken fixture, never a skip.
fn init_git_repo(test_name: &str, path: &Path) -> bool {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(path)
            .output();

        match output {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                panic!(
                    "{test_name}: git {} failed with {}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && std::env::var_os("CI").is_none() =>
            {
                use std::io::Write;

                let notice = format!("SKIPPED {test_name}: git binary is unavailable ({error})\n");
                std::io::stderr()
                    .lock()
                    .write_all(notice.as_bytes())
                    .expect("write visible test skip");
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                panic!("{test_name}: git is required when CI is set ({error})");
            }
            Err(error) => {
                panic!("{test_name}: failed to run git {}: {error}", args.join(" "));
            }
        }
    };

    if !git(&["init", "."]) {
        return false;
    }
    std::fs::write(path.join("seed.txt"), "seed\n").expect("write seed file");
    git(&["add", "seed.txt"]) && git(&["commit", "-m", "seed commit"])
}

/// Assert `or <flag>` in `cwd` reaches TUI start-up instead of exiting at repo
/// detection.
///
/// This is as far as the binary can be driven from a test, and the name says
/// "reaches TUI start-up" rather than "opens the screen" for that reason.
/// Measured, not assumed: with stdout piped the binary writes the eight-byte
/// alternate-screen escape and then exits with "Device not configured" before
/// drawing a single frame, so stdout holds that escape and nothing else. No
/// assertion here can name the screen that opened, and Cockpit reaches this
/// same point identically.
///
/// That gap is closed on the other side, in `src/main.rs`:
/// `test_every_entry_point_flag_excludes_itself_from_is_no_args` pins the
/// dispatch decision itself, so a flag can no longer fall through to Cockpit
/// unnoticed. The screens themselves are covered where they can be rendered —
/// `src/ui/browse.rs` for the Repository Browser, `src/ui/git_ops.rs` and
/// `git_ops_screen_works_in_a_repo_less_session` below for Git Ops.
fn assert_flag_reaches_tui_startup(flag: &str, cwd: &Path) {
    let output = run_isolated(&[flag], cwd);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stdout.contains(ALTERNATE_SCREEN_CSI),
        "`or {flag}` never reached TUI start-up; stdout:\n{stdout:?}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("Use --repo to specify."),
        "`or {flag}` took the hard repo-detection error path; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("gh CLI error"),
        "`or {flag}` took the gh CLI error path; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "`or {flag}` panicked; stderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("Usage"),
        "`or {flag}` printed clap help instead of starting the TUI; stderr:\n{stderr}"
    );
}

#[test]
fn help_exits_successfully() {
    cargo_bin_cmd!("or")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(HELP_BANNER_LINE));
}

#[test]
fn version_exits_successfully() {
    cargo_bin_cmd!("or")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("or "));
}

#[test]
fn init_help_exits_successfully() {
    cargo_bin_cmd!("or")
        .args(["init", "--help"])
        .assert()
        .success();
}

// No-args launches the Cockpit TUI (alternate screen), so we can't test
// the full flow via assert_cmd. But we CAN verify that the binary does NOT
// fall back to printing help — it should attempt to enter TUI mode and
// eventually fail or hang (timeout), never printing "Usage:" to stdout.
// The ASCII-art banner check is omitted because crossterm may render it
// via escape sequences during TUI init, causing false positives.
#[test]
fn no_args_does_not_print_help() {
    let output = cargo_bin_cmd!("or")
        .timeout(std::time::Duration::from_secs(3))
        .output()
        .expect("failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(ALTERNATE_SCREEN_CSI),
        "no-args never reached TUI start-up; stdout:\n{stdout:?}"
    );
    assert!(
        !stdout.contains("Usage"),
        "no-args should enter Cockpit, not print help"
    );
}

#[test]
fn browse_in_non_git_directory_reaches_tui_startup() {
    let tmp = tempfile::tempdir().expect("create tempdir");

    assert_flag_reaches_tui_startup("--browse", tmp.path());
}

#[test]
fn browse_in_git_repo_without_github_remote_reaches_tui_startup() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    if !init_git_repo(
        "browse_in_git_repo_without_github_remote_reaches_tui_startup",
        tmp.path(),
    ) {
        return;
    }

    assert_flag_reaches_tui_startup("--browse", tmp.path());
}

#[test]
fn git_ops_in_non_git_directory_reaches_tui_startup() {
    let tmp = tempfile::tempdir().expect("create tempdir");

    assert_flag_reaches_tui_startup("--git-ops", tmp.path());
}

#[test]
fn git_ops_in_git_repo_without_github_remote_reaches_tui_startup() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    if !init_git_repo(
        "git_ops_in_git_repo_without_github_remote_reaches_tui_startup",
        tmp.path(),
    ) {
        return;
    }

    assert_flag_reaches_tui_startup("--git-ops", tmp.path());
}

/// The contrast that gives [`ALTERNATE_SCREEN_CSI`] its meaning.
///
/// `--pr` does not tolerate a missing repository, so in the same isolated,
/// non-git directory it exits at repo detection and writes nothing to stdout.
/// Without this, an `or` that emitted terminal control sequences before repo
/// detection would make the start-up assertions pass for the wrong reason.
#[test]
fn repo_detection_exit_writes_nothing_to_stdout() {
    let tmp = tempfile::tempdir().expect("create tempdir");

    let output = run_isolated(&["--pr", "1"], tmp.path());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.is_empty(),
        "a flag that exits at repo detection wrote to stdout: {stdout:?}"
    );
    assert!(
        !output.status.success(),
        "--pr without a detectable repository should fail"
    );
}

#[test]
fn invalid_repo_exits_with_error() {
    cargo_bin_cmd!("or")
        .args(["--repo", "invalid/nonexistent-repo-12345", "--pr", "1"])
        .assert()
        .failure();
}

#[test]
fn pr_flag_only_enters_pr_list() {
    cargo_bin_cmd!("or")
        .args(["--repo", "invalid/nonexistent-repo-12345", "--pr"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Usage").not());
}

#[test]
fn pr_short_flag_only_enters_pr_list() {
    cargo_bin_cmd!("or")
        .args(["--repo", "invalid/nonexistent-repo-12345", "-p"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Usage").not());
}

#[test]
fn issue_flag_only_enters_issue_list() {
    cargo_bin_cmd!("or")
        .args(["--repo", "invalid/nonexistent-repo-12345", "--issue"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Usage").not());
}

#[test]
fn issue_short_flag_only_enters_issue_list() {
    cargo_bin_cmd!("or")
        .args(["--repo", "invalid/nonexistent-repo-12345", "-i"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Usage").not());
}

#[test]
fn update_local_comment_missing_id_exits_non_zero() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    cargo_bin_cmd!("or")
        .args([
            "update-local-comment",
            "--repo",
            "owner/repo",
            "--working-dir",
            tmp.path().to_str().unwrap(),
            "--resolve",
            "999",
        ])
        .env("XDG_CACHE_HOME", tmp.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("Missing IDs: 999"))
        .stderr(predicate::str::contains("unknown local comment ID"));
}

#[test]
fn local_comments_purge_removes_file_and_reports_count() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let workdir = tmp.path().join("worktree");
    std::fs::create_dir_all(&workdir).unwrap();

    // Seed a comment so purge has something to delete.
    let comments_dir = tmp.path().join("octorus").join("local-comments");
    std::fs::create_dir_all(&comments_dir).unwrap();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&workdir.to_string_lossy().as_ref(), &mut hasher);
    let workdir_hash = std::hash::Hasher::finish(&hasher);
    let path = comments_dir.join(format!("owner_repo-{:016x}.json", workdir_hash));
    std::fs::write(
        &path,
        r#"{"version":1,"comments":[{"id":1,"path":"a.rs","line":1,"body":"x","user":{"login":"u"},"created_at":"2026-04-27T00:00:00Z"}]}"#,
    )
    .unwrap();

    cargo_bin_cmd!("or")
        .args([
            "local-comments",
            "--repo",
            "owner/repo",
            "--working-dir",
            workdir.to_str().unwrap(),
            "--purge",
        ])
        .env("XDG_CACHE_HOME", tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Purged 1 local comment"));

    assert!(!path.exists());
}

#[test]
fn local_comments_purge_with_no_file_reports_zero() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    cargo_bin_cmd!("or")
        .args([
            "local-comments",
            "--repo",
            "owner/repo",
            "--working-dir",
            tmp.path().to_str().unwrap(),
            "--purge",
        ])
        .env("XDG_CACHE_HOME", tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Purged 0 local comments"));
}

/// Render the whole app into a fixed-size test terminal.
fn render_app(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| octorus::ui::render(frame, app))
        .expect("render");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `--git-ops` without a detectable repository now starts a session with
/// `repo = "local"` and `repo_available = false` instead of exiting at repo
/// detection. Tolerating the missing repo is only a fix if the screen then
/// works, so this builds the app the way `run_with_pr_list` builds it for that
/// case and drives the Git Ops screen itself.
///
/// Delivery of the background results is polled by `App::run`'s loop, which
/// needs a real terminal, so the commit source is exercised directly: Git Ops
/// reads it through exactly this call whenever `pr_number` is `None`, which is
/// what `App::new_pr_list` leaves it as.
#[tokio::test]
async fn git_ops_screen_works_in_a_repo_less_session() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    if !init_git_repo("git_ops_screen_works_in_a_repo_less_session", tmp.path()) {
        return;
    }
    let working_dir = tmp.path().to_string_lossy().to_string();

    let mut app = App::new_pr_list(
        "local",
        Config::default(),
        RepositoryAvailability::Unavailable,
    );
    app.set_working_dir(Some(working_dir.clone()));
    app.open_git_ops();

    assert_eq!(app.repo, "local");
    assert_eq!(app.state, AppState::GitOpsSplitTree);
    let ops = app.git_ops_state.as_ref().expect("git ops state");
    assert_eq!(ops.return_state, AppState::PullRequestList);
    assert!(ops.commit_log.loading, "the commit fetch was never started");
    assert!(ops.commit_log.error.is_none());

    // The screen draws its own panes rather than a data-state error: with no
    // repository there is no PR data to wait on, and `GitOpsSplitTree` is
    // data-state independent, so nothing about `repo = "local"` reaches it.
    let rendered = render_app(&mut app, 100, 24);
    for pane in ["Git Status", "FILES (0)", "Commits", "Diff"] {
        assert!(rendered.contains(pane), "no {pane} pane:\n{rendered}");
    }
    assert!(rendered.contains("No changes"), "{rendered}");
    assert!(!rendered.contains("Error"), "{rendered}");

    // The commit log Git Ops fills in this configuration comes from the local
    // repository, and works with no GitHub repository at all.
    let page = octorus::github::fetch_local_commits(Some(&working_dir), 0, 30)
        .await
        .expect("local commit log");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].message, "seed commit");
    assert!(!page.has_more);
}
