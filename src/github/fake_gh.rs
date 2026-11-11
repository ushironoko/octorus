//! The `gh` test double in `tests/fixtures/fake-gh`. A test that reaches GitHub
//! through `gh` installs it first on PATH, so no call leaves the machine. Each
//! call is logged, and the reply comes from the `FAKE_GH_*` variables.
//!
//! PATH and those variables are process-wide, so a test that installs the double
//! is `#[serial]`, and keeps the guard alive until every `gh` it spawned has
//! finished.

use std::ffi::OsString;
use std::path::PathBuf;

const FAKE_GH_VARS: [&str; 7] = [
    "FAKE_GH_LOG",
    "FAKE_GH_REVIEWS",
    "FAKE_GH_CREATE",
    "FAKE_GH_GRAPHQL",
    "FAKE_GH_PUBLISH",
    "FAKE_GH_PR_REVIEW",
    "FAKE_GH_DRAFTS",
];

/// Puts the test double first on PATH and sets its replies. Dropping it restores
/// PATH and clears the variables.
pub(crate) struct FakeGh {
    _dir: tempfile::TempDir,
    log: PathBuf,
    old_path: Option<OsString>,
}

impl FakeGh {
    pub(crate) fn install(reviews: &str, create: &str, graphql: &str, publish: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let old_path = std::env::var_os("PATH");
        let mut path = OsString::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake-gh"
        ));
        path.push(":");
        path.push(old_path.clone().unwrap_or_default());
        std::env::set_var("PATH", path);
        std::env::set_var("FAKE_GH_LOG", &log);
        std::env::set_var("FAKE_GH_REVIEWS", reviews);
        std::env::set_var("FAKE_GH_CREATE", create);
        std::env::set_var("FAKE_GH_GRAPHQL", graphql);
        std::env::set_var("FAKE_GH_PUBLISH", publish);
        std::env::set_var("FAKE_GH_PR_REVIEW", "");
        std::env::set_var("FAKE_GH_DRAFTS", "[[]]");
        Self {
            _dir: dir,
            log,
            old_path,
        }
    }

    /// Sets the pending review's comments, as a paginated `--slurp` reply.
    pub(crate) fn set_drafts(&self, json: &str) {
        std::env::set_var("FAKE_GH_DRAFTS", json);
    }

    /// Every call made so far, as logged by the test double.
    pub(crate) fn calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        match &self.old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        for name in FAKE_GH_VARS {
            std::env::remove_var(name);
        }
    }
}
