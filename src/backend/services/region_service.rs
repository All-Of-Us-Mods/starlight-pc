//! Reads and writes Among Us' `regionInfo.json` so community servers from the
//! Starlight servers API can be added as selectable in-game regions.
//!
//! Region entries are kept as raw JSON values so region kinds we don't model
//! (e.g. DNS regions) survive a read/write round-trip untouched. `$type` sorts
//! first in serde_json's (BTreeMap-backed) object output, which is what Among
//! Us' polymorphic deserializer expects.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::backend::api::Server;
use crate::backend::error::{AppError, AppResult};

const STATIC_HTTP_TYPE: &str = "StaticHttpRegionInfo, Assembly-CSharp";
/// `TranslateName` for user-added regions — the id the modded servers use, so
/// Among Us shows the region's literal `Name`.
pub const CUSTOM_TRANSLATE_NAME: i64 = 1003;

/// Among Us' region list (`regionInfo.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionInfo {
    #[serde(rename = "CurrentRegionIdx")]
    pub current_region_idx: i32,
    #[serde(rename = "Regions")]
    pub regions: Vec<Value>,
}

/// Display name of a region entry (falls back to `"Unknown"`).
pub fn region_name(region: &Value) -> &str {
    region
        .get("Name")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
}

/// Whether `region` already targets `address`:`port`. Matched on host + port
/// (ignoring the URL scheme) so we can tell an API server is installed
/// regardless of its display name.
pub fn region_has_server(region: &Value, address: &str, port: u16) -> bool {
    let ping_matches = region
        .get("PingServer")
        .and_then(Value::as_str)
        .is_some_and(|p| host_of(p).eq_ignore_ascii_case(address));
    region
        .get("Servers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|s| {
            let port_matches =
                s.get("Port").and_then(Value::as_u64).unwrap_or(0) == u64::from(port);
            let host_matches = s
                .get("Ip")
                .and_then(Value::as_str)
                .is_some_and(|ip| host_of(ip).eq_ignore_ascii_case(address));
            port_matches && (host_matches || ping_matches)
        })
}

/// Host + port of a region's primary server, suitable for querying optional
/// endpoints like the public lobby list. Scheme-agnostic — `build_region`
/// only guesses a scheme from the port number, which doesn't reliably
/// indicate whether an arbitrary custom server actually speaks TLS, so
/// callers that need to make an HTTP request should try both schemes rather
/// than trust `Ip`'s prefix. `None` if the region has no usable server entry.
pub fn region_server_host_port(region: &Value) -> Option<(String, u16)> {
    let server = region
        .get("Servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())?;
    let ip = server.get("Ip").and_then(Value::as_str)?;
    let port = server.get("Port").and_then(Value::as_u64).unwrap_or(443) as u16;
    Some((host_of(ip).to_string(), port))
}

/// A region the user has enabled (present in `regionInfo.json`) paired with its
/// server's host+port — the set of servers to poll for public lobby lists.
#[derive(Debug, Clone)]
pub struct LobbyServer {
    pub region_name: String,
    pub host: String,
    pub port: u16,
}

/// The enabled regions to query for lobbies: every region in `regionInfo.json`
/// that exposes a usable server host+port.
pub fn lobby_servers() -> AppResult<Vec<LobbyServer>> {
    let info = read_region_info()?;
    Ok(info
        .regions
        .iter()
        .filter_map(|region| {
            region_server_host_port(region).map(|(host, port)| LobbyServer {
                region_name: region_name(region).to_string(),
                host,
                port,
            })
        })
        .collect())
}

/// Point Among Us' in-game region selector at the region whose primary server
/// matches `host`:`port` — the same host+port comparison `region_has_server`
/// uses, so this can't drift from "is this the same server" out of sync with
/// the rest of region matching. Returns whether a matching region was found.
pub fn select_region_by_host_port(host: &str, port: u16) -> AppResult<bool> {
    let mut info = read_region_info()?;
    let Some(idx) = info
        .regions
        .iter()
        .position(|region| region_has_server(region, host, port))
    else {
        return Ok(false);
    };
    info.current_region_idx = idx as i32;
    write_region_info(&info)?;
    Ok(true)
}

/// The host portion of a server URL — strips an `http(s)://` scheme and any path.
fn host_of(url: &str) -> &str {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    without_scheme.split('/').next().unwrap_or(without_scheme)
}

#[cfg(windows)]
fn region_info_path() -> AppResult<PathBuf> {
    // %APPDATA% is the Roaming folder; regionInfo.json lives one level up under
    // LocalLow (…/AppData/LocalLow/Innersloth/Among Us/regionInfo.json).
    let roaming = std::env::var_os("APPDATA")
        .ok_or_else(|| AppError::state("APPDATA environment variable is not set"))?;
    let app_data = PathBuf::from(roaming)
        .parent()
        .ok_or_else(|| AppError::state("Could not resolve the LocalLow directory"))?
        .to_path_buf();
    Ok(app_data
        .join("LocalLow")
        .join("Innersloth")
        .join("Among Us")
        .join("regionInfo.json"))
}

#[cfg(target_os = "linux")]
fn region_info_path() -> AppResult<PathBuf> {
    use crate::backend::services::core_service::{self, LinuxRunnerKind};

    let settings = core_service::get_settings()?;
    match settings.linux_runner_kind {
        LinuxRunnerKind::Wine => {
            let explicit = settings.linux_wine_region_info_path.trim();
            if !explicit.is_empty() {
                return Ok(expand_tilde(explicit));
            }
            let prefix = settings.linux_wine_prefix.trim();
            if prefix.is_empty() {
                return Err(AppError::state(
                    "Set the Wine prefix or RegionInfo.json path in Settings → Linux runtime",
                ));
            }
            Ok(region_info_in(wine_user_dir(&expand_tilde(prefix))?))
        }
        LinuxRunnerKind::Proton | LinuxRunnerKind::Steam => {
            let compat = settings.linux_proton_compat_data_path.trim();
            if compat.is_empty() {
                return Err(AppError::state(
                    "Set the Proton compat data path in Settings → Linux runtime \
                     (Auto-detect can find it)",
                ));
            }
            Ok(region_info_in(
                expand_tilde(compat)
                    .join("pfx")
                    .join("drive_c")
                    .join("users")
                    .join("steamuser"),
            ))
        }
    }
}

/// Expand a leading `~/` to the home directory — user-entered paths reach us
/// verbatim, and a literal `~` component would resolve relative to the
/// process working directory.
#[cfg(target_os = "linux")]
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// The user directory inside a Wine prefix's `drive_c/users`. The host
/// username doesn't reliably name it (prefixes can be created by another
/// account or copied from another machine), so prefer the directory that
/// actually holds Among Us' LocalLow data, then `$USER`, then a sole
/// remaining candidate.
#[cfg(target_os = "linux")]
fn wine_user_dir(prefix: &std::path::Path) -> AppResult<PathBuf> {
    let users = prefix.join("drive_c").join("users");
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&users) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || entry.file_name() == "Public" {
                continue;
            }
            if dir
                .join("AppData")
                .join("LocalLow")
                .join("Innersloth")
                .join("Among Us")
                .exists()
            {
                return Ok(dir);
            }
            candidates.push(dir);
        }
    }
    if let Ok(user) = std::env::var("USER") {
        let dir = users.join(&user);
        if dir.is_dir() {
            return Ok(dir);
        }
    }
    if candidates.len() == 1 {
        return Ok(candidates.remove(0));
    }
    Err(AppError::state(
        "Could not find the user folder inside the Wine prefix; set the \
         RegionInfo.json path in Settings → Linux runtime",
    ))
}

/// `RegionInfo.json` under a Wine user directory. Wine resolves paths
/// case-insensitively but we hit the host filesystem directly, so prefer
/// whichever casing already exists on disk.
#[cfg(target_os = "linux")]
fn region_info_in(user_dir: PathBuf) -> PathBuf {
    let dir = user_dir
        .join("AppData")
        .join("LocalLow")
        .join("Innersloth")
        .join("Among Us");
    let lower = dir.join("regionInfo.json");
    if lower.exists() {
        lower
    } else {
        dir.join("RegionInfo.json")
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn region_info_path() -> AppResult<PathBuf> {
    Err(AppError::platform(
        "Configuring Among Us regions is only supported on Windows and Linux",
    ))
}

/// Read the current region file. A missing file yields an empty region list to
/// append to; a malformed file is reported as an error rather than being
/// silently overwritten.
pub fn read_region_info() -> AppResult<RegionInfo> {
    let path = region_info_path()?;
    match std::fs::read_to_string(&path) {
        Ok(raw) => Ok(serde_json::from_str(&raw)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty_region_info()),
        Err(e) => Err(e.into()),
    }
}

fn write_region_info(info: &RegionInfo) -> AppResult<()> {
    let path = region_info_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec(info)?)?;
    Ok(())
}

/// Add a server from the API as a region. No-op (returns `false`) if a region
/// already targets the same host:port.
pub fn add_server_region(server: &Server) -> AppResult<bool> {
    let mut info = read_region_info()?;
    if info
        .regions
        .iter()
        .any(|r| region_has_server(r, &server.address, server.port))
    {
        return Ok(false);
    }
    info.regions.push(server_region(server));
    write_region_info(&info)?;
    Ok(true)
}

/// Add a user-provided server as a region, normalizing the address to a bare
/// host. `translate_name` is [`CUSTOM_TRANSLATE_NAME`] for anything the user
/// adds by hand; deep links may carry their own. No-op (returns `false`) if a
/// region already targets the same host:port.
pub fn add_custom_region(
    name: &str,
    address: &str,
    port: u16,
    dtls: bool,
    translate_name: i64,
) -> AppResult<bool> {
    let host = host_of(address.trim());
    let mut info = read_region_info()?;
    if info
        .regions
        .iter()
        .any(|r| region_has_server(r, host, port))
    {
        return Ok(false);
    }
    info.regions
        .push(build_region(name.trim(), host, port, dtls, translate_name));
    write_region_info(&info)?;
    Ok(true)
}

/// Remove the region with `name`.
pub fn remove_region(name: &str) -> AppResult<()> {
    let mut info = read_region_info()?;
    info.regions.retain(|r| region_name(r) != name);
    // Keep Among Us' in-game selection index within bounds after the removal.
    let max = info.regions.len().saturating_sub(1) as i32;
    info.current_region_idx = info.current_region_idx.clamp(0, max);
    write_region_info(&info)?;
    Ok(())
}

fn server_region(server: &Server) -> Value {
    build_region(
        &server.name,
        &server.address,
        server.port,
        server.dtls,
        server.translate_name,
    )
}

fn build_region(name: &str, address: &str, port: u16, dtls: bool, translate_name: i64) -> Value {
    let scheme = if port == 443 { "https" } else { "http" };
    let ip = format!("{scheme}://{address}");
    json!({
        "$type": STATIC_HTTP_TYPE,
        "Name": name,
        "PingServer": address,
        "Servers": [{
            "Name": "Http-1",
            "Ip": ip,
            "Port": port,
            "UseDtls": dtls,
            "Players": 0,
            "ConnectionFailures": 0,
        }],
        "TargetServer": null,
        "TranslateName": translate_name,
    })
}

fn empty_region_info() -> RegionInfo {
    RegionInfo {
        current_region_idx: 0,
        regions: Vec::new(),
    }
}
