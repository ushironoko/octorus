use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use xdg::BaseDirectories;

use crate::keybinding::{KeyBinding, KeySequence, NamedKey};

/// Security-sensitive AI config keys that require user confirmation
/// when overridden by local `.octorus/config.toml`.
/// Shared between TUI (`App::start_ai_rally`) and headless (`run_headless_with_context`).
pub const SENSITIVE_AI_KEYS: &[&str] = &[
    "ai.reviewer_additional_tools",
    "ai.reviewee_additional_tools",
    "ai.auto_post",
    "ai.reviewer",
    "ai.reviewee",
    "ai.prompt_dir",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub editor: Option<String>,
    pub diff: DiffConfig,
    pub keybindings: KeybindingsConfig,
    pub ai: AiConfig,
    pub git_log: GitLogConfig,
    #[serde(skip)]
    pub project_root: PathBuf,
    /// Path of the global config file if it was loaded successfully.
    #[serde(skip)]
    pub loaded_global_config: Option<PathBuf>,
    /// Path of the local config file if it was loaded successfully.
    #[serde(skip)]
    pub loaded_local_config: Option<PathBuf>,
    /// Set of dotted key paths overridden by the local config (e.g. "diff.theme", "editor").
    /// Computed once at load time to avoid per-frame disk I/O.
    #[serde(skip)]
    pub local_overrides: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub reviewer: String,
    pub reviewee: String,
    pub max_iterations: u32,
    pub timeout_secs: u64,
    /// Custom prompt directory (default: ~/.config/octorus/prompts/)
    pub prompt_dir: Option<String>,
    /// Additional tools for reviewer (Claude adapter only).
    /// Use Claude Code's --allowedTools format (e.g., "Skill", "Bash(git push:*)").
    #[serde(default)]
    pub reviewer_additional_tools: Vec<String>,
    /// Additional tools for reviewee (Claude adapter only).
    /// Use Claude Code's --allowedTools format (e.g., "Skill", "Bash(git push:*)").
    #[serde(default)]
    pub reviewee_additional_tools: Vec<String>,
    /// If true, AI Rally posts reviews/fix comments to PR without confirmation.
    /// Default is false (confirmation prompt before posting).
    #[serde(default)]
    pub auto_post: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitLogConfig {
    /// コミット diff キャッシュ（プリフェッチ含む）の最大エントリ数
    #[serde(default = "default_max_diff_cache")]
    pub max_diff_cache: usize,
}

fn default_max_diff_cache() -> usize {
    20
}

impl Default for GitLogConfig {
    fn default() -> Self {
        Self {
            max_diff_cache: default_max_diff_cache(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffConfig {
    pub theme: String,
    #[serde(deserialize_with = "deserialize_tab_width")]
    pub tab_width: u8,
    /// 追加/削除行に背景色を表示するかどうか
    #[serde(default = "default_true")]
    pub bg_color: bool,
}

fn default_true() -> bool {
    true
}

/// Deserialize tab_width with clamping: values below 1 are clamped to 1.
fn deserialize_tab_width<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u8::deserialize(deserializer)?;
    Ok(value.max(1))
}

/// Configurable keybindings
///
/// Supports three formats in TOML:
/// - Simple string: `move_down = "j"`
/// - Object with modifiers: `page_down = { key = "d", ctrl = true }`
/// - Array for sequences: `jump_to_first = ["g", "g"]`
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    // Navigation
    pub move_down: KeySequence,
    pub move_up: KeySequence,
    pub move_left: KeySequence,
    pub move_right: KeySequence,
    pub page_down: KeySequence,
    pub page_up: KeySequence,
    pub jump_to_first: KeySequence,
    pub jump_to_last: KeySequence,
    pub jump_back: KeySequence,
    pub next_comment: KeySequence,
    pub prev_comment: KeySequence,

    // Actions
    pub approve: KeySequence,
    pub request_changes: KeySequence,
    pub comment: KeySequence,
    pub suggestion: KeySequence,
    pub reply: KeySequence,
    pub refresh: KeySequence,
    pub submit: KeySequence,

    // Mode switching
    pub quit: KeySequence,
    pub help: KeySequence,
    pub comment_list: KeySequence,
    pub ai_rally: KeySequence,
    pub open_panel: KeySequence,

    // Diff operations
    pub go_to_definition: KeySequence,
    pub go_to_file: KeySequence,
    pub open_in_browser: KeySequence,

    // Local mode
    pub toggle_local_mode: KeySequence,
    pub toggle_auto_focus: KeySequence,

    // Markdown rich display
    pub toggle_markdown_rich: KeySequence,

    // List filter
    pub filter: KeySequence,

    // Multiline selection (fallback for Shift+Enter)
    pub multiline_select: KeySequence,

    // PR description
    pub pr_description: KeySequence,

    // CI Checks
    pub ci_checks: KeySequence,

    // Git log
    pub git_log: KeySequence,

    // Issue list
    pub issue_list: KeySequence,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            reviewer: "claude".to_owned(),
            reviewee: "claude".to_owned(),
            max_iterations: 10,
            timeout_secs: 600,
            prompt_dir: None,
            reviewer_additional_tools: Vec::new(),
            reviewee_additional_tools: Vec::new(),
            auto_post: false,
        }
    }
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            theme: "base16-ocean.dark".to_owned(),
            tab_width: 4,
            bg_color: true,
        }
    }
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            // Navigation
            move_down: KeySequence::single(KeyBinding::char('j')),
            move_up: KeySequence::single(KeyBinding::char('k')),
            move_left: KeySequence::single(KeyBinding::char('h')),
            move_right: KeySequence::single(KeyBinding::char('l')),
            page_down: KeySequence::single(KeyBinding::ctrl('d')),
            page_up: KeySequence::single(KeyBinding::ctrl('u')),
            jump_to_first: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('g')),
            jump_to_last: KeySequence::single(KeyBinding::char('G')),
            jump_back: KeySequence::single(KeyBinding::ctrl('o')),
            next_comment: KeySequence::single(KeyBinding::char('n')),
            prev_comment: KeySequence::single(KeyBinding::char('N')),

            // Actions
            approve: KeySequence::single(KeyBinding::char('a')),
            request_changes: KeySequence::single(KeyBinding::char('r')),
            comment: KeySequence::single(KeyBinding::char('c')),
            suggestion: KeySequence::single(KeyBinding::char('s')),
            reply: KeySequence::single(KeyBinding::char('r')),
            refresh: KeySequence::single(KeyBinding::char('R')),
            submit: KeySequence::single(KeyBinding::ctrl('s')),

            // Mode switching
            quit: KeySequence::single(KeyBinding::char('q')),
            help: KeySequence::single(KeyBinding::char('?')),
            comment_list: KeySequence::single(KeyBinding::char('C')),
            ai_rally: KeySequence::single(KeyBinding::char('A')),
            open_panel: KeySequence::single(KeyBinding::named(NamedKey::Enter)),

            // Diff operations
            go_to_definition: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('d')),
            go_to_file: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('f')),
            open_in_browser: KeySequence::single(KeyBinding::char('O')),

            // Local mode
            toggle_local_mode: KeySequence::single(KeyBinding::char('L')),
            toggle_auto_focus: KeySequence::single(KeyBinding::char('F')),

            // Markdown rich display
            toggle_markdown_rich: KeySequence::single(KeyBinding::char('M')),

            // List filter
            filter: KeySequence::double(KeyBinding::char(' '), KeyBinding::char('/')),

            // Multiline selection (fallback for Shift+Enter)
            multiline_select: KeySequence::single(KeyBinding::char('V')),

            // PR description
            pr_description: KeySequence::single(KeyBinding::char('d')),

            // CI Checks
            ci_checks: KeySequence::single(KeyBinding::char('S')),

            // Git log
            git_log: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('l')),

            // Issue list
            issue_list: KeySequence::single(KeyBinding::char('I')),
        }
    }
}

impl KeybindingsConfig {
    /// Validate keybindings for conflicts
    ///
    /// Detects:
    /// - Single keys that conflict with sequence prefixes
    /// - Duplicate keybindings for different actions
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        let mut single_keys: HashMap<KeyBinding, &str> = HashMap::new();
        let mut sequence_prefixes: HashMap<KeyBinding, &str> = HashMap::new();

        // Collect all keybindings
        let bindings: Vec<(&str, &KeySequence)> = vec![
            ("move_down", &self.move_down),
            ("move_up", &self.move_up),
            ("move_left", &self.move_left),
            ("move_right", &self.move_right),
            ("page_down", &self.page_down),
            ("page_up", &self.page_up),
            ("jump_to_first", &self.jump_to_first),
            ("jump_to_last", &self.jump_to_last),
            ("jump_back", &self.jump_back),
            ("next_comment", &self.next_comment),
            ("prev_comment", &self.prev_comment),
            ("approve", &self.approve),
            ("request_changes", &self.request_changes),
            ("comment", &self.comment),
            ("suggestion", &self.suggestion),
            ("reply", &self.reply),
            ("refresh", &self.refresh),
            ("submit", &self.submit),
            ("quit", &self.quit),
            ("help", &self.help),
            ("comment_list", &self.comment_list),
            ("ai_rally", &self.ai_rally),
            ("open_panel", &self.open_panel),
            ("go_to_definition", &self.go_to_definition),
            ("go_to_file", &self.go_to_file),
            ("open_in_browser", &self.open_in_browser),
            ("toggle_local_mode", &self.toggle_local_mode),
            ("toggle_auto_focus", &self.toggle_auto_focus),
            ("toggle_markdown_rich", &self.toggle_markdown_rich),
            ("filter", &self.filter),
            ("multiline_select", &self.multiline_select),
            ("pr_description", &self.pr_description),
            ("ci_checks", &self.ci_checks),
            ("git_log", &self.git_log),
            ("issue_list", &self.issue_list),
        ];

        for (name, seq) in &bindings {
            if seq.0.is_empty() {
                errors.push(format!("keybinding '{}' is empty", name));
                continue;
            }

            if seq.is_single() {
                let key = seq.0[0];
                if let Some(existing) = single_keys.get(&key) {
                    // Allow same key for different contexts (e.g., 'r' for reply and request_changes)
                    // This is intentional - context determines which action is triggered
                    if !is_context_compatible(name, existing) {
                        errors.push(format!(
                            "duplicate keybinding: '{}' and '{}' both use {}",
                            name,
                            existing,
                            key.display()
                        ));
                    }
                } else {
                    single_keys.insert(key, name);
                }
            } else {
                // For sequences, track the first key as a prefix
                if let Some(first) = seq.first() {
                    sequence_prefixes.insert(*first, name);
                }
            }
        }

        // Check for conflicts between single keys and sequence prefixes
        for (key, single_name) in &single_keys {
            if let Some(seq_name) = sequence_prefixes.get(key) {
                // Only warn if they're in the same context
                if !is_context_compatible(single_name, seq_name) {
                    errors.push(format!(
                        "keybinding conflict: '{}' ({}) conflicts with sequence prefix for '{}' ({})",
                        single_name,
                        key.display(),
                        seq_name,
                        key.display()
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Check if two keybindings are in compatible contexts
/// (i.e., they won't conflict because they're used in different views)
fn is_context_compatible(name1: &str, name2: &str) -> bool {
    // These keybindings are used in different contexts:
    // - 'r' is used for 'reply' in comment panel and 'request_changes' in file list
    //
    // NOTE: 'comment' and 'suggestion' are NOT compatible - both are active in diff view
    // and comment panel contexts, so they must have different bindings.
    let context_groups: &[&[&str]] = &[
        &["reply", "request_changes"],
        &["toggle_local_mode", "move_right"], // L vs l: different cases
        &["toggle_auto_focus", "go_to_file"], // F vs gf: different sequence lengths
    ];

    for group in context_groups {
        if group.contains(&name1) && group.contains(&name2) {
            return true;
        }
    }

    false
}

// Custom Serialize for KeybindingsConfig to maintain backwards compatibility
impl Serialize for KeybindingsConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        // For serialization, we output a simplified format
        let mut map = serializer.serialize_map(None)?;

        // Helper to serialize a KeySequence
        fn seq_to_value(seq: &KeySequence) -> toml::Value {
            if seq.is_single() {
                toml::Value::String(seq.display())
            } else {
                toml::Value::Array(
                    seq.0
                        .iter()
                        .map(|k| toml::Value::String(k.display()))
                        .collect(),
                )
            }
        }

        map.serialize_entry("move_down", &seq_to_value(&self.move_down))?;
        map.serialize_entry("move_up", &seq_to_value(&self.move_up))?;
        map.serialize_entry("move_left", &seq_to_value(&self.move_left))?;
        map.serialize_entry("move_right", &seq_to_value(&self.move_right))?;
        map.serialize_entry("page_down", &seq_to_value(&self.page_down))?;
        map.serialize_entry("page_up", &seq_to_value(&self.page_up))?;
        map.serialize_entry("jump_to_first", &seq_to_value(&self.jump_to_first))?;
        map.serialize_entry("jump_to_last", &seq_to_value(&self.jump_to_last))?;
        map.serialize_entry("jump_back", &seq_to_value(&self.jump_back))?;
        map.serialize_entry("next_comment", &seq_to_value(&self.next_comment))?;
        map.serialize_entry("prev_comment", &seq_to_value(&self.prev_comment))?;
        map.serialize_entry("approve", &seq_to_value(&self.approve))?;
        map.serialize_entry("request_changes", &seq_to_value(&self.request_changes))?;
        map.serialize_entry("comment", &seq_to_value(&self.comment))?;
        map.serialize_entry("suggestion", &seq_to_value(&self.suggestion))?;
        map.serialize_entry("reply", &seq_to_value(&self.reply))?;
        map.serialize_entry("refresh", &seq_to_value(&self.refresh))?;
        map.serialize_entry("submit", &seq_to_value(&self.submit))?;
        map.serialize_entry("quit", &seq_to_value(&self.quit))?;
        map.serialize_entry("help", &seq_to_value(&self.help))?;
        map.serialize_entry("comment_list", &seq_to_value(&self.comment_list))?;
        map.serialize_entry("ai_rally", &seq_to_value(&self.ai_rally))?;
        map.serialize_entry("open_panel", &seq_to_value(&self.open_panel))?;
        map.serialize_entry("go_to_definition", &seq_to_value(&self.go_to_definition))?;
        map.serialize_entry("go_to_file", &seq_to_value(&self.go_to_file))?;
        map.serialize_entry("open_in_browser", &seq_to_value(&self.open_in_browser))?;
        map.serialize_entry("toggle_local_mode", &seq_to_value(&self.toggle_local_mode))?;
        map.serialize_entry("toggle_auto_focus", &seq_to_value(&self.toggle_auto_focus))?;
        map.serialize_entry(
            "toggle_markdown_rich",
            &seq_to_value(&self.toggle_markdown_rich),
        )?;
        map.serialize_entry("filter", &seq_to_value(&self.filter))?;
        map.serialize_entry("multiline_select", &seq_to_value(&self.multiline_select))?;
        map.serialize_entry("pr_description", &seq_to_value(&self.pr_description))?;
        map.serialize_entry("ci_checks", &seq_to_value(&self.ci_checks))?;
        map.serialize_entry("git_log", &seq_to_value(&self.git_log))?;
        map.serialize_entry("issue_list", &seq_to_value(&self.issue_list))?;

        map.end()
    }
}

/// Deep merge two TOML values.
/// Tables are merged recursively; all other types are replaced by the override value.
fn deep_merge_toml(base: &mut toml::Value, override_val: toml::Value) {
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
    ///   `ai.reviewer`, `ai.reviewee` — tracked in `local_overrides` and
    ///   guarded by TUI confirmation / headless `--accept-local-overrides`
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
        for section in ["diff", "ai", "keybindings"] {
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
fn is_safe_local_prompt_dir(prompt_dir: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_json_snapshot;

    #[test]
    fn test_default_keybindings() {
        let config = KeybindingsConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_parse_simple_keybinding() {
        let toml_str = r#"
            [keybindings]
            move_down = "n"
            move_up = "e"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.move_down.display(), "n");
        assert_eq!(config.keybindings.move_up.display(), "e");
    }

    #[test]
    fn test_parse_modifier_keybinding() {
        let toml_str = r#"
            [keybindings]
            page_down = { key = "f", ctrl = true }
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.page_down.display(), "Ctrl-f");
    }

    #[test]
    fn test_parse_sequence_keybinding() {
        let toml_str = r#"
            [keybindings]
            jump_to_first = ["g", "g"]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.jump_to_first.display(), "gg");
    }

    #[test]
    fn test_backwards_compatible_defaults() {
        // Empty config should use all defaults
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.keybindings.approve.display(), "a");
        assert_eq!(config.keybindings.request_changes.display(), "r");
        assert_eq!(config.keybindings.filter.display(), "Space/");
    }

    #[test]
    fn test_backwards_compatible_without_filter_key() {
        // Config without filter key should deserialize with default
        let toml_str = r#"
            [keybindings]
            move_down = "j"
            move_up = "k"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.filter.display(), "Space/");
    }

    #[test]
    fn test_parse_ai_config_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_json_snapshot!(config.ai, @r#"
        {
          "reviewer": "claude",
          "reviewee": "claude",
          "max_iterations": 10,
          "timeout_secs": 600,
          "prompt_dir": null,
          "reviewer_additional_tools": [],
          "reviewee_additional_tools": [],
          "auto_post": false
        }
        "#);
    }

    #[test]
    fn test_parse_ai_config_custom() {
        let toml_str = r#"
            [ai]
            reviewer = "codex"
            reviewee = "claude"
            max_iterations = 5
            timeout_secs = 300
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_json_snapshot!(config.ai, @r#"
        {
          "reviewer": "codex",
          "reviewee": "claude",
          "max_iterations": 5,
          "timeout_secs": 300,
          "prompt_dir": null,
          "reviewer_additional_tools": [],
          "reviewee_additional_tools": [],
          "auto_post": false
        }
        "#);
    }

    #[test]
    fn test_parse_ai_config_with_additional_tools() {
        let toml_str = r#"
            [ai]
            reviewer_additional_tools = ["Skill", "WebSearch"]
            reviewee_additional_tools = ["Bash(git push:*)"]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_json_snapshot!(config.ai, @r#"
        {
          "reviewer": "claude",
          "reviewee": "claude",
          "max_iterations": 10,
          "timeout_secs": 600,
          "prompt_dir": null,
          "reviewer_additional_tools": [
            "Skill",
            "WebSearch"
          ],
          "reviewee_additional_tools": [
            "Bash(git push:*)"
          ],
          "auto_post": false
        }
        "#);
    }

    #[test]
    fn test_parse_ai_config_auto_post_true() {
        let toml_str = r#"
            [ai]
            auto_post = true
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.ai.auto_post);
    }

    #[test]
    fn test_parse_ai_config_auto_post_default() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.ai.auto_post);
    }

    #[test]
    fn test_editor_default_is_none() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.editor.is_none());
    }

    #[test]
    fn test_editor_explicit_value() {
        let config: Config = toml::from_str(r#"editor = "vim""#).unwrap();
        assert_eq!(config.editor.as_deref(), Some("vim"));
    }

    #[test]
    fn test_editor_with_args() {
        let config: Config = toml::from_str(r#"editor = "code --wait""#).unwrap();
        assert_eq!(config.editor.as_deref(), Some("code --wait"));
    }

    #[test]
    fn test_toggle_markdown_rich_default_key() {
        let config = KeybindingsConfig::default();
        assert_eq!(config.toggle_markdown_rich.display(), "M");
    }

    #[test]
    fn test_parse_toggle_markdown_rich_custom() {
        let toml_str = r#"
            [keybindings]
            toggle_markdown_rich = "m"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.toggle_markdown_rich.display(), "m");
    }

    #[test]
    fn test_diff_tab_width_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.diff.tab_width, 4);
    }

    #[test]
    fn test_diff_tab_width_custom() {
        let toml_str = r#"
            [diff]
            tab_width = 8
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.diff.tab_width, 8);
    }

    #[test]
    fn test_diff_tab_width_zero_clamped_to_one() {
        let toml_str = r#"
            [diff]
            tab_width = 0
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.diff.tab_width, 1,
            "tab_width = 0 should be clamped to 1"
        );
    }

    #[test]
    fn test_serialize_roundtrip_includes_toggle_markdown_rich() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        assert!(
            serialized.contains("toggle_markdown_rich"),
            "Serialized output should include toggle_markdown_rich"
        );
        // Roundtrip: deserialize back
        let deserialized: KeybindingsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.toggle_markdown_rich.display(),
            config.toggle_markdown_rich.display()
        );
    }

    #[test]
    fn test_serialize_roundtrip_includes_filter() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        assert!(
            serialized.contains("filter"),
            "Serialized output should include filter"
        );
        let deserialized: KeybindingsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.filter.display(), config.filter.display());
    }

    #[test]
    fn test_serialize_roundtrip_includes_multiline_select() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        assert!(
            serialized.contains("multiline_select"),
            "Serialized output should include multiline_select"
        );
        let deserialized: KeybindingsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.multiline_select.display(),
            config.multiline_select.display()
        );
    }

    #[test]
    fn test_multiline_select_default_key() {
        let config = KeybindingsConfig::default();
        assert_eq!(config.multiline_select.display(), "V");
    }

    #[test]
    fn test_parse_multiline_select_custom() {
        let toml_str = r#"
            [keybindings]
            multiline_select = "v"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.multiline_select.display(), "v");
    }

    #[test]
    fn test_backwards_compatible_without_multiline_select() {
        let toml_str = r#"
            [keybindings]
            move_down = "j"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.multiline_select.display(), "V");
    }

    // --- deep_merge_toml / load_from_paths tests ---

    #[test]
    fn test_deep_merge_empty_local_preserves_global() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[ai]
reviewer = "codex"
max_iterations = 5
"#,
        )
        .unwrap();
        fs::write(&local, "").unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.reviewer, "codex");
        assert_eq!(config.ai.max_iterations, 5);
    }

    #[test]
    fn test_deep_merge_local_scalar_overrides_global() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[ai]
reviewer = "codex"
max_iterations = 10
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[ai]
max_iterations = 3
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.reviewer, "codex"); // inherited from global
        assert_eq!(config.ai.max_iterations, 3); // overridden by local
    }

    #[test]
    fn test_deep_merge_nested_table_partial_override() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[ai]
reviewer = "codex"
reviewee = "claude"
max_iterations = 10
timeout_secs = 600
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[ai]
timeout_secs = 300
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.reviewer, "codex");
        assert_eq!(config.ai.reviewee, "claude");
        assert_eq!(config.ai.max_iterations, 10);
        assert_eq!(config.ai.timeout_secs, 300);
    }

    #[test]
    fn test_deep_merge_array_replaced_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[ai]
reviewer_additional_tools = ["Skill", "WebSearch"]
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[ai]
reviewer_additional_tools = ["WebFetch"]
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.reviewer_additional_tools, vec!["WebFetch"]);
    }

    #[test]
    fn test_deep_merge_local_adds_new_section() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(&global, "").unwrap();
        fs::write(
            &local,
            r#"
[ai]
reviewer = "codex"
max_iterations = 3
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.reviewer, "codex");
        assert_eq!(config.ai.max_iterations, 3);
    }

    #[test]
    fn test_deep_merge_keybindings_string_override() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[keybindings]
move_down = "j"
move_up = "k"
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[keybindings]
move_down = "n"
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.keybindings.move_down.display(), "n");
        assert_eq!(config.keybindings.move_up.display(), "k");
    }

    #[test]
    fn test_deep_merge_keybindings_string_to_object_override() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[keybindings]
page_down = "d"
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[keybindings]
page_down = { key = "f", ctrl = true }
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.keybindings.page_down.display(), "Ctrl-f");
    }

    #[test]
    fn test_deep_merge_keybindings_array_to_string_override() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[keybindings]
jump_to_first = ["g", "g"]
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[keybindings]
jump_to_first = "G"
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.keybindings.jump_to_first.display(), "G");
    }

    #[test]
    fn test_deep_merge_tab_width_zero_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[diff]
tab_width = 4
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[diff]
tab_width = 0
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.diff.tab_width, 1);
    }

    #[test]
    fn test_load_from_paths_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("nonexistent_global.toml");
        let local = dir.path().join("nonexistent_local.toml");

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        // Should use all defaults
        assert_eq!(config.ai.reviewer, "claude");
        assert_eq!(config.ai.max_iterations, 10);
        assert_eq!(config.diff.tab_width, 4);
    }

    #[test]
    fn test_load_from_paths_sets_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");
        fs::write(&global, "").unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.project_root, dir.path());
    }

    #[test]
    fn test_local_editor_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(&global, r#"editor = "vim""#).unwrap();
        fs::write(&local, r#"editor = "malicious; rm -rf /""#).unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        // Global editor should be preserved, local editor should be stripped
        assert_eq!(config.editor.as_deref(), Some("vim"));
        // local_overrides should NOT contain "editor"
        assert!(!config.local_overrides.contains("editor"));
    }

    #[test]
    fn test_local_editor_stripped_global_none() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(&global, "").unwrap();
        fs::write(&local, r#"editor = "malicious""#).unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        // No global editor, local editor stripped -> None
        assert!(config.editor.is_none());
    }

    #[test]
    fn test_local_overrides_tracks_ai_keys() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(&global, "").unwrap();
        fs::write(
            &local,
            r#"
[ai]
reviewer = "codex"
reviewee_additional_tools = ["Bash(git push:*)"]
auto_post = true
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert!(config.local_overrides.contains("ai.reviewer"));
        assert!(config
            .local_overrides
            .contains("ai.reviewee_additional_tools"));
        assert!(config.local_overrides.contains("ai.auto_post"));
    }

    #[test]
    fn test_is_safe_local_prompt_dir() {
        // Safe paths
        assert!(is_safe_local_prompt_dir(".octorus/prompts"));
        assert!(is_safe_local_prompt_dir("prompts"));
        assert!(is_safe_local_prompt_dir("my/prompts/dir"));

        // Unsafe: path traversal
        assert!(!is_safe_local_prompt_dir("../../evil"));
        assert!(!is_safe_local_prompt_dir("foo/../bar"));
        assert!(!is_safe_local_prompt_dir(".."));

        // Unsafe: absolute path
        assert!(!is_safe_local_prompt_dir("/absolute/path"));
        assert!(!is_safe_local_prompt_dir("/etc/passwd"));

        // Unsafe: Windows drive prefix paths
        // On Windows, these are parsed as Prefix components.
        // On Unix, they're treated as normal path segments, but we still
        // test the function doesn't crash. The Prefix rejection only
        // activates on Windows where Path::components() yields Prefix.
        #[cfg(windows)]
        {
            assert!(!is_safe_local_prompt_dir("C:evil\\prompts"));
            assert!(!is_safe_local_prompt_dir("C:\\absolute\\path"));
            assert!(!is_safe_local_prompt_dir("\\\\server\\share"));
        }
    }

    #[test]
    fn test_local_prompt_dir_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(&global, "").unwrap();
        fs::write(
            &local,
            r#"
[ai]
prompt_dir = "../../evil"
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        // Traversal path should be rejected, prompt_dir reset to None
        assert!(config.ai.prompt_dir.is_none());
    }

    #[test]
    fn test_local_prompt_dir_absolute_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(&global, "").unwrap();
        fs::write(
            &local,
            r#"
[ai]
prompt_dir = "/absolute/path"
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert!(config.ai.prompt_dir.is_none());
    }

    #[test]
    fn test_global_prompt_dir_absolute_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[ai]
prompt_dir = "/home/user/prompts"
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        // Global config absolute paths are allowed
        assert_eq!(config.ai.prompt_dir.as_deref(), Some("/home/user/prompts"));
    }

    #[test]
    fn test_local_prompt_dir_safe_path_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(&global, "").unwrap();
        fs::write(
            &local,
            r#"
[ai]
prompt_dir = ".octorus/prompts"
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.prompt_dir.as_deref(), Some(".octorus/prompts"));
    }

    #[test]
    fn test_max_iterations_clamped_to_hard_limit() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[ai]
max_iterations = 999999
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.max_iterations, 100);
    }

    #[test]
    fn test_timeout_secs_clamped_to_hard_limit() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[ai]
timeout_secs = 999999
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.timeout_secs, 7200);
    }

    #[test]
    fn test_normal_iterations_and_timeout_not_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[ai]
max_iterations = 50
timeout_secs = 3600
"#,
        )
        .unwrap();

        let config = Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.ai.max_iterations, 50);
        assert_eq!(config.ai.timeout_secs, 3600);
    }

    #[test]
    fn test_pr_description_keybinding_default() {
        let config = KeybindingsConfig::default();
        assert_eq!(config.pr_description.display(), "d");
    }

    #[test]
    fn test_pr_description_keybinding_custom() {
        let toml_str = r#"
            [keybindings]
            pr_description = "D"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.pr_description.display(), "D");
    }

    #[test]
    fn test_pr_description_keybinding_serialize_roundtrip() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("pr_description"));
        let parsed: KeybindingsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.pr_description.display(), "d");
    }

    #[test]
    fn test_git_log_keybinding_serialize_roundtrip() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("git_log"));
        let parsed: KeybindingsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.git_log.display(), "gl");
    }

    #[test]
    fn test_issue_list_keybinding_serialize_roundtrip() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("issue_list"));
        let parsed: KeybindingsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.issue_list.display(), "I");
    }
}
