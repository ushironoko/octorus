use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "ushironoko/octorus";

/// Parse a semver version string "MAJOR.MINOR.PATCH" into a comparable tuple.
/// Returns `None` if the format is invalid.
fn parse_semver(version: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

/// Returns true if `latest` is strictly newer than `current` by semver comparison.
fn is_newer_version(current: &str, latest: &str) -> bool {
    match (parse_semver(current), parse_semver(latest)) {
        (Some(cur), Some(lat)) => lat > cur,
        _ => false, // Invalid format — don't treat as update
    }
}

/// How the binary was installed, determining the appropriate update strategy.
#[derive(Debug, PartialEq)]
enum InstallMethod {
    /// Installed via `cargo install octorus`
    CargoInstall,
    /// Managed by mise (formerly rtx) with GitHub backend
    Mise,
    /// Downloaded from GitHub Releases (positively identified release layout)
    GitHubRelease,
    /// Unknown installation method — cannot safely update in place
    Unknown,
}

/// Well-known directories where GitHub Release binaries are typically installed.
const KNOWN_RELEASE_DIRS: &[&str] = &["/usr/local/bin", "/opt/homebrew/bin"];

/// Detect the installation method from the executable path.
fn detect_install_method(exe_path: &Path) -> InstallMethod {
    if is_path_under_cargo_bin(exe_path) {
        return InstallMethod::CargoInstall;
    }
    if is_path_under_mise(exe_path) {
        return InstallMethod::Mise;
    }
    if is_known_release_location(exe_path) {
        return InstallMethod::GitHubRelease;
    }
    InstallMethod::Unknown
}

/// Check if the path is in a well-known location where release binaries are installed.
fn is_known_release_location(exe_path: &Path) -> bool {
    if let Some(parent) = exe_path.parent() {
        let parent_canonical = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
        for dir in KNOWN_RELEASE_DIRS {
            let known = Path::new(dir);
            let known_canonical = known.canonicalize().unwrap_or_else(|_| known.to_path_buf());
            if parent_canonical == known_canonical {
                return true;
            }
        }
    }
    false
}

/// Check if the path is inside a cargo bin directory.
/// Detects both `$CARGO_HOME/bin/` and the default `~/.cargo/bin/`.
/// Uses path-component-aware comparison to avoid false positives with similar prefixes.
fn is_path_under_cargo_bin(exe_path: &Path) -> bool {
    let exe_canonical = exe_path
        .canonicalize()
        .unwrap_or_else(|_| exe_path.to_path_buf());

    if let Ok(cargo_home) = std::env::var("CARGO_HOME") {
        let cargo_bin = PathBuf::from(&cargo_home).join("bin");
        if let Ok(cargo_bin) = cargo_bin.canonicalize() {
            if exe_canonical.starts_with(&cargo_bin) {
                return true;
            }
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let default_cargo_bin = PathBuf::from(&home).join(".cargo").join("bin");
        if let Ok(default_cargo_bin) = default_cargo_bin.canonicalize() {
            if exe_canonical.starts_with(&default_cargo_bin) {
                return true;
            }
        }
    }

    false
}

/// Check if the path is inside a mise-managed installs directory.
/// mise stores binaries at `$MISE_DATA_DIR/installs/` or `~/.local/share/mise/installs/`.
/// Uses path-component-aware comparison to avoid false positives with similar prefixes.
fn is_path_under_mise(exe_path: &Path) -> bool {
    let exe_canonical = exe_path
        .canonicalize()
        .unwrap_or_else(|_| exe_path.to_path_buf());

    // Check $MISE_DATA_DIR/installs/
    if let Ok(mise_data) = std::env::var("MISE_DATA_DIR") {
        let mise_installs = PathBuf::from(&mise_data).join("installs");
        if let Ok(mise_installs) = mise_installs.canonicalize() {
            if exe_canonical.starts_with(&mise_installs) {
                return true;
            }
        }
    }

    // Check ~/.local/share/mise/installs/ (default)
    if let Ok(home) = std::env::var("HOME") {
        let default_mise = PathBuf::from(&home)
            .join(".local")
            .join("share")
            .join("mise")
            .join("installs");
        if let Ok(default_mise) = default_mise.canonicalize() {
            if exe_canonical.starts_with(&default_mise) {
                return true;
            }
        }
    }

    false
}

/// Update via `cargo install octorus@{version}`.
fn update_via_cargo(version: &str) -> Result<()> {
    println!("Detected cargo installation. Running cargo install...");
    println!();

    let version_spec = format!("octorus@{}", version);
    let status = Command::new("cargo")
        .args(["install", &version_spec])
        .status()
        .context("Failed to run `cargo install`. Is cargo available?")?;

    if !status.success() {
        bail!(
            "`cargo install {}` failed with exit code: {}",
            version_spec,
            status
        );
    }

    Ok(())
}

/// Detect the target triple for the current platform.
/// Must match the targets in .github/workflows/release.yml.
fn detect_target() -> Result<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Ok("aarch64-apple-darwin");
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Ok("x86_64-apple-darwin");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Ok("x86_64-unknown-linux-gnu");
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Ok("aarch64-unknown-linux-gnu");
    }
    #[allow(unreachable_code)]
    {
        bail!(
            "Unsupported platform (os={}, arch={}). Please build from source or use `cargo install octorus`.",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    }
}

/// Fetch the latest release tag name via `gh api`.
fn get_latest_tag() -> Result<String> {
    let output = Command::new("gh")
        .args([
            "api",
            &format!("repos/{}/releases/latest", REPO),
            "-q",
            ".tag_name",
        ])
        .output()
        .context("Failed to execute gh CLI - is it installed and authenticated?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to fetch latest release: {}", stderr.trim());
    }

    let tag = String::from_utf8(output.stdout)
        .context("Invalid UTF-8 in gh output")?
        .trim()
        .to_string();

    if tag.is_empty() {
        bail!("No releases found for {}", REPO);
    }

    Ok(tag)
}

/// Download a release asset to a directory via `gh release download`.
fn download_asset(tag: &str, pattern: &str, dest_dir: &Path) -> Result<()> {
    let output = Command::new("gh")
        .args([
            "release",
            "download",
            tag,
            "--repo",
            REPO,
            "--pattern",
            pattern,
            "--dir",
            &dest_dir.to_string_lossy(),
        ])
        .output()
        .context("Failed to download release asset")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to download {}: {}", pattern, stderr.trim());
    }

    Ok(())
}

/// Extract a .tar.gz archive in the given directory.
fn extract_archive(dir: &Path, archive_name: &str) -> Result<()> {
    let archive_path = dir.join(archive_name);
    let output = Command::new("tar")
        .args(["xzf", &archive_path.to_string_lossy()])
        .current_dir(dir)
        .output()
        .context("Failed to run tar")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to extract archive: {}", stderr.trim());
    }

    Ok(())
}

/// Verify SHA256 checksum of the downloaded archive.
fn verify_checksum(dir: &Path, archive_name: &str) -> Result<()> {
    let sha256_file = dir.join(format!("{}.sha256", archive_name));
    let expected = fs::read_to_string(&sha256_file)
        .context("Failed to read checksum file")?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();

    if expected.is_empty() {
        bail!("Checksum file is empty or malformed");
    }

    let archive_path = dir.join(archive_name);

    // Try shasum (macOS/Linux) first, then sha256sum (Linux)
    let output = Command::new("shasum")
        .args(["-a", "256", &archive_path.to_string_lossy()])
        .output()
        .or_else(|_| {
            Command::new("sha256sum")
                .arg(&archive_path.to_string_lossy().to_string())
                .output()
        })
        .context("Failed to compute checksum (shasum/sha256sum not found)")?;

    if !output.status.success() {
        bail!("Failed to compute SHA256 checksum");
    }

    let actual = String::from_utf8(output.stdout)
        .context("Invalid checksum output")?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();

    if actual != expected {
        bail!(
            "Checksum mismatch!\n  Expected: {}\n  Actual:   {}",
            expected,
            actual
        );
    }

    Ok(())
}

/// Replace the current binary with a new one.
/// Creates a backup (.old) and restores on failure.
fn replace_binary(new_binary: &Path, current_exe: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(new_binary, perms).context("Failed to set executable permissions")?;
    }

    let backup_path = current_exe.with_extension("old");

    // On Unix, a running binary can be renamed
    fs::rename(current_exe, &backup_path).with_context(|| {
        format!(
            "Failed to backup current binary at {}. You may need elevated permissions.",
            current_exe.display()
        )
    })?;

    match fs::copy(new_binary, current_exe) {
        Ok(_) => {
            // Preserve permissions after copy
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::Permissions::from_mode(0o755);
                let _ = fs::set_permissions(current_exe, perms);
            }
            let _ = fs::remove_file(&backup_path);
            Ok(())
        }
        Err(e) => {
            // Restore backup on failure
            let _ = fs::rename(&backup_path, current_exe);
            Err(e).context("Failed to install new binary. Original binary has been restored.")
        }
    }
}

/// Update via GitHub Releases: download prebuilt binary, verify, and replace.
fn update_via_release(latest_tag: &str, latest_version: &str) -> Result<()> {
    let target = detect_target()?;
    let archive_name = format!("octorus-{}-{}.tar.gz", latest_version, target);

    // Download archive + checksum to temp directory
    let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;
    let temp_path = temp_dir.path();

    println!("Downloading {}...", archive_name);
    download_asset(latest_tag, &archive_name, temp_path)?;
    download_asset(
        latest_tag,
        &format!("{}.sha256", archive_name),
        temp_path,
    )?;

    // Verify checksum
    println!("Verifying checksum...");
    verify_checksum(temp_path, &archive_name)?;

    // Extract
    extract_archive(temp_path, &archive_name)?;

    // Locate the new binary inside the extracted directory
    let extracted_dir = format!("octorus-{}-{}", latest_version, target);
    let new_binary = temp_path.join(&extracted_dir).join("or");

    if !new_binary.exists() {
        bail!(
            "Binary not found in archive at expected path: {}/or",
            extracted_dir
        );
    }

    // Replace current binary
    let current_exe =
        std::env::current_exe().context("Failed to determine current executable path")?;
    let current_exe = current_exe
        .canonicalize()
        .context("Failed to resolve current executable path")?;

    println!("Installing to {}...", current_exe.display());
    replace_binary(&new_binary, &current_exe)?;

    Ok(())
}

/// Check if a newer version is available. Returns `Some(latest_version)` if an
/// update exists, or `None` if already up-to-date (or if the check fails silently).
/// Intended for background startup checks — errors are swallowed.
pub fn check_for_update() -> Option<String> {
    let current_version = env!("CARGO_PKG_VERSION");
    let tag = get_latest_tag().ok()?;
    let latest = tag.strip_prefix('v').unwrap_or(&tag);
    if is_newer_version(current_version, latest) {
        Some(latest.to_string())
    } else {
        None
    }
}

/// Run the update command: check for new version, download, verify, and install.
/// Automatically detects whether the binary was installed via `cargo install`
/// and uses the appropriate update method.
pub fn run_update() -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    println!("Checking for updates...");
    let latest_tag = get_latest_tag()?;
    let latest_version = latest_tag.strip_prefix('v').unwrap_or(&latest_tag);

    if !is_newer_version(current_version, latest_version) {
        println!("Already up to date (v{})", current_version);
        return Ok(());
    }

    println!(
        "Updating: v{} → v{}",
        current_version, latest_version
    );

    let current_exe =
        std::env::current_exe().context("Failed to determine current executable path")?;
    let current_exe = current_exe
        .canonicalize()
        .context("Failed to resolve current executable path")?;

    match detect_install_method(&current_exe) {
        InstallMethod::CargoInstall => {
            update_via_cargo(latest_version)?;
        }
        InstallMethod::Mise => {
            println!("Detected mise-managed installation at:");
            println!("  {}", current_exe.display());
            println!();
            println!("Please update via mise:");
            println!("  mise install octorus@{}", latest_version);
            println!("  mise use octorus@{}", latest_version);
            return Ok(());
        }
        InstallMethod::GitHubRelease => {
            update_via_release(&latest_tag, latest_version)?;
        }
        InstallMethod::Unknown => {
            println!("Could not determine how octorus was installed.");
            println!("  Executable path: {}", current_exe.display());
            println!();
            println!("To avoid corrupting a package-managed installation, self-update");
            println!("is only supported for the following install methods:");
            println!("  - cargo install octorus");
            println!("  - mise install octorus");
            println!("  - GitHub Releases (binary in /usr/local/bin or /opt/homebrew/bin)");
            println!();
            println!("Please update using the same method you used to install octorus,");
            println!("or download the latest release manually:");
            println!(
                "  gh release download {} --repo {}",
                latest_tag, REPO
            );
            return Ok(());
        }
    }

    println!("Successfully updated to v{}", latest_version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_semver_valid() {
        assert_eq!(parse_semver("0.5.6"), Some((0, 5, 6)));
        assert_eq!(parse_semver("1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_semver("12.34.56"), Some((12, 34, 56)));
    }

    #[test]
    fn test_parse_semver_invalid() {
        assert_eq!(parse_semver("0.5"), None);
        assert_eq!(parse_semver("0.5.6.7"), None);
        assert_eq!(parse_semver("abc"), None);
        assert_eq!(parse_semver("0.5.beta"), None);
    }

    #[test]
    fn test_is_newer_version() {
        // Newer
        assert!(is_newer_version("0.5.6", "0.5.7"));
        assert!(is_newer_version("0.5.6", "0.6.0"));
        assert!(is_newer_version("0.5.6", "1.0.0"));

        // Same
        assert!(!is_newer_version("0.5.6", "0.5.6"));

        // Older (downgrade)
        assert!(!is_newer_version("0.5.7", "0.5.6"));
        assert!(!is_newer_version("1.0.0", "0.9.9"));

        // Invalid format — never treat as update
        assert!(!is_newer_version("0.5.6", "invalid"));
        assert!(!is_newer_version("invalid", "0.5.7"));
    }

    #[test]
    fn test_detect_target_returns_valid_triple() {
        let result = detect_target();
        assert!(result.is_ok());
        let target = result.unwrap();
        let valid_targets = [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        ];
        assert!(
            valid_targets.contains(&target),
            "Unexpected target: {}",
            target
        );
    }

    #[test]
    fn test_replace_binary_success() {
        let temp_dir = TempDir::new().unwrap();
        let old_path = temp_dir.path().join("or");
        let new_path = temp_dir.path().join("or_new");

        fs::write(&old_path, b"old binary").unwrap();
        fs::write(&new_path, b"new binary").unwrap();

        replace_binary(&new_path, &old_path).unwrap();

        let content = fs::read(&old_path).unwrap();
        assert_eq!(content, b"new binary");

        // Backup should be cleaned up
        assert!(!temp_dir.path().join("or.old").exists());
    }

    #[test]
    fn test_replace_binary_restores_on_failure() {
        let temp_dir = TempDir::new().unwrap();
        let old_path = temp_dir.path().join("or");
        // Point new_path to a non-existent file to trigger copy failure
        let new_path = temp_dir.path().join("nonexistent");

        fs::write(&old_path, b"old binary").unwrap();

        let result = replace_binary(&new_path, &old_path);
        assert!(result.is_err());

        // Original should be restored
        let content = fs::read(&old_path).unwrap();
        assert_eq!(content, b"old binary");
    }

    #[test]
    fn test_verify_checksum_valid() {
        let temp_dir = TempDir::new().unwrap();
        let archive_name = "test.tar.gz";
        let archive_path = temp_dir.path().join(archive_name);
        let checksum_path = temp_dir.path().join(format!("{}.sha256", archive_name));

        // Write a test file
        fs::write(&archive_path, b"test content").unwrap();

        // Compute actual checksum
        let output = Command::new("shasum")
            .args(["-a", "256", &archive_path.to_string_lossy()])
            .output()
            .or_else(|_| {
                Command::new("sha256sum")
                    .arg(&archive_path.to_string_lossy().to_string())
                    .output()
            })
            .unwrap();

        let checksum_line = String::from_utf8(output.stdout).unwrap();
        fs::write(&checksum_path, &checksum_line).unwrap();

        // Should pass verification
        let result = verify_checksum(temp_dir.path(), archive_name);
        assert!(result.is_ok(), "Checksum verification failed: {:?}", result);
    }

    #[test]
    fn test_detect_cargo_install() {
        if let Ok(home) = std::env::var("HOME") {
            let cargo_bin = PathBuf::from(&home).join(".cargo").join("bin");
            if cargo_bin.exists() {
                let fake_exe = cargo_bin.join("or");
                assert!(is_path_under_cargo_bin(&fake_exe));
                assert_eq!(detect_install_method(&fake_exe), InstallMethod::CargoInstall);
            }
        }
    }

    #[test]
    fn test_detect_mise_install() {
        if let Ok(home) = std::env::var("HOME") {
            let mise_dir = PathBuf::from(&home)
                .join(".local")
                .join("share")
                .join("mise")
                .join("installs");
            if mise_dir.exists() {
                let fake_exe = mise_dir.join("octorus").join("0.5.6").join("bin").join("or");
                assert!(is_path_under_mise(&fake_exe));
                assert_eq!(detect_install_method(&fake_exe), InstallMethod::Mise);
            }
        }
    }

    #[test]
    fn test_detect_github_release_for_known_locations() {
        let path = Path::new("/usr/local/bin/or");
        if path.parent().map_or(false, |p| p.exists()) {
            assert_eq!(
                detect_install_method(path),
                InstallMethod::GitHubRelease
            );
        }
    }

    #[test]
    fn test_detect_unknown_for_unrecognized_paths() {
        // A path that doesn't match any known install location
        let path = Path::new("/some/random/location/or");
        assert_eq!(detect_install_method(path), InstallMethod::Unknown);
    }

    #[test]
    fn test_verify_checksum_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let archive_name = "test.tar.gz";
        let archive_path = temp_dir.path().join(archive_name);
        let checksum_path = temp_dir.path().join(format!("{}.sha256", archive_name));

        fs::write(&archive_path, b"test content").unwrap();
        fs::write(
            &checksum_path,
            "0000000000000000000000000000000000000000000000000000000000000000  test.tar.gz\n",
        )
        .unwrap();

        let result = verify_checksum(temp_dir.path(), archive_name);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Checksum mismatch"),
            "Should report checksum mismatch"
        );
    }
}
