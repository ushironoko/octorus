use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use xdg::BaseDirectories;

use crate::init::{
    AGENT_SKILL_CONTENT, AGENT_SKILL_REF_CONFIG, AGENT_SKILL_REF_HEADLESS, DEFAULT_CONFIG,
    DEFAULT_LOCAL_CONFIG, DEFAULT_REREVIEW_PROMPT, DEFAULT_REVIEWEE_PROMPT,
    DEFAULT_REVIEWER_PROMPT,
};
use crate::update::is_newer_version;
use octorus::config::find_project_root;

// ---------------------------------------------------------------------------
// Version Manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VersionManifest {
    pub binary_version: String,
    pub initialized_at: String,
    pub last_migrated_at: Option<String>,
    pub files: HashMap<String, FileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FileRecord {
    pub version: String,
    pub status: FileRecordStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) enum FileRecordStatus {
    Migrated,
    CustomizedSkipped,
    Created,
}

// ---------------------------------------------------------------------------
// File Status / Scope
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
enum FileStatus {
    UpToDate,
    MatchesPreviousDefault { version: String },
    Customized,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FileScope {
    Global,
    Local,
    Skill,
}

// ---------------------------------------------------------------------------
// Default File Hashes
// ---------------------------------------------------------------------------

struct DefaultFileHash {
    scope: FileScope,
    version: &'static str,
    filename: &'static str,
    sha256: &'static str,
}

/// Known SHA-256 hashes of default file contents for each version.
/// Hashes are computed after CRLF normalization (`\r\n` → `\n`).
const DEFAULT_HASHES: &[DefaultFileHash] = &[
    // Global config
    DefaultFileHash {
        scope: FileScope::Global,
        version: "0.5.6",
        filename: "config.toml",
        sha256: HASH_GLOBAL_CONFIG_0_5_6,
    },
    // Local config
    DefaultFileHash {
        scope: FileScope::Local,
        version: "0.5.6",
        filename: "config.toml",
        sha256: HASH_LOCAL_CONFIG_0_5_6,
    },
    // Prompts — global and local share the same defaults, so register both scopes
    DefaultFileHash {
        scope: FileScope::Global,
        version: "0.5.6",
        filename: "reviewer.md",
        sha256: HASH_REVIEWER_0_5_6,
    },
    DefaultFileHash {
        scope: FileScope::Local,
        version: "0.5.6",
        filename: "reviewer.md",
        sha256: HASH_REVIEWER_0_5_6,
    },
    DefaultFileHash {
        scope: FileScope::Global,
        version: "0.5.6",
        filename: "reviewee.md",
        sha256: HASH_REVIEWEE_0_5_6,
    },
    DefaultFileHash {
        scope: FileScope::Local,
        version: "0.5.6",
        filename: "reviewee.md",
        sha256: HASH_REVIEWEE_0_5_6,
    },
    DefaultFileHash {
        scope: FileScope::Global,
        version: "0.5.6",
        filename: "rereview.md",
        sha256: HASH_REREVIEW_0_5_6,
    },
    DefaultFileHash {
        scope: FileScope::Local,
        version: "0.5.6",
        filename: "rereview.md",
        sha256: HASH_REREVIEW_0_5_6,
    },
    // SKILL.md
    DefaultFileHash {
        scope: FileScope::Skill,
        version: "0.5.6",
        filename: "SKILL.md",
        sha256: HASH_SKILL_0_5_6,
    },
    // New version — SKILL.md rewrite + reference files
    DefaultFileHash {
        scope: FileScope::Skill,
        version: "0.6.0",
        filename: "SKILL.md",
        sha256: HASH_SKILL_0_6_0,
    },
    DefaultFileHash {
        scope: FileScope::Skill,
        version: "0.6.0",
        filename: "headless-output.md",
        sha256: HASH_SKILL_REF_HEADLESS_0_6_0,
    },
    DefaultFileHash {
        scope: FileScope::Skill,
        version: "0.6.0",
        filename: "config-reference.md",
        sha256: HASH_SKILL_REF_CONFIG_0_6_0,
    },
];

// These constants are verified by test_default_hashes_match_embedded_content.
// If you change a default file, run `cargo test test_default_hashes` to get the new hash.
const HASH_GLOBAL_CONFIG_0_5_6: &str =
    "c72fc993956ff633bce2d6841d96c1583f02c1b8d8c262e9b11f53a2f6ffcaea";
const HASH_LOCAL_CONFIG_0_5_6: &str =
    "dd3fbdd57e338f31a079e3c7a383fdfbb7f12db79e7be5430ad089e9a2fb3c60";
const HASH_REVIEWER_0_5_6: &str =
    "d9dfdd90d4041ef424edbab3754ab94bafbdad9d69e7297db195cf6194701e58";
const HASH_REVIEWEE_0_5_6: &str =
    "f90c784d4ff49062ace22c68fadfff41b9a6473fbaca5d0af8a24f53d13941c4";
const HASH_REREVIEW_0_5_6: &str =
    "725ca31c8a180bb9333ac7e15ef54a7477afbd27300d594db8c02f3b70f01e56";
const HASH_SKILL_0_5_6: &str = "c09f476002e139332d2d402d823a3ba8abd77f5ccd0c0f694c73e3b0337d9c7d";
const HASH_SKILL_0_6_0: &str = "ec3a390088044e1fd5a1a7b0b4acfdc8ce4e48a1ea3f935801c44889247891fe";
const HASH_SKILL_REF_HEADLESS_0_6_0: &str =
    "f89450eba0d680a394e504a384a1d8a5a0f318974b7b3ebcef55b6079131270a";
const HASH_SKILL_REF_CONFIG_0_6_0: &str =
    "24c757880aef964db0d8b08cc81fcca0be0d7031b6606893f3e25ab801be41e7";

// ---------------------------------------------------------------------------
// Config Migrations (breaking changes registry)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct ConfigMigration {
    from_version: &'static str,
    to_version: &'static str,
    description: &'static str,
    apply: fn(&str) -> Result<String>,
}

/// Registry of config.toml breaking-change migrations.
/// Add entries here when a new version introduces incompatible config changes.
const CONFIG_MIGRATIONS: &[ConfigMigration] = &[
    // Example for future use:
    // ConfigMigration {
    //     from_version: "0.5.6",
    //     to_version: "0.6.0",
    //     description: "Rename [ai] reviewer_additional_tools to reviewer_tools",
    //     apply: migrate_config_0_5_6_to_0_6_0,
    // },
];

// ---------------------------------------------------------------------------
// Migration Actions
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum MigrationAction {
    ReplaceDefault {
        path: PathBuf,
        content: String,
        description: String,
    },
    SkipUpToDate {
        path: PathBuf,
        reason: String,
    },
    SkipCustomized {
        path: PathBuf,
        reason: String,
    },
    MigrateConfig {
        path: PathBuf,
        migrations: Vec<usize>, // indices into CONFIG_MIGRATIONS
    },
    CreateNew {
        path: PathBuf,
        content: String,
        description: String,
    },
    WriteManifest {
        path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Hash / Status helpers
// ---------------------------------------------------------------------------

/// Compute SHA-256 hash of content after CRLF normalization.
pub(crate) fn content_hash(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Detect the originating version of a file by comparing its content hash
/// against known default hashes. Returns `Some(version)` if matched,
/// `None` if the content doesn't match any known default.
pub(crate) fn detect_version_from_hash(
    file_content: &str,
    filename: &str,
    is_local: bool,
) -> Option<String> {
    let hash = content_hash(file_content);
    let scope = if is_skill_file(filename) {
        FileScope::Skill
    } else if is_local {
        FileScope::Local
    } else {
        FileScope::Global
    };

    DEFAULT_HASHES
        .iter()
        .find(|h| h.scope == scope && h.filename == filename && h.sha256 == hash)
        .map(|h| h.version.to_string())
}

/// Determine the status of a file by comparing its hash against known defaults.
fn check_file_status(
    file_content: &str,
    current_version: &str,
    scope: FileScope,
    filename: &str,
) -> FileStatus {
    let hash = content_hash(file_content);

    // Check if it matches the current version's default
    let current_default = DEFAULT_HASHES
        .iter()
        .find(|h| h.scope == scope && h.filename == filename && h.version == current_version);
    if let Some(def) = current_default {
        if hash == def.sha256 {
            return FileStatus::UpToDate;
        }
    }

    // Check if it matches any previous version's default
    for def in DEFAULT_HASHES.iter() {
        if def.scope == scope && def.filename == filename && def.version != current_version {
            if hash == def.sha256 {
                return FileStatus::MatchesPreviousDefault {
                    version: def.version.to_string(),
                };
            }
        }
    }

    FileStatus::Customized
}

// ---------------------------------------------------------------------------
// Manifest I/O
// ---------------------------------------------------------------------------

pub(crate) fn read_manifest(path: &Path) -> Option<VersionManifest> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn write_manifest(path: &Path, manifest: &VersionManifest) -> Result<()> {
    let json =
        serde_json::to_string_pretty(manifest).context("Failed to serialize version manifest")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    fs::write(path, json).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Create a new manifest for the current binary version with all files as Created.
fn bootstrap_manifest(version: &str) -> VersionManifest {
    VersionManifest {
        binary_version: version.to_string(),
        initialized_at: now_iso(),
        last_migrated_at: None,
        files: HashMap::new(),
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Default content lookup
// ---------------------------------------------------------------------------

/// Get the current default content for a file by scope and filename.
fn get_default_content(scope: FileScope, filename: &str) -> Option<&'static str> {
    match (scope, filename) {
        (FileScope::Global, "config.toml") => Some(DEFAULT_CONFIG),
        (FileScope::Local, "config.toml") => Some(DEFAULT_LOCAL_CONFIG),
        (FileScope::Global | FileScope::Local, "reviewer.md") => Some(DEFAULT_REVIEWER_PROMPT),
        (FileScope::Global | FileScope::Local, "reviewee.md") => Some(DEFAULT_REVIEWEE_PROMPT),
        (FileScope::Global | FileScope::Local, "rereview.md") => Some(DEFAULT_REREVIEW_PROMPT),
        (FileScope::Skill, "SKILL.md") => Some(AGENT_SKILL_CONTENT),
        (FileScope::Skill, "headless-output.md") => Some(AGENT_SKILL_REF_HEADLESS),
        (FileScope::Skill, "config-reference.md") => Some(AGENT_SKILL_REF_CONFIG),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Managed file definitions
// ---------------------------------------------------------------------------

struct ManagedFile {
    scope: FileScope,
    filename: &'static str,
    /// Relative path from the scope's root directory.
    relative_path: &'static str,
}

const MANAGED_FILES_GLOBAL: &[ManagedFile] = &[
    ManagedFile {
        scope: FileScope::Global,
        filename: "config.toml",
        relative_path: "config.toml",
    },
    ManagedFile {
        scope: FileScope::Global,
        filename: "reviewer.md",
        relative_path: "prompts/reviewer.md",
    },
    ManagedFile {
        scope: FileScope::Global,
        filename: "reviewee.md",
        relative_path: "prompts/reviewee.md",
    },
    ManagedFile {
        scope: FileScope::Global,
        filename: "rereview.md",
        relative_path: "prompts/rereview.md",
    },
];

const MANAGED_FILES_LOCAL: &[ManagedFile] = &[
    ManagedFile {
        scope: FileScope::Local,
        filename: "config.toml",
        relative_path: "config.toml",
    },
    ManagedFile {
        scope: FileScope::Local,
        filename: "reviewer.md",
        relative_path: "prompts/reviewer.md",
    },
    ManagedFile {
        scope: FileScope::Local,
        filename: "reviewee.md",
        relative_path: "prompts/reviewee.md",
    },
    ManagedFile {
        scope: FileScope::Local,
        filename: "rereview.md",
        relative_path: "prompts/rereview.md",
    },
];

const MANAGED_FILES_SKILL: &[ManagedFile] = &[
    ManagedFile {
        scope: FileScope::Skill,
        filename: "SKILL.md",
        relative_path: "skills/octorus/SKILL.md",
    },
    ManagedFile {
        scope: FileScope::Skill,
        filename: "headless-output.md",
        relative_path: "skills/octorus/references/headless-output.md",
    },
    ManagedFile {
        scope: FileScope::Skill,
        filename: "config-reference.md",
        relative_path: "skills/octorus/references/config-reference.md",
    },
];

/// Check if a filename belongs to a skill file.
fn is_skill_file(filename: &str) -> bool {
    MANAGED_FILES_SKILL.iter().any(|mf| mf.filename == filename)
}

// ---------------------------------------------------------------------------
// Plan construction
// ---------------------------------------------------------------------------

fn build_migration_plan(
    config_dir: &Path,
    skill_dir: Option<&Path>,
    manifest: &Option<VersionManifest>,
    binary_version: &str,
    is_local: bool,
    force: bool,
) -> Vec<MigrationAction> {
    let mut actions = Vec::new();
    let managed_files = if is_local {
        MANAGED_FILES_LOCAL
    } else {
        MANAGED_FILES_GLOBAL
    };

    for mf in managed_files {
        let file_path = config_dir.join(mf.relative_path);
        let action = plan_file_action(
            &file_path,
            mf.scope,
            mf.filename,
            manifest,
            binary_version,
            force,
        );
        actions.push(action);
    }

    // Skill files — global only
    if !is_local {
        if let Some(skill_root) = skill_dir {
            for mf in MANAGED_FILES_SKILL {
                let skill_path = skill_root.join(mf.relative_path);
                actions.push(plan_file_action(
                    &skill_path,
                    mf.scope,
                    mf.filename,
                    manifest,
                    binary_version,
                    force,
                ));
            }
        }
    }

    // Always write manifest
    let manifest_path = config_dir.join(".version");
    actions.push(MigrationAction::WriteManifest {
        path: manifest_path,
    });

    actions
}

fn plan_file_action(
    file_path: &Path,
    scope: FileScope,
    filename: &str,
    manifest: &Option<VersionManifest>,
    binary_version: &str,
    force: bool,
) -> MigrationAction {
    let default_content = match get_default_content(scope, filename) {
        Some(c) => c,
        None => {
            return MigrationAction::SkipCustomized {
                path: file_path.to_path_buf(),
                reason: "No default content available".to_string(),
            };
        }
    };

    // File doesn't exist
    if !file_path.exists() {
        return MigrationAction::CreateNew {
            path: file_path.to_path_buf(),
            content: default_content.to_string(),
            description: format!("Create missing {}", filename),
        };
    }

    // Force mode — always replace
    if force {
        return MigrationAction::ReplaceDefault {
            path: file_path.to_path_buf(),
            content: default_content.to_string(),
            description: format!("Force update {}", filename),
        };
    }

    // Read current content
    let current_content = match fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => {
            return MigrationAction::SkipCustomized {
                path: file_path.to_path_buf(),
                reason: format!("Cannot read {}", filename),
            };
        }
    };

    let status = check_file_status(&current_content, binary_version, scope, filename);

    // For config.toml, check if there are applicable ConfigMigrations
    if filename == "config.toml" {
        let file_version = manifest
            .as_ref()
            .and_then(|m| m.files.get(filename))
            .map(|r| r.version.as_str())
            .unwrap_or("0.0.0");

        let applicable: Vec<usize> = CONFIG_MIGRATIONS
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                is_newer_version(file_version, m.to_version)
                    && !is_newer_version(file_version, m.from_version)
                    || file_version == m.from_version
            })
            .map(|(i, _)| i)
            .collect();

        if !applicable.is_empty() {
            return MigrationAction::MigrateConfig {
                path: file_path.to_path_buf(),
                migrations: applicable,
            };
        }
    }

    match status {
        FileStatus::UpToDate => MigrationAction::SkipUpToDate {
            path: file_path.to_path_buf(),
            reason: format!("{} is already up to date", filename),
        },
        FileStatus::MatchesPreviousDefault { version } => MigrationAction::ReplaceDefault {
            path: file_path.to_path_buf(),
            content: default_content.to_string(),
            description: format!(
                "Update {} (previous default v{} detected)",
                filename, version
            ),
        },
        FileStatus::Customized => MigrationAction::SkipCustomized {
            path: file_path.to_path_buf(),
            reason: format!("{} has been customized", filename),
        },
        FileStatus::Missing => MigrationAction::CreateNew {
            path: file_path.to_path_buf(),
            content: default_content.to_string(),
            description: format!("Create missing {}", filename),
        },
    }
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

fn create_backup(config_dir: &Path, skill_dir: Option<&Path>) -> Result<PathBuf> {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut backup_dir = config_dir.join(format!(".backup-{}", timestamp));

    // Handle collision
    let mut counter = 0;
    while backup_dir.exists() {
        counter += 1;
        backup_dir = config_dir.join(format!(".backup-{}-{}", timestamp, counter));
    }

    fs::create_dir_all(&backup_dir).context("Failed to create backup directory")?;

    // Copy all managed files that exist in config_dir
    for entry in fs::read_dir(config_dir).context("Failed to read config directory")? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip backup dirs themselves and .version
        if name_str.starts_with(".backup-") || name_str == ".version" {
            continue;
        }

        if path.is_file() {
            fs::copy(&path, backup_dir.join(&name))
                .with_context(|| format!("Failed to backup {}", name_str))?;
        } else if path.is_dir() && name_str == "prompts" {
            let prompts_backup = backup_dir.join("prompts");
            fs::create_dir_all(&prompts_backup)?;
            for pentry in fs::read_dir(&path)? {
                let pentry = pentry?;
                if pentry.path().is_file() {
                    fs::copy(pentry.path(), prompts_backup.join(pentry.file_name()))?;
                }
            }
        }
    }

    // Backup skill files if they exist (live outside config_dir under ~/.claude/)
    if let Some(skill_root) = skill_dir {
        for mf in MANAGED_FILES_SKILL {
            let path = skill_root.join(mf.relative_path);
            if path.exists() {
                let backup_path = backup_dir.join(mf.relative_path);
                if let Some(parent) = backup_path.parent() {
                    fs::create_dir_all(parent)
                        .context("Failed to create skill backup directory")?;
                }
                fs::copy(&path, &backup_path)
                    .with_context(|| format!("Failed to backup {}", mf.filename))?;
            }
        }
    }

    Ok(backup_dir)
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

fn execute_plan(
    actions: &[MigrationAction],
    manifest: &mut VersionManifest,
    binary_version: &str,
    is_local: bool,
) -> Result<()> {
    for action in actions {
        match action {
            MigrationAction::ReplaceDefault {
                path,
                content,
                description: _,
            } => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, content)
                    .with_context(|| format!("Failed to write {}", path.display()))?;

                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                manifest.files.insert(
                    filename,
                    FileRecord {
                        version: binary_version.to_string(),
                        status: FileRecordStatus::Migrated,
                    },
                );
            }
            MigrationAction::SkipUpToDate { path, reason: _ } => {
                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();

                // File matches the current default — record as Migrated
                manifest.files.insert(
                    filename,
                    FileRecord {
                        version: binary_version.to_string(),
                        status: FileRecordStatus::Migrated,
                    },
                );
            }
            MigrationAction::SkipCustomized { path, reason: _ } => {
                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();

                // Don't advance the version for skipped files
                if !manifest.files.contains_key(&filename) {
                    // First time seeing this file — detect version from content hash
                    // or fall back to "0.0.0" to ensure future migrations aren't skipped
                    let detected_version = fs::read_to_string(path)
                        .ok()
                        .and_then(|content| detect_version_from_hash(&content, &filename, is_local))
                        .unwrap_or_else(|| "0.0.0".to_string());
                    manifest.files.insert(
                        filename,
                        FileRecord {
                            version: detected_version,
                            status: FileRecordStatus::CustomizedSkipped,
                        },
                    );
                } else {
                    // Already tracked — update status but keep existing version
                    if let Some(record) = manifest.files.get_mut(&filename) {
                        record.status = FileRecordStatus::CustomizedSkipped;
                    }
                }
            }
            MigrationAction::MigrateConfig { path, migrations } => {
                let mut content = fs::read_to_string(path)
                    .with_context(|| format!("Failed to read {}", path.display()))?;

                for &idx in migrations {
                    let migration = &CONFIG_MIGRATIONS[idx];
                    content = (migration.apply)(&content)?;
                }

                fs::write(path, &content)
                    .with_context(|| format!("Failed to write {}", path.display()))?;

                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                manifest.files.insert(
                    filename,
                    FileRecord {
                        version: binary_version.to_string(),
                        status: FileRecordStatus::Migrated,
                    },
                );
            }
            MigrationAction::CreateNew {
                path,
                content,
                description: _,
            } => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, content)
                    .with_context(|| format!("Failed to write {}", path.display()))?;

                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                manifest.files.insert(
                    filename,
                    FileRecord {
                        version: binary_version.to_string(),
                        status: FileRecordStatus::Created,
                    },
                );
            }
            MigrationAction::WriteManifest { path } => {
                manifest.binary_version = binary_version.to_string();
                manifest.last_migrated_at = Some(now_iso());
                write_manifest(path, manifest)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn display_plan(actions: &[MigrationAction]) {
    for action in actions {
        match action {
            MigrationAction::ReplaceDefault {
                path, description, ..
            } => {
                let filename = path.file_name().unwrap_or_default().to_string_lossy();
                println!("  \x1b[32m→\x1b[0m {} — {}", filename, description);
            }
            MigrationAction::SkipUpToDate { path, reason } => {
                let filename = path.file_name().unwrap_or_default().to_string_lossy();
                println!("  \x1b[32m✓\x1b[0m {} — {}", filename, reason);
            }
            MigrationAction::SkipCustomized { path, reason } => {
                let filename = path.file_name().unwrap_or_default().to_string_lossy();
                println!("  \x1b[33m✗\x1b[0m {} — skip ({})", filename, reason);
            }
            MigrationAction::MigrateConfig { path, migrations } => {
                let filename = path.file_name().unwrap_or_default().to_string_lossy();
                let descriptions: Vec<&str> = migrations
                    .iter()
                    .map(|&i| CONFIG_MIGRATIONS[i].description)
                    .collect();
                println!(
                    "  \x1b[34m↑\x1b[0m {} — config migration: {}",
                    filename,
                    descriptions.join(", ")
                );
            }
            MigrationAction::CreateNew {
                path, description, ..
            } => {
                let filename = path.file_name().unwrap_or_default().to_string_lossy();
                println!("  \x1b[36m+\x1b[0m {} — {}", filename, description);
            }
            MigrationAction::WriteManifest { .. } => {
                // Don't display manifest write in plan output
            }
        }
    }
}

fn has_meaningful_actions(actions: &[MigrationAction]) -> bool {
    actions.iter().any(|a| {
        matches!(
            a,
            MigrationAction::ReplaceDefault { .. }
                | MigrationAction::MigrateConfig { .. }
                | MigrationAction::CreateNew { .. }
        )
    })
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run_migrate(dry_run: bool, is_local: bool, force: bool) -> Result<()> {
    let binary_version = env!("CARGO_PKG_VERSION");

    // Determine target directories
    let (config_dir, skill_dir) = if is_local {
        let project_root = find_project_root();
        let octorus_dir = project_root.join(".octorus");
        (octorus_dir, None)
    } else {
        let base_dirs =
            BaseDirectories::with_prefix("octorus").context("Failed to get config directory")?;
        let config_home = base_dirs.get_config_home();

        let claude_dir = std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".claude"))
            .filter(|d| d.is_dir());

        (config_home, claude_dir)
    };

    run_migrate_in(
        &config_dir,
        skill_dir.as_deref(),
        dry_run,
        is_local,
        force,
        binary_version,
    )
}

/// Core migration logic, separated for testability.
fn run_migrate_in(
    config_dir: &Path,
    skill_dir: Option<&Path>,
    dry_run: bool,
    is_local: bool,
    force: bool,
    binary_version: &str,
) -> Result<()> {
    // Check if config dir exists at all
    if !config_dir.exists() {
        bail!(
            "Configuration directory does not exist: {}\nRun `or init{}` first.",
            config_dir.display(),
            if is_local { " --local" } else { "" }
        );
    }

    // Read manifest
    let manifest_path = config_dir.join(".version");
    let manifest = read_manifest(&manifest_path);

    // Handle corrupted manifest
    if manifest_path.exists() && manifest.is_none() {
        eprintln!(
            "\x1b[33mWarning:\x1b[0m .version file is corrupted. Proceeding in bootstrap mode."
        );
    }

    // Check if manifest version is newer than binary
    if let Some(ref m) = manifest {
        if is_newer_version(binary_version, &m.binary_version) {
            bail!(
                "Configuration is from a newer version (v{}) than the current binary (v{}).\n\
                 Run `or update` first to update the binary.",
                m.binary_version,
                binary_version
            );
        }
    }

    // Build plan
    let actions = build_migration_plan(
        &config_dir,
        skill_dir.as_deref(),
        &manifest,
        binary_version,
        is_local,
        force,
    );

    // Check if all up to date
    if !has_meaningful_actions(&actions) && !force {
        println!("Already up to date (v{})", binary_version);
        // Still write manifest to track state
        if manifest.is_none() {
            let mut m = bootstrap_manifest(binary_version);
            // Record existing files
            for action in &actions {
                match action {
                    MigrationAction::SkipUpToDate { path, .. } => {
                        let filename = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        m.files.insert(
                            filename,
                            FileRecord {
                                version: binary_version.to_string(),
                                status: FileRecordStatus::Migrated,
                            },
                        );
                    }
                    MigrationAction::SkipCustomized { path, .. } => {
                        let filename = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        // Detect version from content hash or fall back to "0.0.0"
                        // to preserve provenance and ensure future migrations aren't skipped
                        let detected_version = fs::read_to_string(path)
                            .ok()
                            .and_then(|content| {
                                detect_version_from_hash(&content, &filename, is_local)
                            })
                            .unwrap_or_else(|| "0.0.0".to_string());
                        m.files.insert(
                            filename,
                            FileRecord {
                                version: detected_version,
                                status: FileRecordStatus::CustomizedSkipped,
                            },
                        );
                    }
                    _ => continue,
                }
            }
            m.last_migrated_at = Some(now_iso());
            if !dry_run {
                write_manifest(&manifest_path, &m)?;
            }
        }
        return Ok(());
    }

    // Display plan
    println!("Migration plan (v{}):", binary_version);
    println!();
    display_plan(&actions);
    println!();

    if dry_run {
        println!("Dry run — no changes applied.");
        return Ok(());
    }

    // Create backup (include skill_dir for global migrations so SKILL.md is backed up)
    let backup_dir = create_backup(&config_dir, skill_dir.as_deref())
        .context("Failed to create backup. No changes have been applied.")?;
    println!("Backup created: {}", backup_dir.display());

    // Execute
    let mut working_manifest = manifest.unwrap_or_else(|| bootstrap_manifest(binary_version));

    if let Err(e) = execute_plan(&actions, &mut working_manifest, binary_version, is_local) {
        eprintln!("\x1b[31mError during migration:\x1b[0m {}", e);
        eprintln!();
        eprintln!(
            "A backup of your configuration exists at: {}",
            backup_dir.display()
        );
        eprintln!("You can restore it manually if needed.");
        return Err(e);
    }

    println!();
    println!("Migration complete!");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // === Step 1: Hash computation ===

    #[test]
    fn test_content_hash_deterministic() {
        let content = "hello world";
        let h1 = content_hash(content);
        let h2 = content_hash(content);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn test_content_hash_normalizes_crlf() {
        let lf = "line1\nline2\n";
        let crlf = "line1\r\nline2\r\n";
        assert_eq!(content_hash(lf), content_hash(crlf));
    }

    #[test]
    fn test_content_hash_different_content() {
        assert_ne!(content_hash("aaa"), content_hash("bbb"));
    }

    // === Step 2: FileStatus ===

    #[test]
    fn test_file_status_up_to_date() {
        let status = check_file_status(
            DEFAULT_REVIEWER_PROMPT,
            "0.5.6",
            FileScope::Global,
            "reviewer.md",
        );
        assert_eq!(status, FileStatus::UpToDate);
    }

    #[test]
    fn test_file_status_customized() {
        let status = check_file_status(
            "my custom content that matches no defaults",
            "0.5.6",
            FileScope::Global,
            "reviewer.md",
        );
        assert_eq!(status, FileStatus::Customized);
    }

    // === Step 3: Manifest ===

    #[test]
    fn test_manifest_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join(".version");

        let mut files = HashMap::new();
        files.insert(
            "config.toml".to_string(),
            FileRecord {
                version: "0.5.6".to_string(),
                status: FileRecordStatus::Migrated,
            },
        );
        let manifest = VersionManifest {
            binary_version: "0.5.6".to_string(),
            initialized_at: "2024-01-01T00:00:00Z".to_string(),
            last_migrated_at: Some("2024-06-01T00:00:00Z".to_string()),
            files,
        };

        write_manifest(&path, &manifest).unwrap();

        let loaded = read_manifest(&path).unwrap();
        assert_eq!(loaded.binary_version, "0.5.6");
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            loaded.files["config.toml"].status,
            FileRecordStatus::Migrated
        );
    }

    #[test]
    fn test_manifest_missing_returns_none() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join(".version");
        assert!(read_manifest(&path).is_none());
    }

    #[test]
    fn test_manifest_corrupted_returns_none() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join(".version");
        fs::write(&path, "not valid json {{{").unwrap();
        assert!(read_manifest(&path).is_none());
    }

    // === Step 4: Default hash integrity ===

    #[test]
    fn test_default_hashes_match_embedded_content() {
        let pairs: Vec<(FileScope, &str, &str, &str)> = vec![
            (
                FileScope::Global,
                "config.toml",
                DEFAULT_CONFIG,
                HASH_GLOBAL_CONFIG_0_5_6,
            ),
            (
                FileScope::Local,
                "config.toml",
                DEFAULT_LOCAL_CONFIG,
                HASH_LOCAL_CONFIG_0_5_6,
            ),
            (
                FileScope::Global,
                "reviewer.md",
                DEFAULT_REVIEWER_PROMPT,
                HASH_REVIEWER_0_5_6,
            ),
            (
                FileScope::Global,
                "reviewee.md",
                DEFAULT_REVIEWEE_PROMPT,
                HASH_REVIEWEE_0_5_6,
            ),
            (
                FileScope::Global,
                "rereview.md",
                DEFAULT_REREVIEW_PROMPT,
                HASH_REREVIEW_0_5_6,
            ),
            (
                FileScope::Skill,
                "SKILL.md",
                AGENT_SKILL_CONTENT,
                HASH_SKILL_0_6_0,
            ),
            (
                FileScope::Skill,
                "headless-output.md",
                AGENT_SKILL_REF_HEADLESS,
                HASH_SKILL_REF_HEADLESS_0_6_0,
            ),
            (
                FileScope::Skill,
                "config-reference.md",
                AGENT_SKILL_REF_CONFIG,
                HASH_SKILL_REF_CONFIG_0_6_0,
            ),
        ];

        // Print all hashes for debugging when updating defaults
        let mut all_match = true;
        for (scope, filename, content, expected_hash) in &pairs {
            let actual = content_hash(content);
            if &actual != expected_hash {
                eprintln!(
                    "Hash mismatch for {:?}/{}: actual={}, expected={}",
                    scope, filename, actual, expected_hash,
                );
                all_match = false;
            }
        }
        assert!(
            all_match,
            "One or more hashes do not match. See output above for actual values."
        );
    }

    #[test]
    fn test_file_status_local_prompt_up_to_date() {
        // Local prompts share the same content as global — should be recognized as UpToDate
        let status = check_file_status(
            DEFAULT_REVIEWER_PROMPT,
            "0.5.6",
            FileScope::Local,
            "reviewer.md",
        );
        assert_eq!(status, FileStatus::UpToDate);
    }

    // === Step 5: Plan construction ===

    #[test]
    fn test_build_plan_all_up_to_date() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();

        // Write all defaults
        fs::write(config_dir.join("config.toml"), DEFAULT_CONFIG).unwrap();
        fs::write(
            config_dir.join("prompts/reviewer.md"),
            DEFAULT_REVIEWER_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/reviewee.md"),
            DEFAULT_REVIEWEE_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/rereview.md"),
            DEFAULT_REREVIEW_PROMPT,
        )
        .unwrap();

        let manifest = Some(VersionManifest {
            binary_version: "0.5.6".to_string(),
            initialized_at: "2024-01-01T00:00:00Z".to_string(),
            last_migrated_at: None,
            files: HashMap::new(),
        });

        let actions = build_migration_plan(&config_dir, None, &manifest, "0.5.6", false, false);

        // Should only have skips + WriteManifest
        assert!(
            !has_meaningful_actions(&actions),
            "Expected all up to date, got: {:?}",
            actions
        );
    }

    #[test]
    fn test_build_plan_mixed() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();

        // Up to date
        fs::write(config_dir.join("config.toml"), DEFAULT_CONFIG).unwrap();
        // Customized
        fs::write(
            config_dir.join("prompts/reviewer.md"),
            "my custom reviewer prompt",
        )
        .unwrap();
        // Missing reviewee.md and rereview.md

        let actions = build_migration_plan(&config_dir, None, &None, "0.5.6", false, false);

        let has_skip = actions.iter().any(|a| {
            matches!(
                a,
                MigrationAction::SkipCustomized { .. } | MigrationAction::SkipUpToDate { .. }
            )
        });
        let has_create = actions
            .iter()
            .any(|a| matches!(a, MigrationAction::CreateNew { .. }));
        assert!(has_skip, "Should skip customized/up-to-date files");
        assert!(has_create, "Should create missing files");
    }

    #[test]
    fn test_build_plan_local_excludes_skill() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join(".octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();

        fs::write(config_dir.join("config.toml"), DEFAULT_LOCAL_CONFIG).unwrap();
        fs::write(
            config_dir.join("prompts/reviewer.md"),
            DEFAULT_REVIEWER_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/reviewee.md"),
            DEFAULT_REVIEWEE_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/rereview.md"),
            DEFAULT_REREVIEW_PROMPT,
        )
        .unwrap();

        let skill_dir = temp_dir.path().join(".claude");
        fs::create_dir_all(skill_dir.join("skills/octorus")).unwrap();

        // Local mode should NOT include SKILL.md
        let actions = build_migration_plan(
            &config_dir,
            Some(&skill_dir),
            &None,
            "0.5.6",
            true, // is_local
            false,
        );

        let has_skill = actions.iter().any(|a| {
            let path = match a {
                MigrationAction::ReplaceDefault { path, .. }
                | MigrationAction::CreateNew { path, .. }
                | MigrationAction::SkipCustomized { path, .. }
                | MigrationAction::SkipUpToDate { path, .. } => path,
                _ => return false,
            };
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            is_skill_file(&filename)
        });
        assert!(!has_skill, "Local mode should not include any skill files");
    }

    // === Step 6: Backup & Execution ===

    #[test]
    fn test_create_backup() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();
        fs::write(config_dir.join("config.toml"), "test config").unwrap();
        fs::write(config_dir.join("prompts/reviewer.md"), "test prompt").unwrap();

        let backup_dir = create_backup(&config_dir, None).unwrap();
        assert!(backup_dir.exists());
        assert!(backup_dir.join("config.toml").exists());
        assert!(backup_dir.join("prompts/reviewer.md").exists());

        let content = fs::read_to_string(backup_dir.join("config.toml")).unwrap();
        assert_eq!(content, "test config");
    }

    #[test]
    fn test_create_backup_includes_skill() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();
        fs::write(config_dir.join("config.toml"), "test config").unwrap();

        let skill_dir = temp_dir.path().join(".claude");
        fs::create_dir_all(skill_dir.join("skills/octorus/references")).unwrap();
        fs::write(
            skill_dir.join("skills/octorus/SKILL.md"),
            "test skill content",
        )
        .unwrap();
        fs::write(
            skill_dir.join("skills/octorus/references/headless-output.md"),
            "test headless",
        )
        .unwrap();
        fs::write(
            skill_dir.join("skills/octorus/references/config-reference.md"),
            "test config ref",
        )
        .unwrap();

        let backup_dir = create_backup(&config_dir, Some(&skill_dir)).unwrap();
        assert!(backup_dir.exists());
        assert!(backup_dir.join("config.toml").exists());
        assert!(backup_dir.join("skills/octorus/SKILL.md").exists());
        assert!(backup_dir
            .join("skills/octorus/references/headless-output.md")
            .exists());
        assert!(backup_dir
            .join("skills/octorus/references/config-reference.md")
            .exists());

        let content = fs::read_to_string(backup_dir.join("skills/octorus/SKILL.md")).unwrap();
        assert_eq!(content, "test skill content");

        let headless =
            fs::read_to_string(backup_dir.join("skills/octorus/references/headless-output.md"))
                .unwrap();
        assert_eq!(headless, "test headless");

        let config_ref =
            fs::read_to_string(backup_dir.join("skills/octorus/references/config-reference.md"))
                .unwrap();
        assert_eq!(config_ref, "test config ref");
    }

    #[test]
    fn test_execute_replace_default() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.md");
        fs::write(&file_path, "old content").unwrap();

        let actions = vec![MigrationAction::ReplaceDefault {
            path: file_path.clone(),
            content: "new content".to_string(),
            description: "test".to_string(),
        }];

        let mut manifest = bootstrap_manifest("0.5.6");
        execute_plan(&actions, &mut manifest, "0.5.6", false).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "new content");
        assert_eq!(manifest.files["test.md"].status, FileRecordStatus::Migrated);
    }

    #[test]
    fn test_execute_skip_customized() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("custom.md");
        fs::write(&file_path, "my custom content").unwrap();

        let actions = vec![MigrationAction::SkipCustomized {
            path: file_path.clone(),
            reason: "customized".to_string(),
        }];

        let mut manifest = bootstrap_manifest("0.5.6");
        execute_plan(&actions, &mut manifest, "0.5.6", false).unwrap();

        // File should be unchanged
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "my custom content");
        assert_eq!(
            manifest.files["custom.md"].status,
            FileRecordStatus::CustomizedSkipped
        );
    }

    #[test]
    fn test_execute_skip_up_to_date() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("uptodate.md");
        fs::write(&file_path, "default content").unwrap();

        let actions = vec![MigrationAction::SkipUpToDate {
            path: file_path.clone(),
            reason: "uptodate.md is already up to date".to_string(),
        }];

        let mut manifest = bootstrap_manifest("0.5.6");
        execute_plan(&actions, &mut manifest, "0.5.6", false).unwrap();

        // File should be unchanged
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "default content");
        // Status should be Migrated, NOT CustomizedSkipped
        assert_eq!(
            manifest.files["uptodate.md"].status,
            FileRecordStatus::Migrated
        );
    }

    // === Step 7: Integration ===

    #[test]
    fn test_full_migrate_e2e() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();

        // Simulate `or init` — write defaults
        fs::write(config_dir.join("config.toml"), DEFAULT_CONFIG).unwrap();
        fs::write(
            config_dir.join("prompts/reviewer.md"),
            DEFAULT_REVIEWER_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/reviewee.md"),
            DEFAULT_REVIEWEE_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/rereview.md"),
            DEFAULT_REREVIEW_PROMPT,
        )
        .unwrap();

        let version = "0.5.6";

        // First migration — should be "all up to date" but write manifest
        let actions = build_migration_plan(&config_dir, None, &None, version, false, false);
        assert!(!has_meaningful_actions(&actions));

        // Write manifest for tracking
        let mut manifest = bootstrap_manifest(version);
        execute_plan(&actions, &mut manifest, version, false).unwrap();

        // Manifest should exist
        let manifest_path = config_dir.join(".version");
        assert!(manifest_path.exists());
    }

    #[test]
    fn test_migrate_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();

        fs::write(config_dir.join("config.toml"), DEFAULT_CONFIG).unwrap();
        fs::write(
            config_dir.join("prompts/reviewer.md"),
            DEFAULT_REVIEWER_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/reviewee.md"),
            DEFAULT_REVIEWEE_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/rereview.md"),
            DEFAULT_REREVIEW_PROMPT,
        )
        .unwrap();

        let version = "0.5.6";
        let manifest_path = config_dir.join(".version");

        // Run 1
        let actions1 = build_migration_plan(&config_dir, None, &None, version, false, false);
        let mut m1 = bootstrap_manifest(version);
        execute_plan(&actions1, &mut m1, version, false).unwrap();

        // Run 2
        let manifest2 = read_manifest(&manifest_path);
        let actions2 = build_migration_plan(&config_dir, None, &manifest2, version, false, false);
        assert!(!has_meaningful_actions(&actions2));
    }

    #[test]
    fn test_migrate_dry_run_no_changes() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();

        // Missing files — would normally be created
        fs::write(config_dir.join("config.toml"), DEFAULT_CONFIG).unwrap();

        let actions = build_migration_plan(&config_dir, None, &None, "0.5.6", false, false);

        // Has meaningful actions (creating missing prompt files)
        assert!(has_meaningful_actions(&actions));

        // But dry_run shouldn't create them
        display_plan(&actions);

        // Verify no new files were created
        assert!(!config_dir.join("prompts/reviewer.md").exists());
    }

    #[test]
    fn test_newer_version_error() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(&config_dir).unwrap();

        // Write a manifest claiming a newer version than the binary
        let manifest = VersionManifest {
            binary_version: "99.0.0".to_string(),
            initialized_at: "2024-01-01T00:00:00Z".to_string(),
            last_migrated_at: None,
            files: HashMap::new(),
        };
        write_manifest(&config_dir.join(".version"), &manifest).unwrap();

        // run_migrate_in should error because manifest version > binary version
        let result = run_migrate_in(&config_dir, None, false, false, false, "0.5.6");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("newer version"),
            "Expected 'newer version' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_per_file_tracking() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("octorus");
        fs::create_dir_all(config_dir.join("prompts")).unwrap();

        // Default config — up to date
        fs::write(config_dir.join("config.toml"), DEFAULT_CONFIG).unwrap();
        // Customized reviewer — should be skipped
        fs::write(config_dir.join("prompts/reviewer.md"), "custom reviewer").unwrap();
        // Default reviewee
        fs::write(
            config_dir.join("prompts/reviewee.md"),
            DEFAULT_REVIEWEE_PROMPT,
        )
        .unwrap();
        fs::write(
            config_dir.join("prompts/rereview.md"),
            DEFAULT_REREVIEW_PROMPT,
        )
        .unwrap();

        let version = "0.5.6";
        let actions = build_migration_plan(&config_dir, None, &None, version, false, false);

        let mut manifest = bootstrap_manifest(version);
        execute_plan(&actions, &mut manifest, version, false).unwrap();

        // reviewer.md should be CustomizedSkipped
        assert_eq!(
            manifest.files.get("reviewer.md").unwrap().status,
            FileRecordStatus::CustomizedSkipped,
        );
    }

    #[test]
    fn test_detect_version_from_hash_skill_files() {
        // SKILL.md should map to FileScope::Skill
        let version = detect_version_from_hash(AGENT_SKILL_CONTENT, "SKILL.md", false);
        assert!(version.is_some(), "SKILL.md should be detected");

        // Reference files should also map to FileScope::Skill
        let version =
            detect_version_from_hash(AGENT_SKILL_REF_HEADLESS, "headless-output.md", false);
        assert!(version.is_some(), "headless-output.md should be detected");

        let version =
            detect_version_from_hash(AGENT_SKILL_REF_CONFIG, "config-reference.md", false);
        assert!(version.is_some(), "config-reference.md should be detected");

        // Non-skill file should not match as Skill scope
        let version = detect_version_from_hash("random content", "headless-output.md", false);
        assert!(version.is_none(), "random content should not match");
    }

    #[test]
    fn test_is_skill_file() {
        assert!(is_skill_file("SKILL.md"));
        assert!(is_skill_file("headless-output.md"));
        assert!(is_skill_file("config-reference.md"));

        assert!(!is_skill_file("config.toml"));
        assert!(!is_skill_file("reviewer.md"));
        assert!(!is_skill_file("reviewee.md"));
        assert!(!is_skill_file("rereview.md"));
        assert!(!is_skill_file("random.md"));
    }
}
