use gpui::prelude::FluentBuilder as _;
use gpui::*;
use std::path::PathBuf;

use crate::backend::api;
use crate::backend::services::profile_service::ProfileEntry;
use gpui_component::avatar::Avatar;
use gpui_component::{Icon, IconName, Sizable as _};

/// The profile's icon at `size` px: its custom image, the thumbnail of the mod
/// it borrows its icon from, or the default placeholder. Square rather than
/// round, and the placeholder also covers an image that fails to load.
pub fn profile_icon(profile: &ProfileEntry, size: f32) -> AnyElement {
    let source: Option<ImageSource> = match profile.icon_mode.as_deref() {
        Some("custom") => profile
            .custom_icon_extension
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|ext| {
                PathBuf::from(&profile.path)
                    .join(format!("icon{ext}"))
                    .into()
            }),
        Some("mod") => profile
            .icon_mod_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|mod_id| api::mod_thumbnail_url(mod_id).into()),
        _ => None,
    };

    Avatar::new()
        .with_size(px(size))
        .rounded_md()
        .placeholder(Icon::new(IconName::Inbox))
        .when_some(source, |this, source| this.src(source))
        .into_any_element()
}
