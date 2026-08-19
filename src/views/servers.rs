use gpui::*;
use log::warn;

use crate::backend::api::{self, Server};
use crate::backend::deeplink::ServerLink;
use crate::backend::error::AppResult;
use crate::backend::services::region_service::{self, RegionInfo};
use crate::theme::ThemeExt;
use crate::views::{page_root, section_label};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::skeleton::Skeleton;
use gpui_component::{Icon, IconName, Sizable};

pub struct ServersView {
    state: LoadState,
    /// Current contents of Among Us' `regionInfo.json`, or `None` if it could
    /// not be read.
    regions: Option<RegionInfo>,
    /// Why the region file couldn't be read, when `regions` is `None`. Kept
    /// because the reason is usually actionable — on Linux, an unset Wine
    /// prefix or Proton compat data path.
    regions_error: Option<String>,
    custom_dialog: Option<CustomServerInput>,
    notice: Option<String>,
    error: Option<String>,
}

/// Inputs for the "add custom server" modal.
struct CustomServerInput {
    name: Entity<InputState>,
    address: Entity<InputState>,
    port: Entity<InputState>,
    dtls: bool,
    error: Option<String>,
}

enum LoadState {
    Loading,
    Loaded(Vec<Server>),
    Failed(String),
}

impl ServersView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::load(cx);
        Self {
            state: LoadState::Loading,
            regions: None,
            regions_error: None,
            custom_dialog: None,
            notice: None,
            error: None,
        }
    }

    /// Fetch the server list and read the current region file together.
    fn load(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let (servers, regions) = cx
                .background_executor()
                .spawn(async { (api::fetch_servers(), region_service::read_region_info()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.state = match servers {
                    Ok(servers) => LoadState::Loaded(servers),
                    Err(e) => LoadState::Failed(e.to_string()),
                };
                this.set_regions(regions);
                cx.notify();
            });
        })
        .detach();
    }

    fn reload_regions(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let regions = cx
                .background_executor()
                .spawn(async { region_service::read_region_info() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.set_regions(regions);
                cx.notify();
            });
        })
        .detach();
    }

    fn set_regions(&mut self, regions: AppResult<RegionInfo>) {
        match regions {
            Ok(info) => {
                self.regions = Some(info);
                self.regions_error = None;
            }
            Err(e) => {
                self.regions = None;
                self.regions_error = Some(e.to_string());
            }
        }
    }

    fn add_server(&mut self, server: Server, cx: &mut Context<Self>) {
        self.error = None;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let name = server.name.clone();
                    region_service::add_server_region(&server).map(|added| (name, added))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((name, true)) => this.notice = Some(format!("Added region \"{name}\"")),
                    Ok((name, false)) => this.notice = Some(format!("\"{name}\" is already added")),
                    Err(e) => {
                        warn!("add region failed: {e}");
                        this.error = Some(e.to_string());
                    }
                }
                this.reload_regions(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Add the server described by a `starlight://servers/add` link. Same path
    /// as the "Add custom server" dialog, minus the dialog.
    pub fn add_from_deep_link(&mut self, link: ServerLink, cx: &mut Context<Self>) {
        // `host` and `editable` have nowhere to live: user servers are stored
        // as Among Us regions (`regionInfo.json`), which has no field for
        // either. Logged so a link author can see they arrived.
        log::info!(
            "deep link adds server \"{}\" ({}:{}) hosted by {} (editable: {})",
            link.name,
            link.address,
            link.port,
            link.host,
            link.editable
        );
        self.error = None;
        self.notice = None;
        self.custom_dialog = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    region_service::add_custom_region(
                        &link.name,
                        &link.address,
                        link.port,
                        link.dtls,
                        link.translate_name,
                    )
                    .map(|added| (link.name, added))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((name, true)) => this.notice = Some(format!("Added region \"{name}\"")),
                    Ok((name, false)) => {
                        this.notice = Some(format!("\"{name}\" is already added"));
                    }
                    Err(e) => {
                        warn!("add region from deep link failed: {e}");
                        this.error = Some(e.to_string());
                    }
                }
                this.reload_regions(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn remove_region(&mut self, name: String, cx: &mut Context<Self>) {
        self.error = None;
        self.notice = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { region_service::remove_region(&name) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("remove region failed: {e}");
                    this.error = Some(e.to_string());
                }
                this.reload_regions(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_custom_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("My Server"));
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("au-eu.example.com"));
        let port = cx.new(|cx| InputState::new(window, cx).default_value("443"));
        name.read(cx).focus_handle(cx).focus(window, cx);
        self.custom_dialog = Some(CustomServerInput {
            name,
            address,
            port,
            dtls: false,
            error: None,
        });
        cx.notify();
    }

    fn submit_custom(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.custom_dialog.as_ref() else {
            return;
        };
        let name = dialog.name.read(cx).value().trim().to_string();
        let address = dialog.address.read(cx).value().trim().to_string();
        let port_text = dialog.port.read(cx).value().trim().to_string();
        let dtls = dialog.dtls;

        if name.is_empty() || address.is_empty() {
            if let Some(d) = self.custom_dialog.as_mut() {
                d.error = Some("Name and address are required.".into());
            }
            cx.notify();
            return;
        }
        let Ok(port) = port_text.parse::<u16>() else {
            if let Some(d) = self.custom_dialog.as_mut() {
                d.error = Some("Port must be a number between 1 and 65535.".into());
            }
            cx.notify();
            return;
        };

        self.custom_dialog = None;
        self.notice = None;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    region_service::add_custom_region(
                        &name,
                        &address,
                        port,
                        dtls,
                        region_service::CUSTOM_TRANSLATE_NAME,
                    )
                    .map(|added| (name, added))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((name, true)) => this.notice = Some(format!("Added region \"{name}\"")),
                    Ok((_, false)) => {
                        this.notice = Some("A region for that address already exists.".into())
                    }
                    Err(e) => {
                        warn!("add custom region failed: {e}");
                        this.error = Some(e.to_string());
                    }
                }
                this.reload_regions(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_custom_dialog(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.custom_dialog.as_ref()?;
        let field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(theme.text_muted).child(label))
                .child(Input::new(input))
                .into_any_element()
        };

        let mut items: Vec<AnyElement> = vec![
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Add custom server")
                .into_any_element(),
            field("Name", &dialog.name),
            field("Address", &dialog.address),
            field("Port", &dialog.port),
            Checkbox::new("custom-dtls")
                .label("Use DTLS")
                .checked(dialog.dtls)
                .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                    if let Some(d) = this.custom_dialog.as_mut() {
                        d.dtls = *checked;
                    }
                    cx.notify();
                }))
                .into_any_element(),
        ];
        if let Some(err) = &dialog.error {
            items.push(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(err.clone())
                    .into_any_element(),
            );
        }
        items.push(
            div()
                .flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("custom-cancel")
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.custom_dialog = None;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("custom-add")
                        .primary()
                        .label("Add")
                        .on_click(cx.listener(|this, _, _window, cx| this.submit_custom(cx))),
                )
                .into_any_element(),
        );

        Some(crate::views::modal_overlay(theme, px(420.0), items).into_any_element())
    }

    fn is_installed(&self, server: &Server) -> bool {
        self.regions.as_ref().is_some_and(|info| {
            info.regions
                .iter()
                .any(|r| region_service::region_has_server(r, &server.address, server.port))
        })
    }

    fn render_installed(&self, theme: &crate::theme::Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(info) = self.regions.as_ref() else {
            return div()
                .text_sm()
                .text_color(theme.text_muted)
                .child(
                    self.regions_error
                        .clone()
                        .unwrap_or_else(|| "Could not read regionInfo.json.".to_string()),
                )
                .into_any_element();
        };

        if info.regions.is_empty() {
            return div()
                .text_sm()
                .text_color(theme.text_muted)
                .child("No regions configured yet. Add one below.")
                .into_any_element();
        }

        let rows = info.regions.iter().enumerate().map(|(ix, region)| {
            let name = region_service::region_name(region).to_string();
            let remove_name = name.clone();
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(theme.sidebar_background)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(name),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-region-{ix}")))
                        .ghost()
                        .xsmall()
                        .danger()
                        .icon(Icon::new(IconName::Delete))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.remove_region(remove_name.clone(), cx)
                        })),
                )
                .into_any_element()
        });

        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(rows)
            .into_any_element()
    }

    fn render_available(&self, theme: &crate::theme::Theme, cx: &mut Context<Self>) -> AnyElement {
        match &self.state {
            LoadState::Loading => div()
                .flex()
                .flex_col()
                .gap_2()
                .children((0..4).map(|_| {
                    Skeleton::new()
                        .w_full()
                        .h(px(56.0))
                        .rounded_lg()
                        .into_any_element()
                }))
                .into_any_element(),
            LoadState::Failed(e) => div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .text_color(theme.danger)
                        .child(format!("Failed to load servers: {e}")),
                )
                .child(
                    Button::new("servers-retry")
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.state = LoadState::Loading;
                            cx.notify();
                            Self::load(cx);
                        })),
                )
                .into_any_element(),
            LoadState::Loaded(servers) if servers.is_empty() => div()
                .text_color(theme.text_muted)
                .child("No servers available.")
                .into_any_element(),
            LoadState::Loaded(servers) => {
                // Hide servers that are already configured (matched on host:port).
                let available: Vec<&Server> =
                    servers.iter().filter(|s| !self.is_installed(s)).collect();
                if available.is_empty() {
                    return div()
                        .text_sm()
                        .text_color(theme.text_muted)
                        .child("All available servers have been added.")
                        .into_any_element();
                }
                let rows = available.into_iter().map(|server| {
                    let server_for_add = server.clone();
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(theme.sidebar_background)
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .truncate()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(server.name.clone()),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.text_muted)
                                        .child(format!(
                                            "by {} · {}:{}",
                                            server.owner, server.address, server.port
                                        )),
                                ),
                        )
                        .child(
                            Button::new(SharedString::from(format!("add-server-{}", server.id)))
                                .primary()
                                .xsmall()
                                .icon(Icon::new(IconName::Plus))
                                .label("Add")
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.add_server(server_for_add.clone(), cx)
                                })),
                        )
                        .into_any_element()
                });
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(rows)
                    .into_any_element()
            }
        }
    }
}

impl Render for ServersView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        page_root("servers-page", &theme)
            .relative()
            .overflow_y_scroll()
            .gap_6()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_2xl().font_weight(FontWeight::BOLD).child("Servers"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child("Add community servers as Among Us regions. Restart the game to pick up changes."),
                    ),
            )
            .children(self.error.clone().map(|message| {
                div()
                    .rounded_md()
                    .bg(rgb(0x7f1d1d))
                    .p_3()
                    .text_color(theme.text)
                    .child(message)
            }))
            .children(
                self.notice
                    .clone()
                    .map(|message| div().text_sm().text_color(theme.success).child(message)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(section_label("Installed regions", &theme))
                            .child(
                                Button::new("add-custom-server")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::Plus))
                                    .label("Add custom")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_custom_dialog(window, cx)
                                    })),
                            ),
                    )
                    .child(self.render_installed(&theme, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_label("Available servers", &theme))
                    .child(self.render_available(&theme, cx)),
            )
            .children(self.render_custom_dialog(&theme, cx))
    }
}
