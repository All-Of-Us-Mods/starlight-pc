//! Self-update check against GitHub Releases, Zed-style: check the latest
//! release tag against the running version, and if newer, download the
//! Windows exe and swap it in for the next launch.
//!
//! Windows-only for now — swapping the running executable relies on the
//! quirk that Windows allows renaming (but not overwriting) an in-use file.

use crate::backend::error::{AppError, AppResult};
use log::info;
use serde::Deserialize;
use std::time::Duration;

const RELEASES_API_URL: &str =
    "https://api.github.com/repos/All-Of-Us-Mods/Starlight-PC/releases/latest";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const RELEASE_DOWNLOAD_PREFIX: &str =
    "https://github.com/All-Of-Us-Mods/Starlight-PC/releases/download/";

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub download_url: String,
    pub expected_sha256: Option<String>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

/// Parse a GitHub asset digest of the form "sha256:<64 hex chars>" into
/// the lowercase hex hash. Returns None for any other shape.
fn parse_sha256_digest(digest: &str) -> Option<String> {
    if digest.len() < 7 || !digest[..7].eq_ignore_ascii_case("sha256:") {
        return None;
    }
    let hex = &digest[7..];
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(hex.to_lowercase())
    } else {
        None
    }
}

/// Check the latest GitHub release against the running version. Returns
/// `Ok(None)` if we're already up to date or the release has no Windows
/// asset to offer.
pub fn check_for_update() -> AppResult<Option<UpdateInfo>> {
    info!("checking for updates against {RELEASES_API_URL}");

    let client = crate::backend::services::http_download::http_client(
        REQUEST_TIMEOUT,
        REQUEST_TIMEOUT,
    )?;

    let release: GithubRelease = client
        .get(RELEASES_API_URL)
        .header("User-Agent", "Starlight-Updater")
        .send()?
        .error_for_status()?
        .json()?;

    let tag = release.tag_name.trim_start_matches('v');
    let latest = semver::Version::parse(tag)
        .map_err(|e| AppError::parse(format!("Invalid release tag '{tag}': {e}")))?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver");

    if latest <= current {
        info!("up to date (running {current}, latest release is {latest})");
        return Ok(None);
    }

    let Some(asset) = release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case("Starlight-windows-x86_64.exe"))
    else {
        info!("release {latest} has no Windows asset, skipping");
        return Ok(None);
    };

    if !asset
        .browser_download_url
        .starts_with(RELEASE_DOWNLOAD_PREFIX)
    {
        return Err(AppError::validation(format!(
            "Unexpected update download URL: {}",
            asset.browser_download_url
        )));
    }

    info!("update available: {current} -> {latest}");

    Ok(Some(UpdateInfo {
        version: latest.to_string(),
        download_url: asset.browser_download_url.clone(),
        expected_sha256: asset.digest.as_deref().and_then(parse_sha256_digest),
    }))
}

/// Download the new exe and swap it in place of the running one, then
/// launch it. Windows allows renaming an open/running executable (just not
/// overwriting its contents in place), so: rename the running exe aside,
/// move the downloaded exe into its place, then spawn it. The caller is
/// responsible for quitting the current process afterwards.
#[cfg(windows)]
pub fn apply_update_and_relaunch(info: &UpdateInfo) -> AppResult<()> {
    use crate::backend::services::http_download;
    use std::fs;
    use std::process::Command;

    let current_exe = std::env::current_exe()?;
    let download_path = current_exe.with_extension("download.exe");
    let old_path = current_exe.with_extension("old.exe");

    info!(
        "downloading update {} from {}",
        info.version, info.download_url
    );
    http_download::download_file(&info.download_url, &download_path, None, None, |_, _| {})?;

    let Some(expected) = info.expected_sha256.as_deref() else {
        let _ = fs::remove_file(&download_path);
        return Err(AppError::validation(
            "Release asset has no sha256 digest; refusing to install update",
        ));
    };

    let computed = hash_file_sha256(&download_path)?;
    if computed != expected {
        let _ = fs::remove_file(&download_path);
        return Err(AppError::validation(format!(
            "Update checksum mismatch: expected {expected}, got {computed}"
        )));
    }

    let _ = fs::remove_file(&old_path);
    fs::rename(&current_exe, &old_path)?;
    if let Err(e) = fs::rename(&download_path, &current_exe) {
        // Best-effort rollback so the user isn't left without an exe.
        let _ = fs::rename(&old_path, &current_exe);
        return Err(e.into());
    }

    info!("update {} installed, relaunching", info.version);
    Command::new(&current_exe).spawn()?;

    Ok(())
}

/// Delete a leftover `.old.exe` from a previous update. The rename in
/// [`apply_update_and_relaunch`] leaves the old exe locked until the process
/// that was running it exits, so cleanup happens on the next launch instead.
#[cfg(windows)]
pub fn cleanup_leftover_old_exe() {
    if let Ok(current_exe) = std::env::current_exe() {
        let old_path = current_exe.with_extension("old.exe");
        if std::fs::remove_file(&old_path).is_ok() {
            info!("removed leftover {}", old_path.display());
        }
    }
}

#[cfg(windows)]
fn hash_file_sha256(path: &std::path::Path) -> AppResult<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    Ok(crate::backend::services::hex_digest(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sha256_digest_accepts_valid_digest() {
        let hex = "a".repeat(64);
        assert_eq!(
            parse_sha256_digest(&format!("sha256:{hex}")),
            Some(hex.clone())
        );
    }

    #[test]
    fn parse_sha256_digest_lowercases_uppercase_hex() {
        let upper = "A".repeat(64);
        let lower = "a".repeat(64);
        assert_eq!(parse_sha256_digest(&format!("sha256:{upper}")), Some(lower));
    }

    #[test]
    fn parse_sha256_digest_rejects_wrong_algorithm_prefix() {
        assert_eq!(
            parse_sha256_digest(&format!("sha512:{}", "a".repeat(64))),
            None
        );
    }

    #[test]
    fn parse_sha256_digest_rejects_wrong_length() {
        assert_eq!(
            parse_sha256_digest(&format!("sha256:{}", "a".repeat(63))),
            None
        );
    }

    #[test]
    fn parse_sha256_digest_rejects_non_hex_chars() {
        assert_eq!(
            parse_sha256_digest(&format!("sha256:{}", "z".repeat(64))),
            None
        );
    }

    #[test]
    fn parse_sha256_digest_rejects_empty_string() {
        assert_eq!(parse_sha256_digest(""), None);
    }
}
