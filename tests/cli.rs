use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

const HELP_BANNER_LINE: &str =
    "  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗";

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
        !stdout.contains("Usage"),
        "no-args should enter Cockpit, not print help"
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
