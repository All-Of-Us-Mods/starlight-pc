//! App-specific icons and the asset source that embeds them.
//!
//! Use `gpui_component::IconName` first; only reach for `AppIcon` when no
//! built-in icon fits (sidebar nav glyphs, etc.). `AppIcon` implements
//! [`gpui_component::IconNamed`], so it's interchangeable with the
//! built-in `IconName` anywhere `impl Into<Icon>` is accepted.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_component::IconNamed;
use gpui_component_assets::Assets as ComponentAssets;

#[derive(Clone, Copy)]
pub enum AppIcon {
    Home,
    Compass,
    Library,
    Download,
    Pencil,
    Starlight,
}

impl IconNamed for AppIcon {
    fn path(self) -> SharedString {
        match self {
            AppIcon::Home => "icons/home.svg",
            AppIcon::Compass => "icons/compass.svg",
            AppIcon::Library => "icons/library.svg",
            AppIcon::Download => "icons/download.svg",
            AppIcon::Pencil => "icons/pencil.svg",
            AppIcon::Starlight => "icons/starlight.svg",
        }
        .into()
    }
}

#[derive(Clone)]
pub struct EmbeddedAssets;

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/home.svg" => Some(include_bytes!("../../assets/icons/home.svg")),
            "icons/compass.svg" => Some(include_bytes!("../../assets/icons/compass.svg")),
            "icons/library.svg" => Some(include_bytes!("../../assets/icons/library.svg")),
            "icons/download.svg" => Some(include_bytes!("../../assets/icons/download.svg")),
            "icons/pencil.svg" => Some(include_bytes!("../../assets/icons/pencil.svg")),
            "icons/starlight.svg" => Some(include_bytes!("../../assets/icons/starlight.svg")),
            _ => None,
        };
        if let Some(b) = bytes {
            return Ok(Some(Cow::Borrowed(b)));
        }
        ComponentAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        ComponentAssets.list(path)
    }
}
