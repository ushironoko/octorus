use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use xdg::BaseDirectories;

use super::Config;

/// Deep merge two TOML values.
/// Tables are merged recursively; all other types are replaced by the override value.
pub(super) fn deep_merge_toml(base: &mut toml::Value, override_val: toml::Value) {
    match (base, override_val) {
        (toml::Value::Table(base_table), toml::Value::Table(override_table)) => {
            for (key, override_value) in override_table {
                match base_table.get_mut(&key) {
                    Some(base_value) => deep_merge_toml(base_value, override_value),
                    None => {
                        base_table.insert(key, override_value);
                    }
                }
            }
        }
        (base, override_val) => {
            *base = override_val;
        }
    }
}

/// Find the project root directory.
/// Uses `git rev-parse --show-toplevel` if in a git repository,
/// otherwise falls back to `current_dir()`.
pub fn find_project_root() -> PathBuf {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|s| PathBuf::from(s.trim()))
            } else {
                None
            }
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Find the project root directory starting from a specific directory.
/// Uses `git rev-parse --show-toplevel` with `current_dir` set to `dir`.
pub fn find_project_root_in(dir: &Path) -> PathBuf {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|s| PathBuf::from(s.trim()))
            } else {
                None
            }
        })
        .unwrap_or_else(|| dir.to_path_buf())
}

impl Config {
    pub fn load() -> Result<Self> {
        let global_path = Self::config_path();
        let project_root = find_project_root();
        let local_path = project_root.join(".octorus/config.toml");
        Self::load_from_paths(&global_path, &local_path, project_root)
    }

    /// Load config with project root resolved from a specific directory.
    /// Use this when `--working-dir` is specified so that `.octorus/` is
    /// resolved relative to the working directory's git root, not the
    /// process cwd.
    pub fn load_for_dir(dir: &Path) -> Result<Self> {
        let global_path = Self::config_path();
        let project_root = find_project_root_in(dir);
        let local_path = project_root.join(".octorus/config.toml");
        Self::load_from_paths(&global_path, &local_path, project_root)
    }

    /// Load config by merging global and local TOML files.
    /// Local values override global values at the TOML table level (deep merge).
    ///
    /// SECURITY NOTE: Local `.octorus/config.toml` has a 3-tier trust model:
    /// - **Stripped**: `editor` is removed before merge (command injection risk)
    /// - **Confirmation required**: `ai.*_additional_tools`, `ai.auto_post`,
    ///   `ai.reviewer`, `ai.reviewee`, `ai.review_only` — tracked in
    ///   `local_overrides` and guarded by TUI confirmation / headless
    ///   `--accept-local-overrides`
    /// - **Validated**: `ai.prompt_dir` — path traversal checks applied
    ///
    /// All other keys (theme, keybindings, etc.) are freely overridable.
    pub fn load_from_paths(
        global_path: &Path,
        local_path: &Path,
        project_root: PathBuf,
    ) -> Result<Self> {
        let mut base_value: toml::Value = if global_path.exists() {
            let content =
                fs::read_to_string(global_path).context("Failed to read global config file")?;
            toml::from_str(&content).context("Failed to parse global config file")?
        } else {
            toml::Value::Table(toml::map::Map::new())
        };

        let mut stripped_local_value: Option<toml::Value> = None;
        if local_path.exists() {
            let local_content = fs::read_to_string(local_path)
                .context("Failed to read local config file (.octorus/config.toml)")?;
            let mut local_value: toml::Value = toml::from_str(&local_content)
                .context("Failed to parse local config file (.octorus/config.toml)")?;

            // Strip `editor` key from local config to prevent command injection.
            // Editor preference is a user-level setting, not a per-repository concern.
            if let toml::Value::Table(ref mut t) = local_value {
                if t.remove("editor").is_some() {
                    tracing::warn!(
                        "editor key in local .octorus/config.toml is ignored for security"
                    );
                }
            }

            stripped_local_value = Some(local_value.clone());
            deep_merge_toml(&mut base_value, local_value);
        }

        let mut config: Config = base_value
            .try_into()
            .context("Failed to deserialize merged config")?;
        config.project_root = project_root;
        config.loaded_global_config = if global_path.exists() {
            Some(global_path.to_path_buf())
        } else {
            None
        };
        config.loaded_local_config = if local_path.exists() {
            Some(local_path.to_path_buf())
        } else {
            None
        };
        config.local_overrides = match stripped_local_value {
            Some(ref v) => Self::collect_override_keys_from_value(v),
            None => HashSet::new(),
        };

        // Validate local prompt_dir: reject absolute paths and path traversal
        if config.local_overrides.contains("ai.prompt_dir") {
            if let Some(ref dir) = config.ai.prompt_dir {
                if !is_safe_local_prompt_dir(dir) {
                    tracing::warn!(
                        "ai.prompt_dir '{}' in local config rejected (path traversal or absolute)",
                        dir
                    );
                    config.ai.prompt_dir = None;
                }
            }
        }

        // Clamp AI config values to hard limits to prevent resource exhaustion
        const MAX_ITERATIONS_LIMIT: u32 = 100;
        const MAX_TIMEOUT_SECS_LIMIT: u64 = 7200; // 2 hours
        config.ai.max_iterations = config.ai.max_iterations.min(MAX_ITERATIONS_LIMIT);
        config.ai.timeout_secs = config.ai.timeout_secs.min(MAX_TIMEOUT_SECS_LIMIT);

        // Validate keybindings and warn on conflicts
        if let Err(errors) = config.keybindings.validate() {
            for error in errors {
                eprintln!("Warning: {}", error);
            }
        }

        Ok(config)
    }

    /// Collect dotted key paths from a (stripped) TOML value.
    /// Used to determine which keys were overridden by the local config.
    fn collect_override_keys_from_value(value: &toml::Value) -> HashSet<String> {
        let mut overrides = HashSet::new();
        let toml::Value::Table(table) = value else {
            return overrides;
        };
        if table.contains_key("editor") {
            overrides.insert("editor".to_string());
        }
        for section in ["diff", "ai", "keybindings", "layout"] {
            if let Some(toml::Value::Table(sub)) = table.get(section) {
                for key in sub.keys() {
                    overrides.insert(format!("{}.{}", section, key));
                }
            }
        }
        overrides
    }

    pub fn config_path() -> PathBuf {
        BaseDirectories::with_prefix("octorus")
            .map(|dirs| dirs.get_config_home().join("config.toml"))
            .unwrap_or_else(|_| PathBuf::from("config.toml"))
    }
}

/// Validate that a prompt_dir from local config is safe.
/// Rejects absolute paths, Windows drive prefixes (e.g. `C:evil\prompts`),
/// and paths containing `..` (parent directory traversal).
/// Uses `Path::components()` for platform-independent validation.
pub(super) fn is_safe_local_prompt_dir(prompt_dir: &str) -> bool {
    let path = Path::new(prompt_dir);
    if path.is_absolute() {
        return false;
    }
    path.components().all(|c| {
        !matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    })
}
