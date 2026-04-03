use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

#[test]
fn help_exits_successfully() {
    cargo_bin_cmd!("or")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("octorus"));
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

// no-args now launches the Cockpit TUI (alternate screen),
// which cannot be tested via assert_cmd.
// Cockpit startup routing is covered by unit tests in src/app/cockpit.rs.

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
