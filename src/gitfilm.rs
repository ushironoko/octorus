use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

// Platform-specific embedded binary
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const GITFILM_BINARY: Option<&[u8]> =
    Some(include_bytes!("../vendor/gitfilm-aarch64-apple-darwin"));

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const GITFILM_BINARY: Option<&[u8]> =
    Some(include_bytes!("../vendor/gitfilm-x86_64-apple-darwin"));

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const GITFILM_BINARY: Option<&[u8]> =
    Some(include_bytes!("../vendor/gitfilm-x86_64-unknown-linux-gnu"));

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const GITFILM_BINARY: Option<&[u8]> =
    Some(include_bytes!("../vendor/gitfilm-x86_64-pc-windows-msvc.exe"));

#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "windows", target_arch = "x86_64"),
)))]
const GITFILM_BINARY: Option<&[u8]> = None;

#[cfg(target_os = "windows")]
const GITFILM_FILENAME: &str = "gitfilm.exe";
#[cfg(not(target_os = "windows"))]
const GITFILM_FILENAME: &str = "gitfilm";

/// gitfilm JSON output: 3-area model snapshot
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GitfilmSimOutput {
    pub prev: GitfilmAreaSnapshot,
    pub next: GitfilmAreaSnapshot,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GitfilmAreaSnapshot {
    pub working_tree: Vec<GitfilmFileEntry>,
    pub staging_area: Vec<GitfilmFileEntry>,
    pub repository: GitfilmRepoState,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GitfilmFileEntry {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GitfilmRepoState {
    pub commits: Vec<GitfilmCommit>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GitfilmCommit {
    pub hash: String,
    pub message: String,
}

/// Extract embedded gitfilm binary to cache directory, return path.
pub fn extract_gitfilm() -> Option<PathBuf> {
    let binary = GITFILM_BINARY?;
    let cache_dir = xdg::BaseDirectories::with_prefix("octorus")
        .ok()?
        .get_cache_home();

    std::fs::create_dir_all(&cache_dir).ok()?;
    let dest = cache_dir.join(GITFILM_FILENAME);

    if dest.exists() {
        let existing = std::fs::read(&dest).ok()?;
        let existing_hash = Sha256::digest(&existing);
        let embedded_hash = Sha256::digest(binary);
        if existing_hash == embedded_hash {
            return Some(dest);
        }
    }

    std::fs::write(&dest, binary).ok()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).ok()?;
    }

    Some(dest)
}

/// Run gitfilm simulation as subprocess, return parsed JSON output.
pub async fn simulate(
    gitfilm_path: &Path,
    working_dir: Option<&str>,
    args: &[&str],
) -> Result<GitfilmSimOutput, String> {
    let mut cmd = tokio::process::Command::new(gitfilm_path);
    cmd.arg("--output-json");
    cmd.args(args);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let output = cmd.output().await.map_err(|e| e.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("gitfilm failed (exit {})", output.status)
        } else {
            format!("gitfilm failed: {}", stderr)
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<GitfilmSimOutput>(&stdout)
        .map_err(|e| format!("Failed to parse gitfilm output: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gitfilm_json_valid() {
        let json = r#"{
            "operations": [{"kind": "restore", "args": ["test.txt"]}],
            "prev": {
                "working_tree": [{"path": "test.txt", "status": "modified"}],
                "staging_area": [],
                "repository": {"commits": [{"hash": "abc1234", "message": "init"}]}
            },
            "next": {
                "working_tree": [{"path": "test.txt", "status": "clean"}],
                "staging_area": [],
                "repository": {"commits": [{"hash": "abc1234", "message": "init"}]}
            }
        }"#;

        let result: GitfilmSimOutput = serde_json::from_str(json).unwrap();
        assert_eq!(result.prev.working_tree.len(), 1);
        assert_eq!(result.prev.working_tree[0].path, "test.txt");
        assert_eq!(result.prev.working_tree[0].status, "modified");
        assert_eq!(result.next.working_tree[0].status, "clean");
        assert_eq!(result.prev.repository.commits[0].hash, "abc1234");
    }

    #[test]
    fn test_parse_gitfilm_json_invalid() {
        let json = r#"{"invalid": true}"#;
        let result = serde_json::from_str::<GitfilmSimOutput>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_gitfilm_json_empty_areas() {
        let json = r#"{
            "operations": [],
            "prev": {
                "working_tree": [],
                "staging_area": [],
                "repository": {"commits": []}
            },
            "next": {
                "working_tree": [],
                "staging_area": [],
                "repository": {"commits": []}
            }
        }"#;

        let result: GitfilmSimOutput = serde_json::from_str(json).unwrap();
        assert!(result.prev.working_tree.is_empty());
        assert!(result.next.working_tree.is_empty());
    }

    #[test]
    fn test_parse_gitfilm_json_multiple_files() {
        let json = r#"{
            "operations": [{"kind": "add", "args": ["."]}],
            "prev": {
                "working_tree": [
                    {"path": "src/main.rs", "status": "modified"},
                    {"path": "new.txt", "status": "untracked"}
                ],
                "staging_area": [],
                "repository": {"commits": [{"hash": "def5678", "message": "initial"}]}
            },
            "next": {
                "working_tree": [
                    {"path": "src/main.rs", "status": "clean"},
                    {"path": "new.txt", "status": "clean"}
                ],
                "staging_area": [
                    {"path": "src/main.rs", "status": "staged (modified)"},
                    {"path": "new.txt", "status": "staged (new file)"}
                ],
                "repository": {"commits": [{"hash": "def5678", "message": "initial"}]}
            }
        }"#;

        let result: GitfilmSimOutput = serde_json::from_str(json).unwrap();
        assert_eq!(result.prev.working_tree.len(), 2);
        assert_eq!(result.next.staging_area.len(), 2);
        assert_eq!(result.next.staging_area[1].status, "staged (new file)");
    }
}
