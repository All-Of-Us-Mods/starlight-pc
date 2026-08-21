use crate::backend::error::AppResult;
use serde::{Deserialize, Serialize};

pub const DEFAULT_API_BASE_URL: &str = "https://starlight.allofus.dev";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Post {
    pub id: u32,
    pub title: String,
    pub author: String,
    pub content: String,
    pub tags: Option<Vec<String>>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModResponse {
    pub status: Option<String>,
    pub id: String,
    pub name: String,
    pub description: String,
    pub long_description: Option<String>,
    pub author: String,
    pub mod_type: Option<String>,
    pub license: Option<String>,
    pub links: Option<Vec<ExternalLink>>,
    pub tags: Option<Vec<String>>,
    pub created_at: i64,
    pub updated_at: i64,
    pub downloads: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExternalLink {
    #[serde(rename = "type")]
    pub link_type: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModVersion {
    pub status: Option<String>,
    pub name: String,
    pub version: String,
    pub supported_platforms: Option<Vec<String>>,
    pub downloads: u64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModDependency {
    pub mod_id: String,
    pub name: String,
    pub version_constraint: String,
    #[serde(rename = "type")]
    pub dependency_type: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlatformDownload {
    pub platform: String,
    pub architecture: String,
    pub file_name: Option<String>,
    pub checksum: Option<String>,
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModVersionInfo {
    pub status: Option<String>,
    pub name: String,
    pub version: String,
    pub supported_platforms: Option<Vec<String>>,
    pub downloads: u64,
    pub created_at: i64,
    pub changelog: Option<String>,
    pub dependencies: Vec<ModDependency>,
    pub file_name: Option<String>,
    pub checksum: Option<String>,
    pub download_url: Option<String>,
    pub platforms: Option<Vec<PlatformDownload>>,
}

/// A community server from the Starlight servers API, addable as an in-game
/// Among Us region via `region_service`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Server {
    pub id: u32,
    pub name: String,
    pub owner: String,
    pub address: String,
    pub port: u16,
    pub dtls: bool,
    pub translate_name: i64,
}

/// Response of the optional public lobby list endpoint (`/x-api/games`) that
/// some modded servers implement. See `hpllp013.yaml`. All fields are optional
/// so a server returning a partial payload still deserializes.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GamesResult {
    #[serde(default)]
    pub games: Vec<Game>,
    #[serde(default)]
    pub regions: Vec<LobbyRegion>,
}

/// A single active game (lobby) advertised by a server's lobby list.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Game {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub host_name: Option<String>,
    /// `Lobby`, `Started`, or `Ended` (see Impostor's `GameStates`).
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub player_count: Option<u32>,
    #[serde(default)]
    pub max_players: Option<u32>,
    #[serde(default)]
    pub chat_lang: Option<i64>,
    #[serde(default)]
    pub map_id: Option<u32>,
    /// Matches an `id` in the response's `regions` list.
    #[serde(default)]
    pub region_id: Option<String>,
    #[serde(default)]
    pub mods: Vec<LobbyMod>,
}

/// A mod a lobby requires. `id`/`version` match the Starlight catalog so they
/// can be passed straight to the mod install pipeline.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LobbyMod {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub flags: Option<i64>,
}

/// A region as described by a server's lobby list (distinct from Among Us'
/// local `regionInfo.json` regions). Used to label a lobby's `region_id`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LobbyRegion {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

fn get_json<T: for<'de> Deserialize<'de>>(url: &str) -> AppResult<T> {
    Ok(reqwest::blocking::get(url)?
        .error_for_status()?
        .json::<T>()?)
}

pub fn fetch_news() -> AppResult<Vec<Post>> {
    get_json(&format!("{}/api/v3/news/posts", DEFAULT_API_BASE_URL))
}

pub fn fetch_trending_mods() -> AppResult<Vec<ModResponse>> {
    get_json(&format!("{}/api/v3/mods/trending", DEFAULT_API_BASE_URL))
}

pub fn fetch_mods(limit: u32, offset: u32) -> AppResult<Vec<ModResponse>> {
    get_json(&format!(
        "{}/api/v3/mods?limit={}&offset={}",
        DEFAULT_API_BASE_URL, limit, offset
    ))
}

pub fn fetch_mod(id: &str) -> AppResult<ModResponse> {
    get_json(&format!("{}/api/v3/mods/{}", DEFAULT_API_BASE_URL, id))
}

pub fn mod_url(id: &str) -> String {
    format!("{}/api/v3/mods/{}", DEFAULT_API_BASE_URL, id)
}

pub fn fetch_mod_versions(id: &str) -> AppResult<Vec<ModVersion>> {
    get_json(&format!(
        "{}/api/v3/mods/{}/versions",
        DEFAULT_API_BASE_URL, id
    ))
}

pub fn fetch_mod_version_info(id: &str, version: &str) -> AppResult<ModVersionInfo> {
    get_json(&format!(
        "{}/api/v3/mods/{}/versions/{}",
        DEFAULT_API_BASE_URL,
        id,
        urlencoding::encode(version)
    ))
}

pub fn mod_thumbnail_url(id: &str) -> String {
    format!("{}/api/v3/mods/{}/thumbnail", DEFAULT_API_BASE_URL, id)
}

pub fn search_mods(query: &str, limit: u32, offset: u32) -> AppResult<Vec<ModResponse>> {
    get_json(&format!(
        "{}/api/v3/mods/search?q={}&limit={}&offset={}",
        DEFAULT_API_BASE_URL,
        urlencoding::encode(query),
        limit,
        offset,
    ))
}

pub fn fetch_servers() -> AppResult<Vec<Server>> {
    get_json(&format!("{}/api/v3/servers", DEFAULT_API_BASE_URL))
}

/// Fetch the public lobby list from a server that implements the optional
/// lobby list endpoint (`/public_api/games`, with `/x-api/games` as a
/// fallback — see `hpllp013.yaml`). `host`/`port` identify the
/// region's server; the scheme to use isn't reliably known (a custom region's
/// stored `Ip` scheme is only ever a guess — see `region_service::build_region`),
/// so this tries HTTPS first and falls back to plain HTTP. Short timeouts keep
/// a non-implementing or unresponsive server from stalling a refresh — callers
/// treat any error as "this server has no lobby list" and skip it.
pub fn fetch_lobbies(host: &str, port: u16) -> AppResult<GamesResult> {
    let client = crate::backend::services::http_download::http_client(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(8),
    )?;

    let mut last_err = None;
    for scheme in ["https", "http"] {
        let default_port = if scheme == "https" { 443 } else { 80 };
        let origin = if port == default_port {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{port}")
        };
        for path in ["public_api/games", "x-api/games"] {
            match client
                .get(format!("{origin}/{path}"))
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
            {
                Ok(response) => return Ok(response.json::<GamesResult>()?),
                Err(e) => last_err = Some(e),
            }
        }
    }
    Err(last_err.expect("loop runs at least once").into())
}
