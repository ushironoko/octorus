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

#[test]
fn invalid_repo_exits_with_error() {
    cargo_bin_cmd!("or")
        .args(["--repo", "invalid/nonexistent-repo-12345", "--pr", "1"])
        .assert()
        .failure();
}
