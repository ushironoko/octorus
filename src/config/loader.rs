use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::schema::ThemePreset;
use super::Config;

const THEME_COLOR_KEYS: [&str; 6] = ["accent", "muted", "warning", "danger", "success", "info"];

/// Remove theme values the config parser would reject.
///
/// `global_has` is true when the already-sanitized global table still has that
/// key (`preset`, or `colors.accent` and the other color keys). The global
/// file passes `|_| false` because a bad global value falls back to the preset.
pub(super) fn sanitize_theme_table(
    path: &Path,
    value: &mut toml::Value,
    global_has: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(theme) = value
        .as_table_mut()
        .and_then(|root| root.get_mut("theme"))
        .and_then(|theme| theme.as_table_mut())
    else {
        return warnings;
    };

    let bad_preset = theme.get("preset").and_then(|preset| {
        if known_preset(preset) {
            None
        } else {
            Some(theme_value_text(preset))
        }
    });
    if let Some(shown) = bad_preset {
        let fallback = fallback_label(global_has("preset"));
        warnings.push(format!(
            "{}: theme.preset: unknown preset \"{shown}\" ({fallback})",
            path.display()
        ));
        theme.remove("preset");
    }

    let Some(colors) = theme
        .get_mut("colors")
        .and_then(|colors| colors.as_table_mut())
    else {
        return warnings;
    };
    let bad_colors: Vec<(String, String)> = THEME_COLOR_KEYS
        .iter()
        .filter_map(|key| {
            let color = colors.get(*key)?;
            if known_color(color) {
                None
            } else {
                Some(((*key).to_string(), theme_value_text(color)))
            }
        })
        .collect();
    for (key, shown) in bad_colors {
        let fallback = fallback_label(global_has(&format!("colors.{key}")));
        warnings.push(format!(
            "{}: theme.colors.{key}: invalid color \"{shown}\" ({fallback})",
            path.display()
        ));
        colors.remove(&key);
    }
    warnings
}

pub(super) fn theme_key_held(global: &toml::Value, key: &str) -> bool {
    let Some(theme) = global.get("theme").and_then(toml::Value::as_table) else {
        return false;
    };
    if let Some(color_key) = key.strip_prefix("colors.") {
        return theme
            .get("colors")
            .and_then(toml::Value::as_table)
            .is_some_and(|colors| colors.contains_key(color_key));
    }
    key == "preset" && theme.contains_key("preset")
}

fn known_preset(value: &toml::Value) -> bool {
    value.clone().try_into::<ThemePreset>().is_ok()
}

fn known_color(value: &toml::Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| text.parse::<ratatui::style::Color>().is_ok())
}

fn theme_value_text(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        toml::Value::Integer(number) => number.to_string(),
        toml::Value::Float(number) => number.to_string(),
        toml::Value::Boolean(flag) => flag.to_string(),
        toml::Value::Datetime(datetime) => datetime.to_string(),
        toml::Value::Array(_) => "array".to_string(),
        toml::Value::Table(_) => "table".to_string(),
    }
}

fn fallback_label(global_has_key: bool) -> &'static str {
    if global_has_key {
        "using global"
    } else {
        "using default"
    }
}

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
        let mut theme_warnings = sanitize_theme_table(global_path, &mut base_value, |_| false);

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

            theme_warnings.extend(sanitize_theme_table(local_path, &mut local_value, |key| {
                theme_key_held(&base_value, key)
            }));
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
        for warning in theme_warnings {
            #[allow(clippy::print_stderr)]
            {
                eprintln!("Warning: {}", warning);
            }
        }
        if let Err(errors) = config.keybindings.validate() {
            for error in errors {
                #[allow(clippy::print_stderr)]
                {
                    eprintln!("Warning: {}", error);
                }
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
        for section in ["diff", "ai", "keybindings", "layout", "theme"] {
            if let Some(toml::Value::Table(sub)) = table.get(section) {
                for key in sub.keys() {
                    overrides.insert(format!("{}.{}", section, key));
                }
            }
        }
        overrides
    }

    pub fn config_path() -> PathBuf {
        crate::dirs::project_dirs()
            .map(|dirs| dirs.config_home.join("config.toml"))
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
