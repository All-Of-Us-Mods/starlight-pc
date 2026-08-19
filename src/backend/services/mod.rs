pub mod bepinex_service;
pub mod core_service;
#[cfg(windows)]
pub mod epic_launch_service;
pub mod finder_service;
pub mod http_download;
pub mod launch_service;
pub mod migration_service;
pub mod mod_download_service;
pub mod mod_install_service;
pub mod profile_instance_service;
pub mod profile_service;
pub mod profile_shortcut_service;
pub mod profile_zip_service;
pub mod region_service;
#[cfg(windows)]
pub mod update_service;
#[cfg(windows)]
pub mod xbox_service;

/// Lowercase hex of a digest. `sha2` 0.11 hands back a `hybrid_array::Array`,
/// which no longer formats with `{:x}`.
pub fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    bytes.as_ref().iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
