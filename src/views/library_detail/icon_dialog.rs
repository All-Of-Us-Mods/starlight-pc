//! Profile icon picker dialog. Lives as part of the library-detail page —
//! state hangs off [`LibraryDetailView`], and the dialog is opened on the
//! window's dialog layer while `icon_dialog` is `Some`.

use gpui::*;
use rust_i18n::t;
use gpui_component::alert::Alert;
use gpui_component::avatar::Avatar;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_component::radio::Radio;
use gpui_component::tab::TabBar;
use gpui_component::{Icon, IconName, Sizable as _, WindowExt};

use super::{LibraryDetailView, LoadState};
use crate::backend::api;
use crate::backend::services::profile_service::{self, ProfileIconSelection};
use crate::theme::ThemeExt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IconDialogMode {
    Default,
    Custom,
    Mod,
}

pub struct IconDialogState {
    pub mode: IconDialogMode,
    pub selected_mod_id: Option<String>,
    pub pending_custom: Option<(Vec<u8>, String)>,
    pub error: Option<String>,
}

impl LibraryDetailView {
    pub(super) fn open_icon_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let LoadState::Loaded(profile) = &self.state else {
            return;
        };
        let mode = match profile.icon_mode.as_deref() {
            Some("custom") => IconDialogMode::Custom,
            Some("mod") => IconDialogMode::Mod,
            _ => IconDialogMode::Default,
        };
        // Custom mods have no catalog thumbnail, so they can't be an icon.
        let selected_mod_id = profile.icon_mod_id.clone().or_else(|| {
            profile
                .mods
                .iter()
                .find(|m| !m.is_custom())
                .map(|m| m.mod_id.clone())
        });
        self.icon_dialog = Some(IconDialogState {
            mode,
            selected_mod_id,
            pending_custom: None,
            error: None,
        });
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let view = view.clone();
            if view.read(cx).icon_dialog.is_none() {
                return dialog;
            }
            let body = icon_dialog_body(&view, cx);
            let on_ok = view.clone();
            let on_close = view.clone();
            dialog
                .title(t!("icon.title"))
                .w(px(480.0))
                .child(body)
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("icon-dialog-cancel").label(t!("common.cancel"))),
                        )
                        .child(
                            DialogAction::new()
                                .child(Button::new("icon-dialog-save").primary().label(t!("common.save"))),
                        ),
                )
                // Saving is asynchronous (and can fail validation), so the
                // dialog is dismissed by `save_icon` rather than on click.
                .on_ok(move |_, window, cx| {
                    on_ok.update(cx, |this, cx| this.save_icon(window, cx));
                    false
                })
                .on_close(move |_, _window, cx| {
                    on_close.update(cx, |this, cx| {
                        this.icon_dialog = None;
                        cx.notify();
                    });
                })
        });
        cx.notify();
    }

    pub(super) fn set_icon_mode(&mut self, mode: IconDialogMode, cx: &mut Context<Self>) {
        if let Some(state) = self.icon_dialog.as_mut() {
            state.mode = mode;
            state.error = None;
            if mode == IconDialogMode::Mod
                && state.selected_mod_id.is_none()
                && let LoadState::Loaded(profile) = &self.state
            {
                state.selected_mod_id = profile
                    .mods
                    .iter()
                    .find(|m| !m.is_custom())
                    .map(|m| m.mod_id.clone());
            }
            cx.notify();
        }
    }

    pub(super) fn pick_custom_icon(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("icon.choose_image_prompt").to_string().into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let extension = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| format!(".{}", s.to_lowercase()))
                .unwrap_or_default();
            let read = cx
                .background_executor()
                .spawn(async move { std::fs::read(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(state) = this.icon_dialog.as_mut() {
                    match read {
                        Ok(bytes) if !bytes.is_empty() => {
                            state.pending_custom = Some((bytes, extension));
                            state.error = None;
                        }
                        Ok(_) => state.error = Some(t!("icon.image_empty").to_string()),
                        Err(e) => state.error = Some(t!("icon.image_read_failed", error = e).to_string()),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn save_icon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.icon_dialog.as_ref() else {
            return;
        };
        let window_handle = window.window_handle();
        let selection = match state.mode {
            IconDialogMode::Default => ProfileIconSelection::Default,
            IconDialogMode::Custom => {
                let LoadState::Loaded(profile) = &self.state else {
                    return;
                };
                let has_existing = profile.icon_mode.as_deref() == Some("custom")
                    && profile.custom_icon_extension.is_some();
                match state.pending_custom.clone() {
                    Some((bytes, extension)) => ProfileIconSelection::Custom { bytes, extension },
                    // Keeping the existing image is a no-op save: just close.
                    None if has_existing => {
                        self.icon_dialog = None;
                        window.close_dialog(cx);
                        cx.notify();
                        return;
                    }
                    None => {
                        if let Some(s) = self.icon_dialog.as_mut() {
                            s.error = Some(t!("icon.choose_custom").to_string());
                        }
                        cx.notify();
                        return;
                    }
                }
            }
            IconDialogMode::Mod => {
                let Some(mod_id) = state.selected_mod_id.clone() else {
                    if let Some(s) = self.icon_dialog.as_mut() {
                        s.error = Some(t!("icon.select_mod").to_string());
                    }
                    cx.notify();
                    return;
                };
                ProfileIconSelection::Mod { mod_id }
            }
        };

        let id = self.profile_id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { profile_service::update_profile_icon(&id, selection) })
                .await;
            let saved = result.is_ok();
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.icon_dialog = None;
                    this.spawn_load(cx);
                }
                Err(e) => {
                    if let Some(s) = this.icon_dialog.as_mut() {
                        s.error = Some(t!("icon.update_failed", error = e).to_string());
                    }
                    cx.notify();
                }
            });
            if saved {
                let _ = window_handle.update(cx, |_, window, cx| window.close_dialog(cx));
            }
        })
        .detach();
    }
}

/// Body of the icon dialog: the mode tabs plus the panel for the selected
/// mode. Read back out of the view on every frame, since the dialog lives in
/// the window's dialog layer rather than in this view's element tree.
fn icon_dialog_body(view: &Entity<LibraryDetailView>, cx: &App) -> AnyElement {
    const MODES: [IconDialogMode; 3] = [
        IconDialogMode::Default,
        IconDialogMode::Custom,
        IconDialogMode::Mod,
    ];

    let theme = cx.theme().clone();
    let this = view.read(cx);
    let (Some(state), LoadState::Loaded(profile)) = (this.icon_dialog.as_ref(), &this.state) else {
        return div().into_any_element();
    };
    let mod_names = &this.mod_names;
    let mode = state.mode;

    let on_mode = view.clone();
    let mode_tabs = TabBar::new("icon-mode-tabs")
        .segmented()
        .selected_index(MODES.iter().position(|m| *m == mode).unwrap_or(0))
        .child(t!("icon.tab_default").to_string())
        .child(t!("icon.tab_custom").to_string())
        .child(t!("icon.tab_mod").to_string())
        .on_click(move |ix: &usize, _window, cx| {
            let Some(target) = MODES.get(*ix).copied() else {
                return;
            };
            on_mode.update(cx, |this, cx| this.set_icon_mode(target, cx));
        });

    let body: AnyElement = match mode {
        IconDialogMode::Default => div()
            .text_sm()
            .text_color(theme.text_muted)
            .child(t!("icon.default_desc").to_string())
            .into_any_element(),
        IconDialogMode::Custom => {
            let has_pending = state.pending_custom.is_some();
            let has_existing = profile.icon_mode.as_deref() == Some("custom")
                && profile.custom_icon_extension.is_some();
            let status: AnyElement = if has_pending {
                div()
                    .text_sm()
                    .child(t!("icon.image_ready").to_string())
                    .into_any_element()
            } else if has_existing {
                div()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child(t!("icon.using_existing").to_string())
                    .into_any_element()
            } else {
                div()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child(t!("icon.formats").to_string())
                    .into_any_element()
            };
            let on_pick = view.clone();
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    Button::new("icon-pick-file")
                        .icon(Icon::new(IconName::FolderOpen))
                        .label(if has_pending || has_existing {
                            t!("icon.change_image")
                        } else {
                            t!("icon.choose_image")
                        })
                        .on_click(move |_, window, cx| {
                            on_pick.update(cx, |this, cx| this.pick_custom_icon(window, cx));
                        }),
                )
                .child(status)
                .into_any_element()
        }
        IconDialogMode::Mod => {
            let mods: Vec<String> = profile
                .mods
                .iter()
                // Custom mods have no catalog thumbnail to offer as an icon.
                .filter(|m| !m.is_custom())
                .map(|m| m.mod_id.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if mods.is_empty() {
                div()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child(t!("icon.no_mods").to_string())
                    .into_any_element()
            } else {
                let selected = state.selected_mod_id.clone();
                let items: Vec<AnyElement> = mods
                    .into_iter()
                    .map(|mod_id| {
                        let is_selected = selected.as_deref() == Some(mod_id.as_str());
                        let display_name = mod_names
                            .get(&mod_id)
                            .cloned()
                            .unwrap_or_else(|| mod_id.clone());
                        // The row and the radio inside it are both hit targets,
                        // so a click aimed straight at the radio still selects.
                        let pick = |view: &Entity<LibraryDetailView>, mod_id: &str| {
                            let view = view.clone();
                            let mod_id = mod_id.to_string();
                            move |cx: &mut App| {
                                let mod_id = mod_id.clone();
                                view.update(cx, |this, cx| {
                                    if let Some(s) = this.icon_dialog.as_mut() {
                                        s.selected_mod_id = Some(mod_id);
                                        s.error = None;
                                    }
                                    cx.notify();
                                });
                            }
                        };
                        let on_row_click = pick(view, &mod_id);
                        let on_radio_click = pick(view, &mod_id);
                        div()
                            .id(SharedString::from(format!("icon-mod-{mod_id}")))
                            .flex()
                            .items_center()
                            .gap_2()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(if is_selected {
                                theme.primary
                            } else {
                                theme.border
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.hover))
                            .on_click(move |_, _window, cx| on_row_click(cx))
                            .child(
                                Radio::new(SharedString::from(format!("icon-mod-{mod_id}-radio")))
                                    .checked(is_selected)
                                    .on_click(move |_, _window, cx| on_radio_click(cx)),
                            )
                            .child(
                                Avatar::new()
                                    .with_size(px(36.0))
                                    .rounded_md()
                                    .placeholder(Icon::new(IconName::File))
                                    .src(api::mod_thumbnail_url(&mod_id)),
                            )
                            .child(div().text_sm().truncate().child(display_name))
                            .into_any_element()
                    })
                    .collect();
                div()
                    .id("icon-mod-list")
                    .max_h(px(240.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(items)
                    .into_any_element()
            }
        }
    };

    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(mode_tabs)
        .child(body)
        .children(
            state
                .error
                .clone()
                .map(|msg| Alert::error("icon-dialog-error", msg).small()),
        )
        .into_any_element()
}
