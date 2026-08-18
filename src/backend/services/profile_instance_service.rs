//! Throwaway per-launch copies of a profile, used when the same profile is
//! launched more than once at a time.
//!
//! Among Us + BepInEx (IL2CPP) write shared state inside the profile directory
//! while starting up (interop/cache output, configs, logs), so a second
//! instance started from the same directory mid-warm-up dies. Delaying the
//! second launch until the first had settled used to paper over that; it no
//! longer does. Instead every *extra* concurrent launch of a profile runs from
//! its own copy under `{app_data}/profile-instances`, so instances never touch
//! each other's files and can be started back to back with no delay.
//!
//! Copies are disposable: they're deleted when the instance exits, and any
//! left behind by a crash are swept at startup.

use crate::backend::directories;
use crate::backend::error::AppResult;
use crate::backend::services::profile_service;
use log::{debug, info, warn};
use std::fs;
use std::path::{Path, PathBuf};

const INSTANCES_DIR_NAME: &str = "profile-instances";
/// Separates the profile id from the slot number in a copy's directory name.
/// The name keeps the profile id as a prefix on purpose: the Linux stop path
/// matches running processes by profile id in their cmdline, and the doorstop
/// arguments point at this directory.
const INSTANCE_SUFFIX: &str = "-instance";

/// Profile sub-trees that are only read while the game runs, so a copy can
/// hardlink them instead of duplicating the bytes (`dotnet` alone is ~70 MB).
/// Everything else — BepInEx's interop/cache output, configs — is copied,
/// since that mutable state is exactly what two instances must not share.
const LINKABLE_SUBTREES: [&str; 4] = [
    "dotnet",
    "BepInEx/core",
    "BepInEx/patchers",
    "BepInEx/plugins",
];

/// BepInEx log files. Never copied into an instance (it writes its own), and
/// copied back out to the source profile when the instance is released.
const LOG_FILE_PREFIX: &str = "LogOutput";

fn instances_root() -> AppResult<PathBuf> {
    let dir = directories::app_data_dir()?.join(INSTANCES_DIR_NAME);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn instance_dir(profile_id: &str, slot: usize) -> AppResult<PathBuf> {
    Ok(instances_root()?.join(format!("{profile_id}{INSTANCE_SUFFIX}{slot}")))
}

/// Copy `profile_path` into the directory for `slot`, replacing any stale copy
/// left there, and return the copy's path. The caller launches from it and is
/// responsible for handing it to [`release`] once the instance exits.
pub fn create(profile_id: &str, profile_path: &Path, slot: usize) -> AppResult<PathBuf> {
    let destination = instance_dir(profile_id, slot)?;
    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }

    if let Err(error) = clone_tree(profile_path, &destination, Path::new("")) {
        let _ = fs::remove_dir_all(&destination);
        return Err(error);
    }

    info!(
        "prepared instance copy for profile {profile_id} (slot {slot}) at {}",
        destination.display()
    );
    Ok(destination)
}

/// Delete an instance copy, first saving its BepInEx log next to the source
/// profile as `LogOutput.instance{slot}.log` so the run is still debuggable.
pub fn release(directory: &Path) {
    preserve_log(directory);
    if let Err(error) = fs::remove_dir_all(directory) {
        warn!(
            "failed to remove instance copy {}: {error}",
            directory.display()
        );
    }
}

/// Drop every instance copy on disk. Instance copies only stay valid for the
/// lifetime of the process that launched from them, so anything present at
/// startup is a leftover from a crash.
pub fn cleanup_stale_copies() {
    let Ok(root) = instances_root() else { return };
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            release(&path);
        }
    }
}

fn clone_tree(source: &Path, destination: &Path, relative: &Path) -> AppResult<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)?.flatten() {
        let name = entry.file_name();
        let path = entry.path();
        let child_relative = relative.join(&name);

        if path.is_dir() {
            clone_tree(&path, &destination.join(&name), &child_relative)?;
            continue;
        }

        if name.to_string_lossy().starts_with(LOG_FILE_PREFIX) {
            continue;
        }

        let target = destination.join(&name);
        // Hardlinks are best-effort: they fail on filesystems that don't
        // support them, and then a plain copy is just as correct.
        if is_linkable(&child_relative) && fs::hard_link(&path, &target).is_ok() {
            continue;
        }
        fs::copy(&path, &target)?;
    }
    Ok(())
}

fn is_linkable(relative: &Path) -> bool {
    let relative = relative.to_string_lossy().replace('\\', "/");
    LINKABLE_SUBTREES
        .iter()
        .any(|subtree| relative == *subtree || relative.starts_with(&format!("{subtree}/")))
}

/// Split `{profile_id}-instance{slot}` back into its parts.
fn parse_instance_dir_name(directory: &Path) -> Option<(String, usize)> {
    let name = directory.file_name()?.to_str()?;
    let (profile_id, slot) = name.rsplit_once(INSTANCE_SUFFIX)?;
    Some((profile_id.to_string(), slot.parse().ok()?))
}

fn preserve_log(directory: &Path) {
    let source = directory
        .join("BepInEx")
        .join(format!("{LOG_FILE_PREFIX}.log"));
    if !source.exists() {
        return;
    }
    let Some((profile_id, slot)) = parse_instance_dir_name(directory) else {
        return;
    };
    let Ok(Some(profile)) = profile_service::get_profile_by_id(&profile_id) else {
        return;
    };
    let destination = Path::new(&profile.path)
        .join("BepInEx")
        .join(format!("{LOG_FILE_PREFIX}.instance{slot}.log"));
    if let Err(error) = fs::copy(&source, &destination) {
        debug!("failed to preserve instance log for {profile_id}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linkable_covers_read_only_subtrees_only() {
        assert!(is_linkable(Path::new("dotnet/coreclr.dll")));
        assert!(is_linkable(Path::new("BepInEx/plugins/Mod.dll")));
        assert!(!is_linkable(Path::new(
            "BepInEx/interop/Assembly-CSharp.dll"
        )));
        assert!(!is_linkable(Path::new("BepInEx/config/Mod.cfg")));
        assert!(!is_linkable(Path::new("BepInEx/core-extra/x.dll")));
    }

    #[test]
    fn instance_dir_name_round_trips() {
        let dir = PathBuf::from("/tmp").join("my-profile-1700000000-instance2");
        assert_eq!(
            parse_instance_dir_name(&dir),
            Some(("my-profile-1700000000".to_string(), 2))
        );
        assert_eq!(parse_instance_dir_name(Path::new("/tmp/plain")), None);
    }
}
