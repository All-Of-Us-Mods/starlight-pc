use gpui::*;

use crate::backend::api::{self, ModResponse};
use crate::theme::Theme;
use crate::ui::icon::AppIcon;
use gpui_component::Icon;
use gpui_component::skeleton::Skeleton;
use rust_i18n::t;

pub const MOD_CARD_HEIGHT: f32 = 160.0;
pub const MOD_CARD_IMAGE_SIZE: f32 = 160.0;

pub fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted.chars().rev().collect()
}

pub fn mod_card(
    id: SharedString,
    m: &ModResponse,
    width: Option<Pixels>,
    theme: &Theme,
) -> Stateful<Div> {
    let mut card = div()
        .id(id)
        .flex()
        .h(px(MOD_CARD_HEIGHT))
        .rounded_lg()
        .overflow_hidden()
        .bg(theme.sidebar_background)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|s| s.border_color(theme.primary))
        .child(
            img(api::mod_thumbnail_url(&m.id))
                .w(px(MOD_CARD_IMAGE_SIZE))
                .h_full()
                .flex_none()
                .object_fit(ObjectFit::Contain)
                .bg(theme.hover),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .p_3()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_base()
                        .font_weight(FontWeight::BOLD)
                        .child(m.name.clone()),
                )
                .child(
                    div()
                        .mt_0p5()
                        .truncate()
                        .text_sm()
                        .text_color(theme.text_muted)
                        .child(t!("common.by_author", author = m.author).to_string()),
                )
                .child(
                    div()
                        .mt_2()
                        .min_w_0()
                        .line_clamp(2)
                        .text_sm()
                        .line_height(px(20.0))
                        .text_color(theme.text_muted)
                        .child(m.description.clone()),
                )
                .child(
                    div()
                        .mt_auto()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(Icon::new(AppIcon::Download).text_color(theme.primary))
                        .child(format_count(m.downloads)),
                ),
        );

    if let Some(width) = width {
        card = card.w(width).flex_shrink_0();
    }

    card
}

/// Placeholder card matching the layout of [`mod_card`], for loading
/// grids and carousels.
pub fn mod_card_skeleton(width: Option<Pixels>, theme: &Theme) -> Div {
    let mut card = div()
        .flex()
        .h(px(MOD_CARD_HEIGHT))
        .rounded_lg()
        .overflow_hidden()
        .bg(theme.sidebar_background)
        .border_1()
        .border_color(theme.border)
        .child(Skeleton::new().w(px(MOD_CARD_IMAGE_SIZE)).h_full())
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .child(Skeleton::new().w_3_4().h_4().rounded_md())
                .child(Skeleton::new().w_1_2().h_3().rounded_md())
                .child(Skeleton::new().w_full().h_3().rounded_md())
                .child(Skeleton::new().w_5_6().h_3().rounded_md())
                .child(
                    div()
                        .mt_auto()
                        .child(Skeleton::new().w(px(80.0)).h_4().rounded_md()),
                ),
        );

    if let Some(width) = width {
        card = card.w(width).flex_shrink_0();
    }

    card
}
