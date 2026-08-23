//! High-level "install mod into profile" workflow.
//!
//! Resolves a dependency's semver constraint to a concrete published version,
//! picks the platform/arch-specific download target, then drives the existing
//! `mod_download_service` + `profile_service` plumbing for each mod. On any
//! failure the partial install is rolled back so the profile manifest reflects
//! what's actually on disk.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use semver::{Version, VersionReq};

use crate::backend::api::{
    self, DEFAULT_API_BASE_URL, ModDependency, ModVersion, ModVersionInfo, PlatformDownload,
};
use crate::backend::error::{AppError, AppResult};
use crate::backend::services::{
    core_service::{self, GamePlatform},
    mod_download_service, profile_service,
};

#[derive(Debug, Clone)]
pub struct ResolvedDependency {
    pub mod_id: String,
    pub mod_name: String,
    pub resolved_version: String,
    pub dependency_type: String,
    pub version_constraint: String,
}

#[derive(Debug, Clone)]
pub struct InstallModInput {
    pub mod_id: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct InstalledModResult {
    pub mod_id: String,
    pub file_name: String,
}

struct DownloadTarget {
    url: String,
    file_name: String,
    checksum: Option<String>,
}

/// Pick the newest published version whose semver satisfies `constraint`.
/// Falls back to the newest version if the constraint can't be parsed.
pub fn resolve_version(
    constraint: &str,
    versions_sorted_newest_first: &[ModVersion],
) -> Option<String> {
    if versions_sorted_newest_first.is_empty() {
        return None;
    }
    if constraint == "*" {
        return Some(versions_sorted_newest_first[0].version.clone());
    }
    if let Ok(req) = VersionReq::parse(constraint) {
        for item in versions_sorted_newest_first {
            if let Ok(version) = Version::parse(&item.version)
                && req.matches(&version)
            {
                return Some(item.version.clone());
            }
        }
    }
    Some(versions_sorted_newest_first[0].version.clone())
}

/// Resolve `dependencies` transitively. Returns the flattened, deduplicated
/// list ordered deepest-first so callers can install in iteration order. A dep
/// already in `skip` (e.g. the root mod the user clicked Install on) is not
/// emitted but its sub-tree is still walked. First resolution of a mod_id
/// wins on cycles or diamond dependencies.
pub fn resolve_dependencies(
    dependencies: &[ModDependency],
) -> AppResult<(Vec<ResolvedDependency>, Vec<String>)> {
    resolve_dependencies_excluding(dependencies, &HashSet::new())
}

pub fn resolve_dependencies_excluding(
    dependencies: &[ModDependency],
    skip: &HashSet<String>,
) -> AppResult<(Vec<ResolvedDependency>, Vec<String>)> {
    resolve_dependencies_inner(dependencies, skip, true, None)
}

/// Resolve only required dependency branches. Used by one-click updates,
/// where optional dependencies cannot be presented for an explicit choice.
#[cfg(test)]
fn resolve_required_dependencies_excluding(
    dependencies: &[ModDependency],
    skip: &HashSet<String>,
) -> AppResult<(Vec<ResolvedDependency>, Vec<String>)> {
    resolve_dependencies_inner(dependencies, skip, false, None)
}

/// Resolve required dependency branches while treating `pinned_versions` as
/// roots that the caller will install itself. Unlike a plain exclusion set,
/// every dependency edge targeting a pinned root is validated against that
/// root's selected version before the walk skips it.
pub fn resolve_required_dependencies_with_pins(
    dependencies: &[ModDependency],
    pinned_versions: &HashMap<String, String>,
) -> AppResult<(Vec<ResolvedDependency>, Vec<String>)> {
    resolve_dependencies_inner(dependencies, &HashSet::new(), false, Some(pinned_versions))
}

fn resolve_dependencies_inner(
    dependencies: &[ModDependency],
    skip: &HashSet<String>,
    include_optional: bool,
    pinned_versions: Option<&HashMap<String, String>>,
) -> AppResult<(Vec<ResolvedDependency>, Vec<String>)> {
    let mut out = Vec::new();
    let mut unresolved = Vec::new();
    let mut visited: HashSet<String> = skip.clone();
    for dep in dependencies {
        walk_dep(
            dep,
            &mut visited,
            &mut out,
            &mut unresolved,
            include_optional,
            pinned_versions,
        );
    }
    Ok((out, unresolved))
}

fn walk_dep(
    dep: &ModDependency,
    visited: &mut HashSet<String>,
    out: &mut Vec<ResolvedDependency>,
    unresolved: &mut Vec<String>,
    include_optional: bool,
    pinned_versions: Option<&HashMap<String, String>>,
) {
    if !include_optional && dep.dependency_type.eq_ignore_ascii_case("optional") {
        return;
    }
    if let Some(pinned_version) = pinned_versions.and_then(|pins| pins.get(&dep.mod_id)) {
        if !version_satisfies_constraint(pinned_version, &dep.version_constraint) {
            unresolved.push(format!(
                "{} {} does not satisfy {}",
                dep.mod_id, pinned_version, dep.version_constraint
            ));
        }
        return;
    }
    if !visited.insert(dep.mod_id.clone()) {
        return;
    }
    let mod_item = match api::fetch_mod(&dep.mod_id) {
        Ok(mod_item) => mod_item,
        Err(e) => {
            log::warn!("Failed to fetch mod '{}': {e}", dep.mod_id);
            unresolved.push(dep.mod_id.clone());
            return;
        }
    };
    let mut versions = match api::fetch_mod_versions(&dep.mod_id) {
        Ok(versions) => versions,
        Err(e) => {
            log::warn!("Failed to fetch versions for '{}': {e}", dep.mod_id);
            unresolved.push(dep.mod_id.clone());
            return;
        }
    };
    versions.sort_by_key(|version| std::cmp::Reverse(version.created_at));
    let Some(version) = resolve_version(&dep.version_constraint, &versions) else {
        log::warn!(
            "No version of '{}' satisfies constraint '{}'",
            dep.mod_id,
            dep.version_constraint
        );
        unresolved.push(dep.mod_id.clone());
        return;
    };

    // Recurse into this dep's own dependencies first so they install before it.
    if let Ok(info) = api::fetch_mod_version_info(&dep.mod_id, &version) {
        for sub in &info.dependencies {
            walk_dep(
                sub,
                visited,
                out,
                unresolved,
                include_optional,
                pinned_versions,
            );
        }
    }

    out.push(ResolvedDependency {
        mod_id: dep.mod_id.clone(),
        mod_name: mod_item.name,
        resolved_version: version,
        dependency_type: dep.dependency_type.clone(),
        version_constraint: dep.version_constraint.clone(),
    });
}

fn version_satisfies_constraint(version: &str, constraint: &str) -> bool {
    if constraint == "*" {
        return true;
    }
    match (
        Version::parse(version.trim_start_matches('v')),
        VersionReq::parse(constraint),
    ) {
        (Ok(version), Ok(requirement)) => requirement.matches(&version),
        // Preserve the resolver's existing fallback for legacy constraints it
        // cannot parse instead of rejecting previously installable metadata.
        (_, Err(_)) => true,
        (Err(_), Ok(_)) => false,
    }
}

/// Expand the mods a lobby requires into a concrete install list, including
/// transitive dependencies and ordered deepest-first so dependencies install
/// before dependents. Mods (or dependencies) that can't be resolved against the
/// catalog are skipped and returned as the second value, so a single unknown
/// mod doesn't block installing the rest. Does not filter already-installed
/// versions — that's the caller's job, since it needs the target profile.
///
/// A mod's lobby-pinned version always wins over whatever version a *different*
/// required mod's dependency walk resolves for it — versions are seeded from
/// `required` up front, before any dependency is walked, so a diamond
/// dependency can't silently override an exact lobby-pinned version.
pub fn plan_lobby_mods(required: &[InstallModInput]) -> (Vec<InstallModInput>, Vec<String>) {
    let mut versions: HashMap<String, String> = required
        .iter()
        .map(|req| (req.mod_id.clone(), req.version.clone()))
        .collect();
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut unresolved: Vec<String> = Vec::new();

    for req in required {
        // Confirm the exact mod+version exists and pull its dependency tree.
        let Ok(info) = api::fetch_mod_version_info(&req.mod_id, &req.version) else {
            unresolved.push(req.mod_id.clone());
            continue;
        };
        if let Ok((deps, nested_unresolved)) = resolve_dependencies(&info.dependencies) {
            for dep in deps {
                // `or_insert` never overwrites a lobby-pinned version already
                // seeded above; first resolution wins for pure transitive deps.
                versions
                    .entry(dep.mod_id.clone())
                    .or_insert(dep.resolved_version);
                if seen.insert(dep.mod_id.clone()) {
                    order.push(dep.mod_id);
                }
            }
            for mod_id in nested_unresolved {
                if !unresolved.contains(&mod_id) {
                    unresolved.push(mod_id);
                }
            }
        }
        if seen.insert(req.mod_id.clone()) {
            order.push(req.mod_id.clone());
        }
    }

    let out = order
        .into_iter()
        .filter_map(|mod_id| {
            let version = versions.get(&mod_id)?.clone();
            Some(InstallModInput { mod_id, version })
        })
        .collect();
    (out, unresolved)
}

fn absolute_url(path_or_url: &str) -> String {
    if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        return path_or_url.to_string();
    }
    let trimmed_base = DEFAULT_API_BASE_URL.trim_end_matches('/');
    let trimmed_path = path_or_url.trim_start_matches('/');
    format!("{trimmed_base}/{trimmed_path}")
}

fn pick_platform_target(
    platforms: &[PlatformDownload],
    fallback_file_name: Option<&str>,
    fallback_checksum: Option<&str>,
    game_platform: &GamePlatform,
    mod_id: &str,
    version: &str,
) -> Option<DownloadTarget> {
    let arch_fallbacks: &[&str] = match game_platform {
        GamePlatform::Epic => &["x64", "x86"],
        _ => &["x86"],
    };
    let preferred = arch_fallbacks.iter().find_map(|arch| {
        platforms
            .iter()
            .find(|e| e.platform == "windows" && e.architecture == *arch)
            .map(|e| (e, *arch))
    });
    // Fall back to whatever the API offers if nothing matched our preferred arches.
    let (entry, arch): (&PlatformDownload, &str) = match preferred {
        Some(p) => p,
        None => {
            let first = platforms.first()?;
            (first, first.architecture.as_str())
        }
    };
    let url = entry.download_url.clone().unwrap_or_else(|| {
        format!(
            "/api/v3/mods/{mod_id}/versions/{version}/file?platform={}&arch={arch}",
            entry.platform
        )
    });
    let file_name = entry
        .file_name
        .clone()
        .or_else(|| fallback_file_name.map(str::to_string))?;
    Some(DownloadTarget {
        url: absolute_url(&url),
        file_name,
        checksum: entry
            .checksum
            .clone()
            .or_else(|| fallback_checksum.map(str::to_string)),
    })
}

fn resolve_download_target(
    mod_id: &str,
    version: &str,
    version_info: &ModVersionInfo,
    game_platform: &GamePlatform,
) -> AppResult<DownloadTarget> {
    if let Some(platforms) = version_info.platforms.as_ref().filter(|p| !p.is_empty())
        && let Some(target) = pick_platform_target(
            platforms,
            version_info.file_name.as_deref(),
            version_info.checksum.as_deref(),
            game_platform,
            mod_id,
            version,
        )
    {
        return Ok(target);
    }

    let file_name = version_info.file_name.clone().ok_or_else(|| {
        AppError::validation(format!(
            "Mod '{mod_id}' version '{version}' has no downloadable file_name"
        ))
    })?;
    let url = version_info
        .download_url
        .clone()
        .unwrap_or_else(|| format!("/api/v3/mods/{mod_id}/versions/{version}/file"));
    Ok(DownloadTarget {
        url: absolute_url(&url),
        file_name,
        checksum: version_info.checksum.clone(),
    })
}

/// Download each mod into the profile's `BepInEx/plugins/` directory and
/// register it in the profile manifest. Returns the list of installed mods
/// (in input order). Rolls back on failure.
pub fn install_mods_for_profile(
    profile_id: &str,
    mods: &[InstallModInput],
) -> AppResult<Vec<InstalledModResult>> {
    let settings = core_service::get_settings()?;
    let game_platform = settings.game_platform;

    let profile = profile_service::get_profile_by_id(profile_id)?
        .ok_or_else(|| AppError::validation(format!("Profile '{profile_id}' not found")))?;
    let profile_path = profile.path.clone();

    // Updating a disabled mod would currently replace its manifest entry with
    // an enabled one. Require the user to opt in by enabling it first, and
    // enforce that rule here so non-UI install paths cannot bypass it.
    if let Some(item) = mods.iter().find(|item| {
        profile
            .mods
            .iter()
            .any(|installed| installed.mod_id == item.mod_id && !installed.enabled)
    }) {
        return Err(AppError::validation(format!(
            "Mod '{}' is disabled; enable it before updating",
            item.mod_id
        )));
    }

    // Snapshot prior entries so we can restore the manifest on rollback.
    let mut previous: HashMap<String, Option<(String, Option<String>)>> = HashMap::new();
    for item in mods {
        let prior = profile
            .mods
            .iter()
            .find(|m| m.mod_id == item.mod_id)
            .map(|m| (m.version.clone(), m.file.clone()));
        previous.insert(item.mod_id.clone(), prior);
    }

    let plugins_dir = PathBuf::from(&profile_path).join("BepInEx").join("plugins");
    std::fs::create_dir_all(&plugins_dir)?;

    let mut downloaded: Vec<InstalledModResult> = Vec::new();
    let mut persisted: Vec<InstalledModResult> = Vec::new();
    // Old files replaced by upgrades. Deleted only after every mod has
    // installed — deleting inside the loop would leave a later rollback
    // restoring a manifest entry whose DLL is already gone.
    let mut replaced_files: Vec<String> = Vec::new();

    for item in mods {
        let info = api::fetch_mod_version_info(&item.mod_id, &item.version)?;
        let target =
            match resolve_download_target(&item.mod_id, &item.version, &info, &game_platform) {
                Ok(t) => t,
                Err(e) => {
                    rollback(
                        &profile_path,
                        profile_id,
                        &downloaded,
                        &persisted,
                        &previous,
                    );
                    return Err(e);
                }
            };

        let destination = plugins_dir.join(&target.file_name);
        if let Err(e) = mod_download_service::download_mod(
            item.mod_id.clone(),
            target.url,
            destination.to_string_lossy().into_owned(),
            target.checksum.clone(),
        ) {
            rollback(
                &profile_path,
                profile_id,
                &downloaded,
                &persisted,
                &previous,
            );
            return Err(e);
        }

        downloaded.push(InstalledModResult {
            mod_id: item.mod_id.clone(),
            file_name: target.file_name.clone(),
        });

        if let Err(e) = profile_service::add_mod_to_profile(
            profile_id,
            &item.mod_id,
            &item.version,
            &target.file_name,
        ) {
            rollback(
                &profile_path,
                profile_id,
                &downloaded,
                &persisted,
                &previous,
            );
            return Err(e);
        }
        persisted.push(InstalledModResult {
            mod_id: item.mod_id.clone(),
            file_name: target.file_name.clone(),
        });

        if let Some(Some((_version, Some(old_file)))) = previous.get(&item.mod_id)
            && old_file != &target.file_name
        {
            replaced_files.push(old_file.clone());
        }
    }

    for old_file in replaced_files {
        let _ = profile_service::delete_mod_file(&profile_path, &old_file);
    }

    Ok(downloaded)
}

fn rollback(
    profile_path: &str,
    profile_id: &str,
    downloaded: &[InstalledModResult],
    persisted: &[InstalledModResult],
    previous: &HashMap<String, Option<(String, Option<String>)>>,
) {
    for item in persisted.iter().rev() {
        if let Some(prior) = previous.get(&item.mod_id) {
            match prior {
                Some((version, Some(file))) => {
                    let _ = profile_service::add_mod_to_profile(
                        profile_id,
                        &item.mod_id,
                        version,
                        file,
                    );
                }
                _ => {
                    let _ = profile_service::remove_mod_from_profile(profile_id, &item.mod_id);
                }
            }
        }
    }
    for item in downloaded.iter().rev() {
        let _ = profile_service::delete_mod_file(profile_path, &item.file_name);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use crate::backend::api::ModDependency;

    use super::{resolve_required_dependencies_excluding, resolve_required_dependencies_with_pins};

    #[test]
    fn required_update_plan_skips_optional_and_explicitly_excluded_mods() {
        let dependencies = vec![
            ModDependency {
                mod_id: "optional-mod".into(),
                name: "Optional".into(),
                version_constraint: "*".into(),
                dependency_type: "optional".into(),
            },
            ModDependency {
                mod_id: "root-mod".into(),
                name: "Root".into(),
                version_constraint: "*".into(),
                dependency_type: "required".into(),
            },
        ];
        let skip = HashSet::from(["root-mod".to_string()]);

        let (resolved, unresolved) =
            resolve_required_dependencies_excluding(&dependencies, &skip).unwrap();

        assert!(resolved.is_empty());
        assert!(unresolved.is_empty());
    }

    #[test]
    fn pinned_batch_root_must_satisfy_incoming_constraint() {
        let dependency = ModDependency {
            mod_id: "shared-root".into(),
            name: "Shared Root".into(),
            version_constraint: "^1.0".into(),
            dependency_type: "required".into(),
        };

        let incompatible = HashMap::from([("shared-root".into(), "2.0.0".into())]);
        let (_, unresolved) = resolve_required_dependencies_with_pins(
            std::slice::from_ref(&dependency),
            &incompatible,
        )
        .unwrap();
        assert_eq!(unresolved.len(), 1);

        let compatible = HashMap::from([("shared-root".into(), "1.4.0".into())]);
        let (resolved, unresolved) =
            resolve_required_dependencies_with_pins(&[dependency], &compatible).unwrap();
        assert!(resolved.is_empty());
        assert!(unresolved.is_empty());
    }
}
