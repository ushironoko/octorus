mod keybindings;
mod loader;
mod schema;

pub use keybindings::KeybindingsConfig;
pub use loader::{find_project_root, find_project_root_in};
pub use schema::{AiConfig, DiffConfig, GitOpsConfig, LayoutConfig, ShellConfig};

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

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
    #[serde(default)]
    pub layout: LayoutConfig,
    #[serde(alias = "git_log")]
    pub git_ops: GitOpsConfig,
    pub shell: ShellConfig,
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

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_json_snapshot;
    use std::fs;

    // Re-import loader internals for testing
    use super::loader::is_safe_local_prompt_dir;

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
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.keybindings.approve.display(), "a");
        assert_eq!(config.keybindings.request_changes.display(), "r");
        assert_eq!(config.keybindings.filter.display(), "Space/");
    }

    #[test]
    fn test_backwards_compatible_without_filter_key() {
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
        assert_eq!(config.editor.as_deref(), Some("vim"));
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
    fn test_issue_list_keybinding_serialize_roundtrip() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("issue_list"));
        let parsed: KeybindingsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.issue_list.display(), "I");
    }

    /// KeybindingsConfig の全フィールドが serialize に含まれることを検証。
    /// 新しいフィールドを追加した際に serialize_entry の追加を忘れるとここで落ちる。
    #[test]
    fn test_all_keybinding_fields_are_serialized() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();

        let expected_fields = [
            "move_down",
            "move_up",
            "move_left",
            "move_right",
            "page_down",
            "page_up",
            "jump_to_first",
            "jump_to_last",
            "jump_back",
            "next_comment",
            "prev_comment",
            "approve",
            "request_changes",
            "comment",
            "suggestion",
            "reply",
            "refresh",
            "submit",
            "quit",
            "help",
            "comment_list",
            "ai_rally",
            "open_panel",
            "go_to_definition",
            "go_to_file",
            "open_in_browser",
            "toggle_local_mode",
            "toggle_auto_focus",
            "toggle_zen_mode",
            "toggle_markdown_rich",
            "filter",
            "multiline_select",
            "pr_description",
            "ci_checks",
            "git_ops",
            "git_ops_stage",
            "git_ops_stage_all",
            "git_ops_discard",
            "git_ops_commit",
            "git_ops_undo",
            "git_ops_push",
            "issue_list",
            "tab_switch",
            "mark_viewed",
            "mark_viewed_dir",
            "tree_toggle",
        ];

        for field in &expected_fields {
            assert!(
                serialized.contains(field),
                "KeybindingsConfig field '{}' is missing from Serialize impl",
                field
            );
        }

        let parsed: KeybindingsConfig = toml::from_str(&serialized).unwrap();
        let reserialized = toml::to_string(&parsed).unwrap();
        assert_eq!(
            serialized, reserialized,
            "Serialize roundtrip mismatch — a field may be missing from Serialize or Default impl"
        );
    }

    /// validate() の bindings リストが全フィールドを含むことを検証。
    #[test]
    fn test_validate_covers_all_serialized_fields() {
        let config = KeybindingsConfig::default();
        let serialized = toml::to_string(&config).unwrap();

        let serialized_keys: Vec<&str> = serialized
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.contains(" = ") && !line.starts_with('#') && !line.starts_with('[') {
                    line.split(" = ").next()
                } else {
                    None
                }
            })
            .collect();

        assert!(config.validate().is_ok(), "Default keybindings should validate without errors");

        assert!(
            !serialized_keys.is_empty(),
            "Serialized keybindings should not be empty"
        );
    }

    #[test]
    fn test_zen_mode_deserialization() {
        let toml = r#"
[layout]
zen_mode = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.layout.zen_mode);

        let default_config = Config::default();
        assert!(!default_config.layout.zen_mode);
    }

    #[test]
    fn test_layout_left_panel_width_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.layout.left_panel_width, 35);
    }

    #[test]
    fn test_layout_left_panel_width_custom() {
        let toml_str = r#"
            [layout]
            left_panel_width = 50
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.layout.left_panel_width, 50);
    }

    #[test]
    fn test_layout_left_panel_width_below_min_clamped() {
        let toml_str = r#"
            [layout]
            left_panel_width = 5
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.layout.left_panel_width, 10,
            "Values below 10 should be clamped to 10"
        );
    }

    #[test]
    fn test_layout_left_panel_width_above_max_clamped() {
        let toml_str = r#"
            [layout]
            left_panel_width = 95
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.layout.left_panel_width, 90,
            "Values above 90 should be clamped to 90"
        );
    }

    #[test]
    fn test_layout_left_panel_width_boundary() {
        let config_10: Config = toml::from_str(
            r#"
            [layout]
            left_panel_width = 10
        "#,
        )
        .unwrap();
        assert_eq!(config_10.layout.left_panel_width, 10);

        let config_90: Config = toml::from_str(
            r#"
            [layout]
            left_panel_width = 90
        "#,
        )
        .unwrap();
        assert_eq!(config_90.layout.left_panel_width, 90);
    }

    #[test]
    fn test_layout_zen_mode_default() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.layout.zen_mode);
    }

    #[test]
    fn test_layout_zen_mode_custom() {
        let toml_str = r#"
            [layout]
            zen_mode = true
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.layout.zen_mode);
    }

    #[test]
    fn test_layout_helper_methods() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.layout.left_panel_percent(), 35);
        assert_eq!(config.layout.right_panel_percent(), 65);

        let config_50: Config = toml::from_str(
            r#"
            [layout]
            left_panel_width = 50
        "#,
        )
        .unwrap();
        assert_eq!(config_50.layout.left_panel_percent(), 50);
        assert_eq!(config_50.layout.right_panel_percent(), 50);
    }

    #[test]
    fn test_deep_merge_layout_left_panel_width_local_override() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[layout]
left_panel_width = 40
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[layout]
left_panel_width = 25
"#,
        )
        .unwrap();

        let config =
            Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.layout.left_panel_width, 25);
    }

    #[test]
    fn test_deep_merge_layout_left_panel_width_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(
            &global,
            r#"
[layout]
left_panel_width = 35
"#,
        )
        .unwrap();
        fs::write(
            &local,
            r#"
[layout]
left_panel_width = 0
"#,
        )
        .unwrap();

        let config =
            Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert_eq!(config.layout.left_panel_width, 10);
    }

    #[test]
    fn test_layout_override_keys_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");

        fs::write(&global, "").unwrap();
        fs::write(
            &local,
            r#"
[layout]
left_panel_width = 50
zen_mode = true
"#,
        )
        .unwrap();

        let config =
            Config::load_from_paths(&global, &local, dir.path().to_path_buf()).unwrap();
        assert!(
            config.local_overrides.contains("layout.left_panel_width"),
            "layout.left_panel_width should be tracked as local override"
        );
        assert!(
            config.local_overrides.contains("layout.zen_mode"),
            "layout.zen_mode should be tracked as local override"
        );
    }
}
