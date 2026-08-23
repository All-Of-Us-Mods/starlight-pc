//! Process-wide cache of Starlight catalog mod lookups (`api::fetch_mod`),
//! shared by every view that needs to resolve a mod id to catalog info — a
//! mod looked up once (e.g. opening a profile in the Library) is reused by
//! any other view that needs it (e.g. browsing lobbies) instead of
//! re-fetching.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use log::warn;

use crate::backend::api::{self, ModResponse};

/// `None` means the catalog confirmed (HTTP 404) it has no matching mod —
/// cached too, so callers don't retry forever. A transient failure (network
/// error, timeout, 5xx) is never cached here, so it's retried on the next
/// lookup instead of permanently mislabeling the mod as missing.
static CACHE: LazyLock<Mutex<HashMap<String, Option<ModResponse>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Latest published version per catalog mod. The nested option follows the
/// same convention as `CACHE`: an empty version list is cached, while request
/// failures are not.
static LATEST_VERSION_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The cached result for `mod_id`. Outer `None` means it hasn't been looked
/// up this session; `Some(None)` means it was looked up and not found.
pub fn get(mod_id: &str) -> Option<Option<ModResponse>> {
    CACHE.lock().ok()?.get(mod_id).cloned()
}

/// Display names for every catalog mod resolved so far this session.
pub fn cached_names() -> HashMap<String, String> {
    CACHE
        .lock()
        .map(|cache| {
            cache
                .iter()
                .filter_map(|(id, info)| info.as_ref().map(|m| (id.clone(), m.name.clone())))
                .collect()
        })
        .unwrap_or_default()
}

/// Latest versions resolved so far, omitting catalog mods with no releases.
pub fn cached_latest_versions() -> HashMap<String, String> {
    LATEST_VERSION_CACHE
        .lock()
        .map(|cache| {
            cache
                .iter()
                .filter_map(|(id, version)| {
                    version
                        .as_ref()
                        .map(|version| (id.clone(), version.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the catalog's latest release for `mod_id`, caching successful
/// responses for the app session. The versions endpoint is ordered newest
/// first, matching the version selected by the mod detail page.
pub fn fetch_latest_version(mod_id: &str) -> Option<String> {
    if let Ok(cache) = LATEST_VERSION_CACHE.lock()
        && let Some(cached) = cache.get(mod_id)
    {
        return cached.clone();
    }

    match api::fetch_mod_versions(mod_id) {
        Ok(versions) => {
            let latest = versions.first().map(|version| version.version.clone());
            if let Ok(mut cache) = LATEST_VERSION_CACHE.lock() {
                cache.insert(mod_id.to_string(), latest.clone());
            }
            latest
        }
        Err(e) => {
            warn!("latest mod version lookup failed for {mod_id}: {e}");
            None
        }
    }
}

/// Whether the catalog's latest release is newer than the installed version.
/// Catalog versions are normally semver; legacy labels fall back to identity
/// because the versions endpoint itself establishes newest-first ordering.
pub fn is_version_outdated(installed: &str, latest: &str) -> bool {
    let parse = |version: &str| semver::Version::parse(version.trim().trim_start_matches('v'));
    match (parse(installed), parse(latest)) {
        (Ok(installed), Ok(latest)) => latest > installed,
        _ => installed.trim() != latest.trim(),
    }
}

/// Resolve `mod_id` against the Starlight catalog, using (and populating)
/// the shared cache. A confirmed 404 caches a negative result; any other
/// failure (network error, timeout, 5xx) doesn't, so it's retried on the next
/// lookup instead of permanently mislabeling the mod as missing. Blocking —
/// does a network request on a cache miss, so call from the background
/// executor, never from `render`.
pub fn fetch(mod_id: &str) -> Option<ModResponse> {
    if let Some(cached) = get(mod_id) {
        return cached;
    }
    let result = (|| -> Result<Option<ModResponse>, reqwest::Error> {
        let response = reqwest::blocking::get(api::mod_url(mod_id))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(response.error_for_status()?.json::<ModResponse>()?))
    })();
    match result {
        Ok(info) => {
            if let Ok(mut cache) = CACHE.lock() {
                cache.insert(mod_id.to_string(), info.clone());
            }
            info
        }
        Err(e) => {
            // Transient failure — don't cache it, so the next lookup retries
            // instead of permanently treating this mod as missing.
            warn!("mod catalog lookup failed for {mod_id}: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_version_outdated;

    #[test]
    fn compares_catalog_versions_semantically() {
        assert!(is_version_outdated("1.9.0", "1.10.0"));
        assert!(!is_version_outdated("1.10.0", "1.9.0"));
        assert!(!is_version_outdated("v1.0.0", "1.0.0"));
    }

    #[test]
    fn compares_non_semver_versions_by_catalog_identity() {
        assert!(is_version_outdated("release-1", "release-2"));
        assert!(!is_version_outdated("release-2", "release-2"));
    }
}
