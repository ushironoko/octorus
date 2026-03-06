use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use xdg::BaseDirectories;

use octorus::config::find_project_root;

/// Default config.toml content
const DEFAULT_CONFIG: &str = r#"# Editor for writing review body.
# Resolved in order: this value → $VISUAL → $EDITOR → vi
# Supports arguments: editor = "code --wait"
# editor = "vim"

[diff]
theme = "base16-ocean.dark"
# Number of spaces per tab character in diff view (minimum: 1)
tab_width = 4

[keybindings]
approve = 'a'
request_changes = 'r'
comment = 'c'
suggestion = 's'

[ai]
reviewer = "claude"
reviewee = "claude"
max_iterations = 10
timeout_secs = 600
# prompt_dir = "/custom/path/to/prompts"  # Optional: custom prompt directory

# Additional tools for reviewer agent (Claude only)
# Specify in Claude Code --allowedTools format
# reviewer_additional_tools = ["Skill", "WebSearch"]

# Additional tools for reviewee agent (Claude only)
# NOTE: git push is disabled by default for safety.
# To enable automatic push, add "Bash(git push:*)" to this list.
# reviewee_additional_tools = ["Skill", "Bash(git push:*)"]
"#;

/// Default prompt templates (same as embedded in binary)
const DEFAULT_REVIEWER_PROMPT: &str = include_str!("ai/defaults/reviewer.md");
const DEFAULT_REVIEWEE_PROMPT: &str = include_str!("ai/defaults/reviewee.md");
const DEFAULT_REREVIEW_PROMPT: &str = include_str!("ai/defaults/rereview.md");

/// Agent skill content for Claude Code integration
const AGENT_SKILL_CONTENT: &str = include_str!("ai/defaults/skill.md");

/// Default local config.toml content
const DEFAULT_LOCAL_CONFIG: &str = r#"# Project-local octorus configuration.
# Values here override the global config (~/.config/octorus/config.toml).
# Only specify values you want to override.

# [diff]
# theme = "base16-ocean.dark"
# tab_width = 4

# [ai]
# reviewer = "claude"
# reviewee = "claude"
# max_iterations = 10
# timeout_secs = 600
"#;

/// Run the init command
pub fn run_init(force: bool, local: bool) -> Result<()> {
    if local {
        let project_root = find_project_root();
        return run_init_local(&project_root, force);
    }
    let base_dirs =
        BaseDirectories::with_prefix("octorus").context("Failed to get config directory")?;

    let config_home = base_dirs.get_config_home();

    // Create config directory if needed
    if !config_home.exists() {
        println!(
            "Creating configuration directory: {}",
            config_home.display()
        );
        fs::create_dir_all(&config_home).context("Failed to create config directory")?;
    }

    // Write config.toml
    let config_path = config_home.join("config.toml");
    write_file_if_needed(&config_path, DEFAULT_CONFIG, force, "config.toml")?;

    // Create prompts directory
    let prompts_dir = config_home.join("prompts");
    if !prompts_dir.exists() {
        println!("Creating prompts directory: {}", prompts_dir.display());
        fs::create_dir_all(&prompts_dir).context("Failed to create prompts directory")?;
    }

    // Write prompt templates
    write_file_if_needed(
        &prompts_dir.join("reviewer.md"),
        DEFAULT_REVIEWER_PROMPT,
        force,
        "reviewer.md",
    )?;
    write_file_if_needed(
        &prompts_dir.join("reviewee.md"),
        DEFAULT_REVIEWEE_PROMPT,
        force,
        "reviewee.md",
    )?;
    write_file_if_needed(
        &prompts_dir.join("rereview.md"),
        DEFAULT_REREVIEW_PROMPT,
        force,
        "rereview.md",
    )?;

    // Generate agent skill for Claude Code (if ~/.claude exists)
    if let Err(e) = generate_agent_skill(force) {
        eprintln!("Warning: Failed to generate agent skill: {}", e);
    }

    println!();
    println!("Initialization complete!");
    println!();
    println!(
        "You can customize prompts by editing files in {}",
        prompts_dir.display()
    );
    println!("Available template variables: {{{{repo}}}}, {{{{pr_number}}}}, {{{{pr_title}}}}, {{{{diff}}}}, etc.");

    Ok(())
}

/// Write a file if it doesn't exist or force is true
fn write_file_if_needed(path: &PathBuf, content: &str, force: bool, name: &str) -> Result<()> {
    if path.exists() && !force {
        println!(
            "Skipping {} (already exists, use --force to overwrite)",
            name
        );
        return Ok(());
    }

    println!("Writing {}...", name);
    fs::write(path, content).with_context(|| format!("Failed to write {}", name))?;
    Ok(())
}

/// Generate agent skill file in the given claude directory (testable core)
fn generate_agent_skill_in(claude_dir: &Path, force: bool) -> Result<()> {
    let skill_dir = claude_dir.join("skills").join("octorus");
    if !skill_dir.exists() {
        fs::create_dir_all(&skill_dir).context("Failed to create agent skill directory")?;
    }
    write_file_if_needed(
        &skill_dir.join("SKILL.md"),
        AGENT_SKILL_CONTENT,
        force,
        "SKILL.md (agent skill)",
    )
}

/// Generate agent skill for Claude Code (if ~/.claude exists)
fn generate_agent_skill(force: bool) -> Result<()> {
    let claude_dir = match std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".claude"))
    {
        Some(dir) if dir.is_dir() => dir,
        _ => return Ok(()),
    };
    generate_agent_skill_in(&claude_dir, force)
}

/// Run init for project-local .octorus/ directory
fn run_init_local(project_root: &Path, force: bool) -> Result<()> {
    let octorus_dir = project_root.join(".octorus");

    // Create .octorus directory if needed
    if !octorus_dir.exists() {
        println!("Creating local config directory: {}", octorus_dir.display());
        fs::create_dir_all(&octorus_dir).context("Failed to create .octorus directory")?;
    }

    // Write config.toml
    let config_path = octorus_dir.join("config.toml");
    write_file_if_needed(&config_path, DEFAULT_LOCAL_CONFIG, force, "config.toml")?;

    // Create prompts directory
    let prompts_dir = octorus_dir.join("prompts");
    if !prompts_dir.exists() {
        println!("Creating prompts directory: {}", prompts_dir.display());
        fs::create_dir_all(&prompts_dir).context("Failed to create prompts directory")?;
    }

    // Write prompt templates
    write_file_if_needed(
        &prompts_dir.join("reviewer.md"),
        DEFAULT_REVIEWER_PROMPT,
        force,
        "reviewer.md",
    )?;
    write_file_if_needed(
        &prompts_dir.join("reviewee.md"),
        DEFAULT_REVIEWEE_PROMPT,
        force,
        "reviewee.md",
    )?;
    write_file_if_needed(
        &prompts_dir.join("rereview.md"),
        DEFAULT_REREVIEW_PROMPT,
        force,
        "rereview.md",
    )?;

    println!();
    println!("Local initialization complete!");
    println!("Project-local config: {}", config_path.display());
    println!("Project-local prompts: {}", prompts_dir.display());
    println!();
    println!(
        "\x1b[36mTip:\x1b[0m Commit .octorus/ to share project-specific settings with your team."
    );
    println!("     Or add .octorus/ to .gitignore for personal-only configuration.");
    println!();
    println!("\x1b[33mWarning:\x1b[0m .octorus/config.toml can override \x1b[1mALL\x1b[0m settings including editor,");
    println!("         AI tool permissions, and auto_post. If you commit .octorus/ to a");
    println!("         public repository, cloners will inherit these settings when running \x1b[1mor\x1b[0m.");
    println!("         Review the config carefully before committing.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper to run init with a specific temp directory
    fn run_init_in_temp_dir(temp_dir: &TempDir, force: bool) -> Result<()> {
        // Create the base directories manually to avoid env var issues
        let config_home = temp_dir.path().join("octorus");

        if !config_home.exists() {
            println!(
                "Creating configuration directory: {}",
                config_home.display()
            );
            fs::create_dir_all(&config_home)?;
        }

        let config_path = config_home.join("config.toml");
        write_file_if_needed(&config_path, DEFAULT_CONFIG, force, "config.toml")?;

        let prompts_dir = config_home.join("prompts");
        if !prompts_dir.exists() {
            println!("Creating prompts directory: {}", prompts_dir.display());
            fs::create_dir_all(&prompts_dir)?;
        }

        write_file_if_needed(
            &prompts_dir.join("reviewer.md"),
            DEFAULT_REVIEWER_PROMPT,
            force,
            "reviewer.md",
        )?;
        write_file_if_needed(
            &prompts_dir.join("reviewee.md"),
            DEFAULT_REVIEWEE_PROMPT,
            force,
            "reviewee.md",
        )?;
        write_file_if_needed(
            &prompts_dir.join("rereview.md"),
            DEFAULT_REREVIEW_PROMPT,
            force,
            "rereview.md",
        )?;

        Ok(())
    }

    #[test]
    fn test_run_init_creates_files() {
        let temp_dir = TempDir::new().unwrap();

        run_init_in_temp_dir(&temp_dir, false).unwrap();

        let config_path = temp_dir.path().join("octorus/config.toml");
        let prompts_dir = temp_dir.path().join("octorus/prompts");

        assert!(config_path.exists(), "config.toml should exist");
        assert!(
            prompts_dir.join("reviewer.md").exists(),
            "reviewer.md should exist"
        );
        assert!(
            prompts_dir.join("reviewee.md").exists(),
            "reviewee.md should exist"
        );
        assert!(
            prompts_dir.join("rereview.md").exists(),
            "rereview.md should exist"
        );

        let config_content = fs::read_to_string(&config_path).unwrap();
        assert!(config_content.contains("# editor = \"vim\""));
        assert!(config_content.contains("[ai]"));
    }

    #[test]
    fn test_run_init_skips_existing() {
        let temp_dir = TempDir::new().unwrap();

        // Create config directory and file with custom content
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        fs::write(&config_path, "custom = true").unwrap();

        // Run init without force
        run_init_in_temp_dir(&temp_dir, false).unwrap();

        // Verify custom content is preserved
        let content = fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, "custom = true");
    }

    #[test]
    fn test_run_init_force_overwrites() {
        let temp_dir = TempDir::new().unwrap();

        // Create config directory and file with custom content
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        fs::write(&config_path, "custom = true").unwrap();

        // Run init with force
        run_init_in_temp_dir(&temp_dir, true).unwrap();

        // Verify content was overwritten
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("# editor = \"vim\""));
        assert!(!content.contains("custom = true"));
    }

    #[test]
    fn test_run_init_local_creates_files() {
        let temp_dir = TempDir::new().unwrap();

        run_init_local(temp_dir.path(), false).unwrap();

        let octorus_dir = temp_dir.path().join(".octorus");
        let config_path = octorus_dir.join("config.toml");
        let prompts_dir = octorus_dir.join("prompts");

        assert!(config_path.exists(), "config.toml should exist");
        assert!(
            prompts_dir.join("reviewer.md").exists(),
            "reviewer.md should exist"
        );
        assert!(
            prompts_dir.join("reviewee.md").exists(),
            "reviewee.md should exist"
        );
        assert!(
            prompts_dir.join("rereview.md").exists(),
            "rereview.md should exist"
        );

        let config_content = fs::read_to_string(&config_path).unwrap();
        assert!(config_content.contains("Project-local octorus configuration"));
        assert!(config_content.contains("# [ai]"));
    }

    #[test]
    fn test_run_init_local_skips_existing() {
        let temp_dir = TempDir::new().unwrap();
        let octorus_dir = temp_dir.path().join(".octorus");
        fs::create_dir_all(&octorus_dir).unwrap();
        let config_path = octorus_dir.join("config.toml");
        fs::write(&config_path, "custom = true").unwrap();

        run_init_local(temp_dir.path(), false).unwrap();

        let content = fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, "custom = true");
    }

    #[test]
    fn test_run_init_local_force_overwrites() {
        let temp_dir = TempDir::new().unwrap();
        let octorus_dir = temp_dir.path().join(".octorus");
        fs::create_dir_all(&octorus_dir).unwrap();
        let config_path = octorus_dir.join("config.toml");
        fs::write(&config_path, "custom = true").unwrap();

        run_init_local(temp_dir.path(), true).unwrap();

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("Project-local octorus configuration"));
        assert!(!content.contains("custom = true"));
    }

    #[test]
    fn test_generate_agent_skill_creates_file() {
        let temp_dir = TempDir::new().unwrap();
        let claude_dir = temp_dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        generate_agent_skill_in(&claude_dir, false).unwrap();

        let skill_path = claude_dir.join("skills/octorus/SKILL.md");
        assert!(skill_path.exists(), "SKILL.md should exist");

        let content = fs::read_to_string(&skill_path).unwrap();
        assert!(content.contains("or"), "Should contain binary name 'or'");
        assert!(
            content.contains("--ai-rally"),
            "Should contain --ai-rally flag"
        );
    }

    #[test]
    fn test_generate_agent_skill_respects_force() {
        let temp_dir = TempDir::new().unwrap();
        let claude_dir = temp_dir.path().join(".claude");
        let skill_dir = claude_dir.join("skills/octorus");
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        fs::write(&skill_path, "custom content").unwrap();

        // force=false should skip
        generate_agent_skill_in(&claude_dir, false).unwrap();
        let content = fs::read_to_string(&skill_path).unwrap();
        assert_eq!(content, "custom content");

        // force=true should overwrite
        generate_agent_skill_in(&claude_dir, true).unwrap();
        let content = fs::read_to_string(&skill_path).unwrap();
        assert!(content.contains("--ai-rally"));
        assert!(!content.contains("custom content"));
    }

    #[test]
    fn test_generate_agent_skill_skips_when_claude_dir_missing() {
        let temp_dir = TempDir::new().unwrap();
        let claude_dir = temp_dir.path().join(".claude");
        // .claude does not exist — simulate the wrapper's behavior
        assert!(!claude_dir.is_dir());

        // The wrapper checks is_dir() and returns Ok(()) silently
        // Here we verify that generate_agent_skill_in still works
        // but the wrapper would never call it when the dir is missing.
        // Simulate the wrapper logic directly:
        let result = if claude_dir.is_dir() {
            generate_agent_skill_in(&claude_dir, false)
        } else {
            Ok(())
        };
        assert!(result.is_ok());

        let skill_path = claude_dir.join("skills/octorus/SKILL.md");
        assert!(
            !skill_path.exists(),
            "SKILL.md should not be created when .claude dir is missing"
        );
    }

    #[test]
    fn test_generate_agent_skill_skips_when_claude_is_file() {
        let temp_dir = TempDir::new().unwrap();
        let claude_path = temp_dir.path().join(".claude");
        // Create .claude as a file, not a directory
        fs::write(&claude_path, "not a directory").unwrap();
        assert!(!claude_path.is_dir());

        // Simulate the wrapper logic: is_dir() returns false for a file
        let result = if claude_path.is_dir() {
            generate_agent_skill_in(&claude_path, false)
        } else {
            Ok(())
        };
        assert!(result.is_ok());

        let skill_path = claude_path.join("skills/octorus/SKILL.md");
        assert!(
            !skill_path.exists(),
            "SKILL.md should not be created when .claude is a file"
        );
    }

    #[test]
    fn test_generate_agent_skill_creates_intermediate_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let claude_dir = temp_dir.path().join(".claude");
        // Only create .claude, not skills/octorus/
        fs::create_dir_all(&claude_dir).unwrap();

        generate_agent_skill_in(&claude_dir, false).unwrap();

        let skill_path = claude_dir.join("skills/octorus/SKILL.md");
        assert!(
            skill_path.exists(),
            "Should create intermediate directories and SKILL.md"
        );
    }
}
