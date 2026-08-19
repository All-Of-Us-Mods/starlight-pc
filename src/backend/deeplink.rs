//! `starlight://` deep links — one parser for every link the app understands.
//!
//! Links reach the app two ways: in argv when the shell opens the registered
//! scheme, and forwarded verbatim by a second instance (see
//! [`crate::backend::single_instance`]). Both go through [`parse`], and the
//! resulting [`DeepLink`] is routed in one place (`main::handle_deep_link`),
//! which either does the backend work itself or publishes
//! [`crate::backend::events::BackendEvent::DeepLink`] for the workspace to
//! act on — via [`dispatch_to_ui`], which holds links that arrive before the
//! workspace is listening.
//!
//! Supported links:
//!
//! | Link | Effect |
//! |------|--------|
//! | `starlight://profile/{id}` | Launch the profile (what desktop shortcuts use) |
//! | `starlight://profile/{id}/edit` | Open the profile's page |
//! | `starlight://mods/{id}` | Open the mod's page |
//! | `starlight://servers/add?name=…&address=…` | Add a server region and show the Servers page |
//!
//! Adding a link means adding a variant here, parsing it in [`parse`], and
//! handling that variant at both routing sites.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crate::backend::events::{self, BackendEvent};

pub const SCHEME: &str = "starlight";

/// Port used when a `servers/add` link doesn't specify one.
pub const DEFAULT_SERVER_PORT: u16 = 22023;

/// A parsed `starlight://` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    /// `starlight://profile/{id}` — launch the profile.
    LaunchProfile(String),
    /// `starlight://profile/{id}/edit` — open the profile's page.
    OpenProfile(String),
    /// `starlight://mods/{id}` — open the mod's page.
    OpenMod(String),
    /// `starlight://servers/add?…` — add a server and show the Servers page.
    AddServer(Box<ServerLink>),
}

/// The server described by a `starlight://servers/add` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerLink {
    /// `name` (required) — the region's display name.
    pub name: String,
    /// `address` (required) — IP or hostname, without a scheme.
    pub address: String,
    /// `port` — the ping server's port. Defaults to [`DEFAULT_SERVER_PORT`].
    pub port: u16,
    /// `host` — who runs the server. Defaults to `address`.
    pub host: String,
    /// `dtls` — whether the server speaks DTLS. Defaults to false.
    pub dtls: bool,
    /// `translateName` — Among Us localization id; 1003 (the default) makes it
    /// show the literal name, which is what custom servers want.
    pub translate_name: i64,
    /// `editable` — whether the user may edit the server. Defaults to true.
    pub editable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Not a `starlight://` URL at all — an ordinary argv entry.
    NotADeepLink,
    /// A `starlight://` URL whose path isn't one we handle.
    Unsupported(String),
    /// A link we recognize with missing or malformed parameters.
    Invalid(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::NotADeepLink => write!(f, "not a {SCHEME}:// link"),
            ParseError::Unsupported(path) => write!(f, "unsupported {SCHEME}:// link: {path}"),
            ParseError::Invalid(reason) => write!(f, "invalid {SCHEME}:// link: {reason}"),
        }
    }
}

/// Parse a `starlight://` URL as passed in argv (or forwarded by a second
/// instance). Percent-encoded segments and query values are decoded.
pub fn parse(arg: &str) -> Result<DeepLink, ParseError> {
    let rest = arg
        .strip_prefix(&format!("{SCHEME}://"))
        .ok_or(ParseError::NotADeepLink)?;
    // Fragments carry nothing for us; the query is parsed per link kind.
    let rest = rest.split('#').next().unwrap_or(rest);
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, query),
        None => (rest, ""),
    };
    // The shell likes to append a trailing slash to scheme URLs.
    let path = path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').collect();

    match segments.as_slice() {
        ["profile", id] => Ok(DeepLink::LaunchProfile(profile_or_mod_id(id, "profile")?)),
        ["profile", id, action] if action.eq_ignore_ascii_case("edit") => {
            Ok(DeepLink::OpenProfile(profile_or_mod_id(id, "profile")?))
        }
        ["mods", id] => Ok(DeepLink::OpenMod(profile_or_mod_id(id, "mod")?)),
        ["servers", "add"] => Ok(DeepLink::AddServer(Box::new(parse_server_link(query)?))),
        _ => Err(ParseError::Unsupported(path.to_string())),
    }
}

/// Links waiting for the workspace to start listening. The event bus only
/// delivers to receivers that already exist, and a forwarded link can arrive
/// during startup — before the workspace subscribes — so those are parked here
/// instead of being dropped.
struct UiSink {
    listening: bool,
    pending: Vec<DeepLink>,
}

static UI_SINK: Mutex<UiSink> = Mutex::new(UiSink {
    listening: false,
    pending: Vec::new(),
});

/// Hand a link to the UI: published now if the workspace is listening, parked
/// for [`take_pending_ui_links`] if it isn't yet.
pub fn dispatch_to_ui(link: DeepLink) {
    let mut sink = UI_SINK.lock().unwrap_or_else(|e| e.into_inner());
    if sink.listening {
        events::publish(BackendEvent::DeepLink(link));
    } else {
        sink.pending.push(link);
    }
}

/// Called by the workspace right after it subscribes to the event bus: marks
/// the UI as listening and returns the links that arrived before then.
pub fn take_pending_ui_links() -> Vec<DeepLink> {
    let mut sink = UI_SINK.lock().unwrap_or_else(|e| e.into_inner());
    sink.listening = true;
    std::mem::take(&mut sink.pending)
}

/// The `starlight://profile/{id}` URL a desktop shortcut launches.
pub fn profile_launch_url(profile_id: &str) -> String {
    format!("{SCHEME}://profile/{}", urlencoding::encode(profile_id))
}

fn profile_or_mod_id(raw: &str, kind: &str) -> Result<String, ParseError> {
    let id = decode(raw).trim().to_string();
    if id.is_empty() {
        return Err(ParseError::Invalid(format!("missing {kind} id")));
    }
    Ok(id)
}

fn parse_server_link(query: &str) -> Result<ServerLink, ParseError> {
    let params = parse_query(query);
    let get = |key: &str| params.get(key).map(String::as_str).map(str::trim);

    let name = get("name")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ParseError::Invalid("servers/add needs a 'name' parameter".to_string()))?;
    // Addresses are stored (and matched) as bare hosts; a pasted scheme is a
    // common enough mistake to just strip.
    let address = get("address")
        .map(|address| {
            address
                .strip_prefix("https://")
                .or_else(|| address.strip_prefix("http://"))
                .unwrap_or(address)
                .trim_end_matches('/')
        })
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            ParseError::Invalid("servers/add needs an 'address' parameter".to_string())
        })?;

    let port = match get("port").filter(|v| !v.is_empty()) {
        Some(raw) => raw
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| {
                ParseError::Invalid(format!("'{raw}' is not a port between 1 and 65535"))
            })?,
        None => DEFAULT_SERVER_PORT,
    };
    let translate_name = match get("translatename").filter(|v| !v.is_empty()) {
        Some(raw) => raw
            .parse::<i64>()
            .map_err(|_| ParseError::Invalid(format!("'{raw}' is not a valid translateName")))?,
        None => crate::backend::services::region_service::CUSTOM_TRANSLATE_NAME,
    };

    Ok(ServerLink {
        name: name.to_string(),
        host: get("host")
            .filter(|v| !v.is_empty())
            .unwrap_or(address)
            .to_string(),
        address: address.to_string(),
        port,
        dtls: parse_bool(get("dtls"), "dtls", false)?,
        translate_name,
        editable: parse_bool(get("editable"), "editable", true)?,
    })
}

fn parse_bool(raw: Option<&str>, key: &str, default: bool) -> Result<bool, ParseError> {
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return Ok(default);
    };
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(ParseError::Invalid(format!(
            "'{raw}' is not a true/false value for '{key}'"
        ))),
    }
}

/// Query string to decoded key/value pairs. Keys are lowercased so
/// `translateName` and `translatename` both work; later duplicates win.
fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key).to_ascii_lowercase(), decode(value))
        })
        .collect()
}

/// Percent-decode, falling back to the raw text if it isn't valid UTF-8.
fn decode(raw: &str) -> String {
    urlencoding::decode(raw)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(link: DeepLink) -> ServerLink {
        match link {
            DeepLink::AddServer(server) => *server,
            other => panic!("expected a server link, got {other:?}"),
        }
    }

    #[test]
    fn parses_profile_links() {
        assert_eq!(
            parse("starlight://profile/my-profile-123"),
            Ok(DeepLink::LaunchProfile("my-profile-123".into()))
        );
        // Shell-appended trailing slash.
        assert_eq!(
            parse("starlight://profile/my-profile-123/"),
            Ok(DeepLink::LaunchProfile("my-profile-123".into()))
        );
        assert_eq!(
            parse("starlight://profile/town%20of%20us-1/edit"),
            Ok(DeepLink::OpenProfile("town of us-1".into()))
        );
        assert_eq!(
            parse("starlight://profile/"),
            Err(ParseError::Unsupported("profile".into()))
        );
        assert_eq!(
            parse("starlight://profile/a/b"),
            Err(ParseError::Unsupported("profile/a/b".into()))
        );
        assert_eq!(parse("--flag"), Err(ParseError::NotADeepLink));
    }

    #[test]
    fn parses_mod_links() {
        assert_eq!(
            parse("starlight://mods/reactor"),
            Ok(DeepLink::OpenMod("reactor".into()))
        );
        assert_eq!(
            parse("starlight://mods/"),
            Err(ParseError::Unsupported("mods".into()))
        );
    }

    #[test]
    fn server_link_defaults_everything_but_name_and_address() {
        let link = server(
            parse("starlight://servers/add?name=AOU%20Europe&address=eu.allofus.dev").unwrap(),
        );
        assert_eq!(link.name, "AOU Europe");
        assert_eq!(link.address, "eu.allofus.dev");
        assert_eq!(link.port, DEFAULT_SERVER_PORT);
        // The hoster defaults to the address, not to a blank string.
        assert_eq!(link.host, "eu.allofus.dev");
        assert!(!link.dtls);
        assert_eq!(link.translate_name, 1003);
        assert!(link.editable);
    }

    #[test]
    fn server_link_reads_every_option() {
        let link = server(
            parse(
                "starlight://servers/add?name=Custom&address=https://eu.allofus.dev/\
                 &port=443&host=All%20Of%20Us&dtls=true&translateName=42&editable=false",
            )
            .unwrap(),
        );
        // A pasted scheme and trailing slash are stripped from the address.
        assert_eq!(link.address, "eu.allofus.dev");
        assert_eq!(link.port, 443);
        assert_eq!(link.host, "All Of Us");
        assert!(link.dtls);
        assert_eq!(link.translate_name, 42);
        assert!(!link.editable);
    }

    #[test]
    fn server_link_rejects_missing_and_malformed_parameters() {
        assert_eq!(
            parse("starlight://servers/add?address=eu.allofus.dev"),
            Err(ParseError::Invalid(
                "servers/add needs a 'name' parameter".into()
            ))
        );
        assert_eq!(
            parse("starlight://servers/add?name=Custom"),
            Err(ParseError::Invalid(
                "servers/add needs an 'address' parameter".into()
            ))
        );
        assert!(matches!(
            parse("starlight://servers/add?name=C&address=a.dev&port=99999"),
            Err(ParseError::Invalid(_))
        ));
        assert!(matches!(
            parse("starlight://servers/add?name=C&address=a.dev&dtls=maybe"),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn shortcut_url_round_trips_through_the_parser() {
        let url = profile_launch_url("town of us-1");
        assert_eq!(
            parse(&url),
            Ok(DeepLink::LaunchProfile("town of us-1".into()))
        );
    }
}
