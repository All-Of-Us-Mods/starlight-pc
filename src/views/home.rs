use gpui::*;
use rust_i18n::t;

use crate::backend::api::{self, ModResponse, Post};
use crate::theme::ThemeExt;
use crate::ui::mod_card;
use gpui_component::alert::Alert;
use gpui_component::button::Button;
use gpui_component::skeleton::Skeleton;

#[derive(Clone, Debug)]
pub enum HomeEvent {
    OpenMod(String),
    OpenNews(Post),
}

impl EventEmitter<HomeEvent> for HomeView {}

pub struct HomeView {
    news: Loading<Vec<Post>>,
    trending: Loading<Vec<ModResponse>>,
}

enum Loading<T> {
    Pending,
    Ready(T),
    Failed(String),
}

const CARD_WIDTH: f32 = 420.0;
const NEWS_CARD_WIDTH: f32 = 320.0;

impl HomeView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            news: Loading::Pending,
            trending: Loading::Pending,
        };
        view.fetch(cx);
        view
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        self.news = Loading::Pending;
        self.trending = Loading::Pending;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let news = cx
                .background_executor()
                .spawn(async { api::fetch_news() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.news = match news {
                    Ok(v) => Loading::Ready(v),
                    Err(e) => Loading::Failed(e.to_string()),
                };
                cx.notify();
            });
            let trending = cx
                .background_executor()
                .spawn(async { api::fetch_trending_mods() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.trending = match trending {
                    Ok(v) => Loading::Ready(v),
                    Err(e) => Loading::Failed(e.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }
}

/// Error text plus a Retry button, shared by both sections (retrying reloads
/// the whole page — news and trending come from the same fetch pass).
fn failed_row(id: &'static str, message: &str, cx: &mut Context<HomeView>) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(Alert::error(id, message.to_string()).flex_1())
        .child(
            Button::new(SharedString::from(format!("{id}-button")))
                .label(t!("common.retry"))
                .on_click(cx.listener(|this, _, _window, cx| this.fetch(cx))),
        )
        .into_any_element()
}

fn section_title(text: gpui::SharedString) -> impl IntoElement {
    div()
        .text_lg()
        .font_weight(FontWeight::SEMIBOLD)
        .pb_3()
        .child(text)
}

fn carousel(id: &'static str, items: Vec<AnyElement>) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .gap_3()
        .overflow_x_scroll()
        .pb_2()
        .children(items)
}

fn news_card_skeleton(theme: &crate::theme::Theme) -> impl IntoElement {
    div()
        .w(px(NEWS_CARD_WIDTH))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .rounded_lg()
        .bg(theme.sidebar_background)
        .border_1()
        .border_color(theme.border)
        .child(Skeleton::new().w_2_3().h_4().rounded_md())
        .child(Skeleton::new().w_1_3().h_3().rounded_md())
        .child(Skeleton::new().w_full().h_3().rounded_md())
        .child(Skeleton::new().w_5_6().h_3().rounded_md())
}

fn news_card(post: &Post, theme: &crate::theme::Theme, cx: &mut Context<HomeView>) -> AnyElement {
    let post_for_click = post.clone();
    div()
        .id(SharedString::from(format!("news-{}", post.id)))
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .w(px(NEWS_CARD_WIDTH))
        .flex_shrink_0()
        .rounded_lg()
        .bg(theme.sidebar_background)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|s| s.border_color(theme.primary))
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            cx.emit(HomeEvent::OpenNews(post_for_click.clone()));
        }))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .line_clamp(2)
                .child(post.title.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child(t!("common.by_author", author = post.author).to_string()),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.text_muted)
                .child(post.content.chars().take(140).collect::<String>()),
        )
        .into_any_element()
}

impl HomeView {
    fn mod_card(
        m: &ModResponse,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!("trending-{}", m.id));
        let mod_id_for_click = m.id.clone();
        mod_card::mod_card(id, m, Some(px(CARD_WIDTH)), theme)
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cx.emit(HomeEvent::OpenMod(mod_id_for_click.clone()));
            }))
            .into_any_element()
    }
}

impl Render for HomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let news_body: AnyElement = match &self.news {
            Loading::Pending => div()
                .flex()
                .gap_3()
                .pb_2()
                .children((0..4).map(|_| news_card_skeleton(&theme).into_any_element()))
                .into_any_element(),
            Loading::Failed(e) => failed_row("news-retry", &e.clone(), cx),
            Loading::Ready(items) => carousel(
                "news-carousel",
                items.iter().map(|p| news_card(p, &theme, cx)).collect(),
            )
            .into_any_element(),
        };

        let trending_body: AnyElement = match &self.trending {
            Loading::Pending => div()
                .flex()
                .gap_3()
                .pb_2()
                .children((0..3).map(|_| {
                    mod_card::mod_card_skeleton(Some(px(CARD_WIDTH)), &theme).into_any_element()
                }))
                .into_any_element(),
            Loading::Failed(e) => failed_row("trending-retry", &e.clone(), cx),
            Loading::Ready(items) => carousel(
                "trending-carousel",
                items
                    .iter()
                    .map(|m| Self::mod_card(m, &theme, cx))
                    .collect(),
            )
            .into_any_element(),
        };

        crate::views::page_root("home-page", &theme)
            .overflow_y_scroll()
            .gap_8()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .child(t!("nav.home")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(section_title(t!("home.news").into()))
                    .child(news_body),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(section_title(t!("home.trending").into()))
                    .child(trending_body),
            )
    }
}
