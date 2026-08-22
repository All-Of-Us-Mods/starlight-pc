use gpui::*;
use log::warn;
use rust_i18n::t;

use crate::backend::api::{self, Server};
use crate::backend::deeplink::ServerLink;
use crate::backend::error::AppResult;
use crate::backend::services::region_service::{self, RegionInfo};
use crate::theme::ThemeExt;
use crate::ui::icon::AppIcon;
use crate::views::{page_root, section_label};
use gpui_component::alert::Alert;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_component::form::{field, v_form};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::skeleton::Skeleton;
use gpui_component::{Icon, IconName, Sizable, WindowExt};

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

/// Inputs for the add / edit server modal.
struct CustomServerInput {
    name: Entity<InputState>,
    address: Entity<InputState>,
    port: Entity<InputState>,
    dtls: bool,
    error: Option<String>,
    /// Index of the region being edited, or `None` when adding a new one.
    editing: Option<usize>,
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
                    Ok((name, true)) => {
                        this.notice = Some(t!("servers.added", name = name).to_string())
                    }
                    Ok((name, false)) => {
                        this.notice = Some(t!("servers.already_added", name = name).to_string())
                    }
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
    pub fn add_from_deep_link(
        &mut self,
        link: ServerLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        // A link arriving mid-add/edit wins: drop the half-filled dialog.
        if self.custom_dialog.take().is_some() {
            window.close_dialog(cx);
        }
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
                    Ok((name, true)) => {
                        this.notice = Some(t!("servers.added", name = name).to_string())
                    }
                    Ok((name, false)) => {
                        this.notice = Some(t!("servers.already_added", name = name).to_string());
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
        self.open_dialog(None, region_service::RegionFields::default(), window, cx);
    }

    /// Open the same dialog pre-filled with an installed region's current
    /// values; saving replaces that region in place.
    fn open_edit_dialog(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(region) = self
            .regions
            .as_ref()
            .and_then(|info| info.regions.get(index))
        else {
            return;
        };
        let fields = region_service::region_fields(region);
        self.open_dialog(Some(index), fields, window, cx);
    }

    fn open_dialog(
        &mut self,
        editing: Option<usize>,
        fields: region_service::RegionFields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("servers.my_server").to_string())
                .default_value(fields.name)
        });
        let address = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("au-eu.example.com")
                .default_value(fields.address)
        });
        let port = cx.new(|cx| InputState::new(window, cx).default_value(fields.port.to_string()));
        name.read(cx).focus_handle(cx).focus(window, cx);
        // Enter submits from any of the three fields, like the profile dialogs.
        // The subscriptions die with these inputs when the dialog is replaced.
        for input in [&name, &address, &port] {
            cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_custom(cx);
                    // A rejected submit keeps its error on screen.
                    if this.custom_dialog.is_none() {
                        window.close_dialog(cx);
                    }
                }
            })
            .detach();
        }
        self.custom_dialog = Some(CustomServerInput {
            name,
            address,
            port,
            dtls: fields.dtls,
            error: None,
            editing,
        });

        // The dialog lives in the window's dialog layer, above the page and its
        // scroll container. It outlives a single render, so its fields are read
        // back out of the view every frame rather than captured once.
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let view = view.clone();
            let Some(state) = view.read(cx).custom_dialog.as_ref() else {
                return dialog;
            };
            let (name, address, port, dtls, error, editing) = (
                state.name.clone(),
                state.address.clone(),
                state.port.clone(),
                state.dtls,
                state.error.clone(),
                state.editing.is_some(),
            );
            let on_toggle = view.clone();
            let on_ok = view.clone();
            let on_close = view.clone();
            dialog
                .title(if editing {
                    t!("servers.edit_title")
                } else {
                    t!("servers.add_title")
                })
                .w(px(420.0))
                .child(
                    v_form()
                        .child(
                            field()
                                .label(t!("servers.name").to_string())
                                .child(Input::new(&name)),
                        )
                        .child(
                            field()
                                .label(t!("servers.address").to_string())
                                .child(Input::new(&address)),
                        )
                        .child(
                            field()
                                .label(t!("servers.port").to_string())
                                .child(Input::new(&port)),
                        )
                        .child(
                            field().child(
                                Checkbox::new("custom-dtls")
                                    .label(t!("servers.use_dtls").to_string())
                                    .checked(dtls)
                                    .on_click(move |checked: &bool, _window, cx| {
                                        let checked = *checked;
                                        on_toggle.update(cx, |this, cx| {
                                            if let Some(d) = this.custom_dialog.as_mut() {
                                                d.dtls = checked;
                                            }
                                            cx.notify();
                                        });
                                    }),
                            ),
                        ),
                )
                .children(error.map(|e| Alert::error("custom-server-error", e)))
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("custom-cancel").label(t!("common.cancel"))),
                        )
                        .child(DialogAction::new().child(
                            Button::new("custom-save").primary().label(if editing {
                                t!("common.save")
                            } else {
                                t!("servers.add")
                            }),
                        )),
                )
                // A rejected submit keeps the dialog open with its error shown;
                // `custom_dialog` is only cleared once the save went through.
                .on_ok(move |_, _window, cx| {
                    on_ok.update(cx, |this, cx| {
                        this.submit_custom(cx);
                        this.custom_dialog.is_none()
                    })
                })
                .on_close(move |_, _window, cx| {
                    on_close.update(cx, |this, cx| {
                        this.custom_dialog = None;
                        cx.notify();
                    });
                })
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
        let editing = dialog.editing;

        if name.is_empty() || address.is_empty() {
            if let Some(d) = self.custom_dialog.as_mut() {
                d.error = Some(t!("servers.name_address_required").to_string());
            }
            cx.notify();
            return;
        }
        let Ok(port) = port_text.parse::<u16>() else {
            if let Some(d) = self.custom_dialog.as_mut() {
                d.error = Some(t!("servers.port_invalid").to_string());
            }
            cx.notify();
            return;
        };
        // The same rule `update_region` enforces on save, checked here against
        // the loaded region list so a duplicate is caught while the dialog is
        // still open (and the typed address still on screen).
        let clash = self.regions.as_ref().and_then(|info| {
            region_service::conflicting_region_name(info, editing, &address, port)
        });
        if let Some(other) = clash {
            if let Some(d) = self.custom_dialog.as_mut() {
                d.error = Some(t!("servers.conflict", name = other).to_string());
            }
            cx.notify();
            return;
        }

        // Clearing this is what tells the dialog it may close (see `on_ok`).
        self.custom_dialog = None;
        self.notice = None;
        self.error = None;
        cx.notify();

        if let Some(index) = editing {
            self.save_region_edit(index, name, address, port, dtls, cx);
            return;
        }

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
                    Ok((name, true)) => {
                        this.notice = Some(t!("servers.added", name = name).to_string())
                    }
                    Ok((_, false)) => this.notice = Some(t!("servers.address_exists").to_string()),
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

    /// Write an edited region back in place. The index comes from the row the
    /// user opened, so a region file changed underneath us (another edit, an
    /// in-game change) is caught by `update_region`'s bounds check.
    fn save_region_edit(
        &mut self,
        index: usize,
        name: String,
        address: String,
        port: u16,
        dtls: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let fields = region_service::RegionFields {
                        name: name.clone(),
                        address,
                        port,
                        dtls,
                    };
                    region_service::update_region(index, &fields).map(|()| name)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(name) => this.notice = Some(t!("servers.saved", name = name).to_string()),
                    Err(e) => {
                        warn!("update region failed: {e}");
                        this.error = Some(e.to_string());
                    }
                }
                this.reload_regions(cx);
                cx.notify();
            });
        })
        .detach();
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
                        .unwrap_or_else(|| t!("servers.regioninfo_unreadable").to_string()),
                )
                .into_any_element();
        };

        if info.regions.is_empty() {
            return div()
                .text_sm()
                .text_color(theme.text_muted)
                .child(t!("servers.no_regions").to_string())
                .into_any_element();
        }

        let rows = info.regions.iter().enumerate().map(|(ix, region)| {
            let fields = region_service::region_fields(region);
            let remove_name = fields.name.clone();
            let target = format!(
                "{}:{}{}",
                fields.address,
                fields.port,
                if fields.dtls { " · DTLS" } else { "" }
            );
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
                                .child(fields.name),
                        )
                        // The address is what an edit actually changes, so show it.
                        .child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child(target),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("edit-region-{ix}")))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(AppIcon::Pencil))
                        .tooltip(t!("servers.edit_tooltip").to_string())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_edit_dialog(ix, window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-region-{ix}")))
                        .ghost()
                        .xsmall()
                        .danger()
                        .icon(Icon::new(IconName::Delete))
                        .tooltip(t!("servers.remove_tooltip").to_string())
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
                    Alert::error(
                        "servers-load-failed",
                        t!("servers.load_failed", error = e).to_string(),
                    )
                    .flex_1(),
                )
                .child(
                    Button::new("servers-retry")
                        .label(t!("common.retry"))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.state = LoadState::Loading;
                            cx.notify();
                            Self::load(cx);
                        })),
                )
                .into_any_element(),
            LoadState::Loaded(servers) if servers.is_empty() => div()
                .text_color(theme.text_muted)
                .child(t!("servers.none_available").to_string())
                .into_any_element(),
            LoadState::Loaded(servers) => {
                // Hide servers that are already configured (matched on host:port).
                let available: Vec<&Server> =
                    servers.iter().filter(|s| !self.is_installed(s)).collect();
                if available.is_empty() {
                    return div()
                        .text_sm()
                        .text_color(theme.text_muted)
                        .child(t!("servers.all_added").to_string())
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
                                        .child(
                                            t!(
                                                "servers.by_address",
                                                owner = server.owner,
                                                address = server.address,
                                                port = server.port,
                                            )
                                            .to_string(),
                                        ),
                                ),
                        )
                        .child(
                            Button::new(SharedString::from(format!("add-server-{}", server.id)))
                                .primary()
                                .xsmall()
                                .icon(Icon::new(IconName::Plus))
                                .label(t!("servers.add"))
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
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .child(t!("nav.servers")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child(t!("servers.description").to_string()),
                    ),
            )
            .children(self.error.clone().map(|message| {
                Alert::error("servers-error", message).on_close(cx.listener(
                    |this, _, _window, cx| {
                        this.error = None;
                        cx.notify();
                    },
                ))
            }))
            .children(self.notice.clone().map(|message| {
                Alert::success("servers-notice", message).on_close(cx.listener(
                    |this, _, _window, cx| {
                        this.notice = None;
                        cx.notify();
                    },
                ))
            }))
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
                            .child(section_label(t!("servers.installed_regions"), &theme))
                            .child(
                                Button::new("add-custom-server")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::Plus))
                                    .label(t!("servers.add_custom"))
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
                    .child(section_label(t!("servers.available"), &theme))
                    .child(self.render_available(&theme, cx)),
            )
    }
}
