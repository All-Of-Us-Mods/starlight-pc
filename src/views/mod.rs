pub mod explore;
pub mod home;
pub mod library;
pub mod library_detail;
pub mod lobbies;
pub mod mod_detail;
pub mod news_detail;
pub mod servers;
pub mod settings;

use crate::theme::Theme;
use gpui::*;

/// Shared outer container for every top-level page: full-size vertical flex
/// with the app font, base text color/size, and the standard page padding
/// (extra top padding clears the custom title bar). Callers chain on the bits
/// that vary per page — `.overflow_*()`, `.gap_*()`, `.relative()`, children.
pub fn page_root(id: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .size_full()
        .font_family(crate::theme::FONT_FAMILY)
        .text_color(theme.text)
        .text_size(px(14.0))
        .p_8()
        .pt(px(48.0))
}

/// Muted, small-caps heading used above content sections in detail views.
pub fn section_label(text: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_muted)
        .child(text)
}
