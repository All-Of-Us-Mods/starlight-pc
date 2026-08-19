//! Desktop shortcuts that launch a profile via the `starlight://` deep link.
//!
//! Windows-only: the shortcut is an `[InternetShortcut]` `.url` file written
//! to the desktop, and the scheme is registered per-user under
//! `HKCU\Software\Classes` so no elevation is needed. Opening the shortcut
//! makes the shell start the app with the URL as its first argument, which
//! `main` picks up via [`parse_profile_deep_link`] to auto-launch the profile.

pub const DEEP_LINK_SCHEME: &str = "starlight";
pub const PROFILE_LINK_HOST: &str = "profile";

/// What a `starlight://profile/…` deep link asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileDeepLink {
    /// `starlight://profile/{id}` — launch the profile (desktop shortcuts).
    Launch(String),
    /// `starlight://profile/{id}/edit` — open the profile's page in the app.
    Edit(String),
}

/// Parse a `starlight://profile/{id}[/edit]` deep link, as passed in argv when
/// the shell opens the registered scheme.
pub fn parse_profile_deep_link(arg: &str) -> Option<ProfileDeepLink> {
    let prefix = format!("{DEEP_LINK_SCHEME}://{PROFILE_LINK_HOST}/");
    // The shell may append a trailing slash to either form.
    let rest = arg.strip_prefix(&prefix)?.trim_end_matches('/');
    let (raw_id, edit) = match rest.split_once('/') {
        None => (rest, false),
        Some((id, action)) if action.eq_ignore_ascii_case("edit") => (id, true),
        Some(_) => return None,
    };
    if raw_id.is_empty() {
        return None;
    }
    let id = urlencoding::decode(raw_id).ok()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    Some(if edit {
        ProfileDeepLink::Edit(id)
    } else {
        ProfileDeepLink::Launch(id)
    })
}

#[cfg(windows)]
mod windows_impl {
    use super::{DEEP_LINK_SCHEME, PROFILE_LINK_HOST};
    use crate::backend::api;
    use crate::backend::error::{AppError, AppResult};
    use crate::backend::services::profile_service::{self, ProfileEntry};
    use log::warn;
    use std::fs;
    use std::path::PathBuf;

    const SHORTCUT_PREFIX: &str = "Starlight - ";
    const SHORTCUT_ICON_DIR: &str = "shortcut-icons";
    const SHORTCUT_ICON_SIZE: u32 = 256;

    /// Register (or refresh) the `starlight://` scheme for the current user so
    /// deep links open this executable. Cheap enough to run on every startup,
    /// which also keeps the registered path current when the app moves.
    pub fn register_deep_link_scheme() -> AppResult<()> {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;

        let exe = std::env::current_exe()?.to_string_lossy().to_string();
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(format!(r"Software\Classes\{DEEP_LINK_SCHEME}"))?;
        key.set_value("", &format!("URL:{DEEP_LINK_SCHEME} Protocol"))?;
        key.set_value("URL Protocol", &"")?;
        let (icon, _) = key.create_subkey("DefaultIcon")?;
        icon.set_value("", &format!("\"{exe}\",0"))?;
        let (command, _) = key.create_subkey(r"shell\open\command")?;
        command.set_value("", &format!("\"{exe}\" \"%1\""))?;
        Ok(())
    }

    /// Write a `.url` shortcut for the profile onto the user's desktop.
    /// Returns the shortcut's path.
    pub fn create_desktop_shortcut(profile_id: &str) -> AppResult<String> {
        let Some(profile) = profile_service::get_profile_by_id(profile_id)? else {
            return Err(AppError::validation(format!(
                "Profile '{profile_id}' not found"
            )));
        };

        let desktop_dir = directories::UserDirs::new()
            .and_then(|dirs| dirs.desktop_dir().map(|p| p.to_path_buf()))
            .ok_or_else(|| AppError::platform("Could not determine the desktop directory"))?;
        fs::create_dir_all(&desktop_dir)?;

        let shortcut_name = sanitize_shortcut_name(&profile.name);
        let shortcut_path = desktop_dir.join(format!("{SHORTCUT_PREFIX}{shortcut_name}.url"));
        let shortcut_url = format!(
            "{DEEP_LINK_SCHEME}://{PROFILE_LINK_HOST}/{}",
            urlencoding::encode(&profile.id)
        );
        let icon_path = resolve_icon_path(&profile)?;
        let contents = format!(
            "[InternetShortcut]\r\nURL={shortcut_url}\r\nIconFile={}\r\nIconIndex=0\r\n",
            icon_path.display()
        );
        fs::write(&shortcut_path, contents)?;

        Ok(shortcut_path.to_string_lossy().to_string())
    }

    /// The icon to point the shortcut at. Profiles with a custom or mod icon
    /// get it rendered to a `.ico` under the app data dir (`.url` icons must
    /// be .ico/.exe/.dll); anything else — including render/download failures
    /// — falls back to the app executable's embedded icon.
    fn resolve_icon_path(profile: &ProfileEntry) -> AppResult<PathBuf> {
        match write_shortcut_icon(profile) {
            Ok(Some(path)) => Ok(path),
            Ok(None) => Ok(std::env::current_exe()?),
            Err(e) => {
                warn!(
                    "shortcut icon for profile '{}' failed, using app icon: {e}",
                    profile.id
                );
                Ok(std::env::current_exe()?)
            }
        }
    }

    /// Source image bytes for the profile's icon, mirroring how the UI picks
    /// it: the `icon{ext}` file in the profile dir for `custom`, the catalog
    /// thumbnail for `mod`, nothing for `default`.
    fn profile_icon_bytes(profile: &ProfileEntry) -> AppResult<Option<Vec<u8>>> {
        match profile.icon_mode.as_deref() {
            Some("custom") => {
                let Some(ext) = profile
                    .custom_icon_extension
                    .as_deref()
                    .filter(|s| !s.is_empty())
                else {
                    return Ok(None);
                };
                let path = PathBuf::from(&profile.path).join(format!("icon{ext}"));
                Ok(Some(fs::read(path)?))
            }
            Some("mod") => {
                let Some(mod_id) = profile.icon_mod_id.as_deref().filter(|s| !s.is_empty()) else {
                    return Ok(None);
                };
                let response = reqwest::blocking::get(api::mod_thumbnail_url(mod_id))?;
                if !response.status().is_success() {
                    return Ok(None);
                }
                Ok(Some(response.bytes()?.to_vec()))
            }
            _ => Ok(None),
        }
    }

    /// Render the profile's icon to `{app_data}/shortcut-icons/{profile_id}.ico`.
    /// Returns `None` when the profile uses the default icon.
    fn write_shortcut_icon(profile: &ProfileEntry) -> AppResult<Option<PathBuf>> {
        let Some(bytes) = profile_icon_bytes(profile)? else {
            return Ok(None);
        };
        let img = image::load_from_memory(&bytes)
            .map_err(|e| AppError::other(format!("decode profile icon: {e}")))?;
        let resized = img.resize(
            SHORTCUT_ICON_SIZE,
            SHORTCUT_ICON_SIZE,
            image::imageops::FilterType::Lanczos3,
        );
        let dir = crate::backend::directories::app_data_dir()?.join(SHORTCUT_ICON_DIR);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.ico", profile.id));
        resized
            .save_with_format(&path, image::ImageFormat::Ico)
            .map_err(|e| AppError::other(format!("write shortcut icon: {e}")))?;
        Ok(Some(path))
    }

    /// Strip characters Windows forbids in file names, so the profile name can
    /// be used as the shortcut file name.
    fn sanitize_shortcut_name(name: &str) -> String {
        let cleaned: String = name
            .chars()
            .map(|ch| match ch {
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
                c if c.is_control() => '-',
                c => c,
            })
            .collect();
        let cleaned = cleaned.trim().trim_end_matches('.').to_string();
        if cleaned.is_empty() {
            "Profile".to_string()
        } else {
            cleaned
        }
    }
}

#[cfg(windows)]
pub use windows_impl::{create_desktop_shortcut, register_deep_link_scheme};

#[cfg(test)]
mod tests {
    use super::{ProfileDeepLink, parse_profile_deep_link};

    #[test]
    fn parses_profile_deep_link() {
        assert_eq!(
            parse_profile_deep_link("starlight://profile/my-profile-123"),
            Some(ProfileDeepLink::Launch("my-profile-123".to_string()))
        );
        // Shell-appended trailing slash.
        assert_eq!(
            parse_profile_deep_link("starlight://profile/my-profile-123/"),
            Some(ProfileDeepLink::Launch("my-profile-123".to_string()))
        );
        assert_eq!(parse_profile_deep_link("starlight://profile/"), None);
        assert_eq!(parse_profile_deep_link("starlight://profile/a/b"), None);
        assert_eq!(parse_profile_deep_link("starlight://other/x"), None);
        assert_eq!(parse_profile_deep_link("--flag"), None);
    }

    #[test]
    fn parses_edit_deep_link() {
        assert_eq!(
            parse_profile_deep_link("starlight://profile/my-profile-123/edit"),
            Some(ProfileDeepLink::Edit("my-profile-123".to_string()))
        );
        assert_eq!(
            parse_profile_deep_link("starlight://profile/my-profile-123/edit/"),
            Some(ProfileDeepLink::Edit("my-profile-123".to_string()))
        );
        // Percent-encoded ids decode the same for both forms.
        assert_eq!(
            parse_profile_deep_link("starlight://profile/town%20of%20us-1/edit"),
            Some(ProfileDeepLink::Edit("town of us-1".to_string()))
        );
        assert_eq!(parse_profile_deep_link("starlight://profile//edit"), None);
        assert_eq!(
            parse_profile_deep_link("starlight://profile/my-profile-123/delete"),
            None
        );
    }
}
