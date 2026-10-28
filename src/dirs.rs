use std::path::{Path, PathBuf};

use anyhow::Result;
use xdg::BaseDirectories;

/// Config and cache directories for octorus.
#[derive(Debug)]
pub struct ProjectDirs {
    pub config_home: PathBuf,
    pub cache_home: PathBuf,
}

/// Load octorus XDG directories. Errors when the home directory is unknown.
pub fn project_dirs() -> Result<ProjectDirs> {
    project_dirs_from_home(std::env::home_dir().as_deref())
}

/// `home` is `std::env::home_dir()`.
///
/// xdg 2 failed inside `with_prefix` when home was missing, before reading
/// `XDG_*_HOME`. xdg 3 reports that case as `None`. Paths still come from xdg.
fn project_dirs_from_home(home: Option<&Path>) -> Result<ProjectDirs> {
    if home.is_none() {
        return Err(missing_home());
    }

    let base = BaseDirectories::with_prefix("octorus");
    Ok(ProjectDirs {
        config_home: base.get_config_home().ok_or_else(missing_home)?,
        cache_home: base.get_cache_home().ok_or_else(missing_home)?,
    })
}

fn missing_home() -> anyhow::Error {
    anyhow::anyhow!("$HOME must be set").context("Failed to get config directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_home_keeps_the_xdg2_error() {
        let error = project_dirs_from_home(None).expect_err("missing home is an error");
        let rendered = format!("{error:?}");
        let rendered = rendered
            .split("\n\nStack backtrace:")
            .next()
            .expect("error text");
        assert_eq!(
            rendered,
            "Failed to get config directory\n\nCaused by:\n    $HOME must be set"
        );
    }
}
