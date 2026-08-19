use crate::backend::directories;
use crate::backend::error::AppResult;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_BEPINEX_URL_X86: &str = "https://builds.bepinex.dev/projects/bepinex_be/752/BepInEx-Unity.IL2CPP-win-x86-6.0.0-be.752%2Bdd0655f.zip";
const DEFAULT_BEPINEX_URL_X64: &str = "https://builds.bepinex.dev/projects/bepinex_be/752/BepInEx-Unity.IL2CPP-win-x64-6.0.0-be.752%2Bdd0655f.zip";
const SETTINGS_FILE_NAME: &str = "settings.json";
const BOOT_CONFIG_FILE_NAME: &str = "boot.config";
/// Unity writes this entry with hyphens, alongside `build-guid=` and friends.
const SINGLE_INSTANCE_KEY: &str = "single-instance=";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GamePlatform {
    Steam,
    Epic,
    Xbox,
}

impl GamePlatform {
    /// Which BepInEx build the platform's game binary needs. Steam (and
    /// itch.io, if it's ever added) ships a 32-bit Among Us; Epic and Xbox
    /// ship 64-bit.
    pub fn bepinex_arch(self) -> BepInExArch {
        match self {
            GamePlatform::Steam => BepInExArch::X86,
            GamePlatform::Epic | GamePlatform::Xbox => BepInExArch::X64,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            GamePlatform::Steam => "Steam",
            GamePlatform::Epic => "Epic",
            GamePlatform::Xbox => "Xbox",
        }
    }
}

/// Architecture of an installed BepInEx build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BepInExArch {
    X86,
    X64,
}

impl BepInExArch {
    pub fn as_str(self) -> &'static str {
        match self {
            BepInExArch::X86 => "x86",
            BepInExArch::X64 => "x64",
        }
    }
}

fn default_true() -> bool {
    true
}

/// Sidebar width in pixels. Below `crate::workspace::SIDEBAR_COLLAPSE_WIDTH`
/// the sidebar renders as an icon rail — see `crate::workspace`.
fn default_sidebar_width() -> f32 {
    175.0
}
/// Name of the JSON theme applied on startup when settings don't name one.
/// Themes are resolved by name against `crate::theme`'s registry.
fn default_theme_name() -> String {
    "Starlight".to_string()
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinuxRunnerKind {
    Wine,
    Proton,
    /// Hand the launch to the Steam client (`steam -applaunch`) so Steamworks
    /// (online) and the Steam Linux Runtime (audio) are set up by Steam itself.
    #[default]
    Steam,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub bepinex_url_x86: String,
    pub bepinex_url_x64: String,
    pub among_us_path: String,
    pub close_on_launch: bool,
    pub allow_multi_instance_launch: bool,
    pub game_platform: GamePlatform,
    pub cache_bepinex: bool,
    pub xbox_app_id: Option<String>,
    #[serde(default)]
    pub linux_runner_kind: LinuxRunnerKind,
    #[serde(default)]
    pub linux_runner_binary: String,
    #[serde(default)]
    pub linux_wine_prefix: String,
    /// Explicit path to Among Us' `RegionInfo.json` for plain Wine setups,
    /// where the prefix layout (user name inside `drive_c/users`) varies.
    /// Empty means "derive from the Wine prefix".
    #[serde(default)]
    pub linux_wine_region_info_path: String,
    #[serde(default)]
    pub linux_proton_compat_data_path: String,
    #[serde(default)]
    pub linux_proton_steam_client_path: String,
    #[serde(default)]
    pub linux_proton_use_steam_run: bool,
    /// Name of the active JSON theme (see `crate::theme`).
    #[serde(default = "default_theme_name")]
    pub theme_name: String,
    #[serde(default = "default_true")]
    pub show_stars_background: bool,
    /// Width the user dragged the sidebar to; drives icon mode when small.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            bepinex_url_x86: DEFAULT_BEPINEX_URL_X86.to_string(),
            bepinex_url_x64: DEFAULT_BEPINEX_URL_X64.to_string(),
            among_us_path: String::new(),
            close_on_launch: false,
            allow_multi_instance_launch: false,
            game_platform: GamePlatform::Steam,
            cache_bepinex: false,
            xbox_app_id: None,
            linux_runner_kind: LinuxRunnerKind::Steam,
            linux_runner_binary: String::new(),
            linux_wine_prefix: String::new(),
            linux_wine_region_info_path: String::new(),
            linux_proton_compat_data_path: String::new(),
            linux_proton_steam_client_path: String::new(),
            linux_proton_use_steam_run: true,
            theme_name: default_theme_name(),
            show_stars_background: true,
            sidebar_width: default_sidebar_width(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AppSettingsPatch {
    pub bepinex_url_x86: Option<String>,
    pub bepinex_url_x64: Option<String>,
    pub among_us_path: Option<String>,
    pub close_on_launch: Option<bool>,
    pub allow_multi_instance_launch: Option<bool>,
    pub game_platform: Option<GamePlatform>,
    pub cache_bepinex: Option<bool>,
    pub xbox_app_id: Option<Option<String>>,
    pub linux_runner_kind: Option<LinuxRunnerKind>,
    pub linux_runner_binary: Option<String>,
    pub linux_wine_prefix: Option<String>,
    pub linux_wine_region_info_path: Option<String>,
    pub linux_proton_compat_data_path: Option<String>,
    pub linux_proton_steam_client_path: Option<String>,
    pub linux_proton_use_steam_run: Option<bool>,
    pub theme_name: Option<String>,
    pub show_stars_background: Option<bool>,
    pub sidebar_width: Option<f32>,
}

fn settings_path() -> AppResult<PathBuf> {
    Ok(directories::app_data_dir()?.join(SETTINGS_FILE_NAME))
}

fn write_settings_to_file(path: &Path, settings: &AppSettings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary_path = path.with_extension("json.tmp");
    fs::write(&temporary_path, serde_json::to_vec_pretty(settings)?)?;
    fs::rename(&temporary_path, path)?;
    Ok(())
}

fn read_legacy_settings() -> AppResult<Option<AppSettings>> {
    let registry_path = directories::app_data_dir()?
        .join(".settings")
        .join("registry.json");
    if !registry_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&registry_path)?;

    let Ok(store) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(None);
    };

    let Some(settings_val) = store.get("settings") else {
        return Ok(None);
    };

    #[derive(Deserialize)]
    struct LegacySettingsPatch {
        bepinex_url: Option<String>,
        bepinex_url_x86: Option<String>,
        bepinex_url_x64: Option<String>,
        among_us_path: Option<String>,
        close_on_launch: Option<bool>,
        allow_multi_instance_launch: Option<bool>,
        game_platform: Option<GamePlatform>,
        cache_bepinex: Option<bool>,
        xbox_app_id: Option<String>,
        linux_runner_kind: Option<LinuxRunnerKind>,
        linux_runner_binary: Option<String>,
        linux_wine_prefix: Option<String>,
        linux_proton_compat_data_path: Option<String>,
        linux_proton_steam_client_path: Option<String>,
        linux_proton_use_steam_run: Option<bool>,
    }

    let mut settings = AppSettings::default();
    let Ok(patch) = serde_json::from_value::<LegacySettingsPatch>(settings_val.clone()) else {
        return Ok(None);
    };

    if let Some(value) = patch.bepinex_url_x86 {
        settings.bepinex_url_x86 = value;
    }
    if let Some(value) = patch.bepinex_url_x64 {
        settings.bepinex_url_x64 = value;
    }
    if let Some(value) = patch.bepinex_url {
        settings.bepinex_url_x86 = value.clone();
        settings.bepinex_url_x64 = value.replace("win-x86-", "win-x64-");
    }
    if let Some(value) = patch.among_us_path {
        settings.among_us_path = value;
    }
    if let Some(value) = patch.close_on_launch {
        settings.close_on_launch = value;
    }
    if let Some(value) = patch.allow_multi_instance_launch {
        settings.allow_multi_instance_launch = value;
    }
    if let Some(value) = patch.game_platform {
        settings.game_platform = value;
    }
    if let Some(value) = patch.cache_bepinex {
        settings.cache_bepinex = value;
    }
    if let Some(value) = patch.xbox_app_id {
        settings.xbox_app_id = Some(value);
    }
    if let Some(value) = patch.linux_runner_kind {
        settings.linux_runner_kind = value;
    }
    if let Some(value) = patch.linux_runner_binary {
        settings.linux_runner_binary = value;
    }
    if let Some(value) = patch.linux_wine_prefix {
        settings.linux_wine_prefix = value;
    }
    if let Some(value) = patch.linux_proton_compat_data_path {
        settings.linux_proton_compat_data_path = value;
    }
    if let Some(value) = patch.linux_proton_steam_client_path {
        settings.linux_proton_steam_client_path = value;
    }
    if let Some(value) = patch.linux_proton_use_steam_run {
        settings.linux_proton_use_steam_run = value;
    }

    Ok(Some(settings))
}

pub fn get_settings() -> AppResult<AppSettings> {
    let path = settings_path()?;
    if path.exists() {
        let raw = fs::read_to_string(&path)?;
        match serde_json::from_str::<AppSettings>(&raw) {
            Ok(settings) => return Ok(settings),
            Err(error) => {
                log::warn!(
                    "Failed to parse settings file at '{}': {error}. Falling back to migration/default settings.",
                    path.display()
                );
            }
        }
    }

    if let Some(legacy_settings) = read_legacy_settings()? {
        write_settings_to_file(&path, &legacy_settings)?;
        return Ok(legacy_settings);
    }

    Ok(AppSettings::default())
}

pub fn update_settings(patch: AppSettingsPatch) -> AppResult<AppSettings> {
    let mut settings = get_settings()?;
    let enabling_multi_instance = patch.allow_multi_instance_launch == Some(true);
    let changing_among_us_path = patch.among_us_path.is_some();

    if let Some(value) = patch.bepinex_url_x86 {
        settings.bepinex_url_x86 = value;
    }
    if let Some(value) = patch.bepinex_url_x64 {
        settings.bepinex_url_x64 = value;
    }
    if let Some(value) = patch.among_us_path {
        settings.among_us_path = value;
    }
    if let Some(value) = patch.close_on_launch {
        settings.close_on_launch = value;
    }
    if let Some(value) = patch.allow_multi_instance_launch {
        settings.allow_multi_instance_launch = value;
    }
    if let Some(value) = patch.game_platform {
        settings.game_platform = value;
    }
    if let Some(value) = patch.cache_bepinex {
        settings.cache_bepinex = value;
    }
    if let Some(value) = patch.xbox_app_id {
        settings.xbox_app_id = value;
    }
    if let Some(value) = patch.linux_runner_kind {
        settings.linux_runner_kind = value;
    }
    if let Some(value) = patch.linux_runner_binary {
        settings.linux_runner_binary = value;
    }
    if let Some(value) = patch.linux_wine_prefix {
        settings.linux_wine_prefix = value;
    }
    if let Some(value) = patch.linux_wine_region_info_path {
        settings.linux_wine_region_info_path = value;
    }
    if let Some(value) = patch.linux_proton_compat_data_path {
        settings.linux_proton_compat_data_path = value;
    }
    if let Some(value) = patch.linux_proton_steam_client_path {
        settings.linux_proton_steam_client_path = value;
    }
    if let Some(value) = patch.linux_proton_use_steam_run {
        settings.linux_proton_use_steam_run = value;
    }
    if let Some(value) = patch.theme_name {
        settings.theme_name = value;
    }
    if let Some(value) = patch.show_stars_background {
        settings.show_stars_background = value;
    }
    if let Some(value) = patch.sidebar_width {
        settings.sidebar_width = value;
    }

    // Unity refuses to start a second process when this boot.config entry is
    // present. Remove it when multi-instance launching is enabled so the
    // setting works for the user's actual game installation rather than only
    // changing Starlight's launch behavior.
    if settings.allow_multi_instance_launch && (enabling_multi_instance || changing_among_us_path) {
        remove_single_instance_from_boot_config(&settings.among_us_path)?;
    }

    let path = settings_path()?;
    write_settings_to_file(&path, &settings)?;

    Ok(settings)
}

/// Remove Unity's single-process setting from the configured Among Us
/// installation. A missing game/config file is harmless because users can
/// enable the setting before configuring or installing the game.
pub fn remove_single_instance_from_boot_config(among_us_path: &str) -> AppResult<bool> {
    let boot_config = Path::new(among_us_path)
        .join("Among Us_Data")
        .join(BOOT_CONFIG_FILE_NAME);
    if !boot_config.exists() {
        return Ok(false);
    }

    let contents = fs::read_to_string(&boot_config)?;
    let Some(updated) = remove_single_instance_line(&contents) else {
        return Ok(false);
    };

    fs::write(boot_config, updated)?;
    Ok(true)
}

fn remove_single_instance_line(contents: &str) -> Option<String> {
    let mut removed = false;
    let mut updated = String::with_capacity(contents.len());

    // split_inclusive keeps the original LF/CRLF endings and whether the file
    // ended with a newline, avoiding unrelated boot.config changes.
    for segment in contents.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim_start().starts_with(SINGLE_INSTANCE_KEY) {
            removed = true;
        } else {
            updated.push_str(segment);
        }
    }

    removed.then_some(updated)
}

pub fn get_bepinex_cache_path(architecture: &str) -> AppResult<String> {
    Ok(directories::app_data_dir()?
        .join("cache")
        .join(format!("bepinex-{architecture}.zip"))
        .to_string_lossy()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_settings_to_file_round_trips_and_leaves_no_temp_file() {
        let path = std::env::temp_dir().join(format!(
            "starlight-settings-test-{}.json",
            std::process::id()
        ));

        write_settings_to_file(&path, &AppSettings::default()).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        serde_json::from_str::<AppSettings>(&raw).unwrap();
        assert!(!path.with_extension("json.tmp").exists());

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn remove_single_instance_line_preserves_other_boot_config_content() {
        // Verbatim shape of a real Among Us boot.config: hyphenated keys, and
        // single-instance carries no value.
        let contents = "gc-max-time-slice=3\nsingle-instance=\nbuild-guid=abc\n";

        assert_eq!(
            remove_single_instance_line(contents),
            Some("gc-max-time-slice=3\nbuild-guid=abc\n".to_string())
        );
        assert_eq!(
            remove_single_instance_line("build-guid=abc\r\nsingle-instance=\r\nfoo=bar\r\n"),
            Some("build-guid=abc\r\nfoo=bar\r\n".to_string())
        );
        assert_eq!(
            remove_single_instance_line("build-guid=abc\nfoo=bar\n"),
            None
        );
    }
}
