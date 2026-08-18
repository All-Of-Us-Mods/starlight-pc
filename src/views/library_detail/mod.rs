//! Profile detail page. Composes the launch / metadata controls with the
//! reusable [`LogPanel`] (in `ui/log_panel`) and the icon picker dialog (in
//! `icon_dialog`). Long-running work — disk reads, API lookups, launch —
//! always happens on the background executor; this module only orchestrates.

mod icon_dialog;

use gpui::*;
use log::warn;

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::backend::api;
use crate::backend::events::{self, BackendEvent};
use crate::backend::services::bepinex_service::{BepInExProgress, BepInExTargetType};
use crate::backend::services::launch_service;
use crate::backend::services::profile_service::{self, ProfileEntry, ProfileModEntry, ZipOp};
#[cfg(windows)]
use crate::backend::services::profile_shortcut_service;
use crate::backend::state::game_runtime;
use crate::backend::state::mod_catalog_cache;
use crate::settings as app_settings;
use crate::theme::ThemeExt;
use crate::ui::format;
use crate::ui::icon::AppIcon;
use crate::ui::log_panel::LogPanel;
use crate::ui::profile_icon::profile_icon;
use crate::views::page_root;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::progress::Progress;
use gpui_component::skeleton::Skeleton;
use gpui_component::switch::Switch;
use gpui_component::{Disableable, Icon, IconName, Sizable};

use icon_dialog::{IconDialogState, render_icon_dialog};

#[derive(Clone, Debug)]
pub enum LibraryDetailEvent {
    Close,
    /// The user clicked "Install Mods" — navigate to the Explore page.
    OpenExplore,
}

impl EventEmitter<LibraryDetailEvent> for LibraryDetailView {}

pub struct LibraryDetailView {
    pub(super) profile_id: String,
    pub(super) state: LoadState,
    bep_progress: Option<BepInExProgress>,
    confirming_delete: bool,
    /// Mod id awaiting delete confirmation, if any (two-step like the profile
    /// delete, but per row).
    confirming_delete_mod: Option<String>,
    launch_error: Option<String>,
    /// Success/info message (e.g. "Exported profile to …"); rendered non-red.
    notice: Option<String>,
    rename_dialog: Option<Entity<InputState>>,
    /// 0–100 while an export is running; `None` otherwise.
    export_progress: Option<f64>,
    pub(super) icon_dialog: Option<IconDialogState>,
    running_count: usize,
    stoppable_count: usize,
    /// Launches the user has requested but that haven't shown up in a backend
    /// GameStateChanged yet (launches are serialized, and one that needs its
    /// own copy of the profile spends a moment preparing it). Added on top of
    /// the backend count so the UI reflects the click immediately.
    pending_launches: usize,
    log_panel: Entity<LogPanel>,
    /// API-resolved display names per mod_id, populated lazily after load.
    mod_names: HashMap<String, String>,
}

pub(super) enum LoadState {
    Loading,
    Loaded(ProfileEntry),
    NotFound,
    Failed(String),
}

impl LibraryDetailView {
    pub fn new(profile_id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let log_panel = cx.new(|cx| LogPanel::new(window, cx));

        let view = Self {
            profile_id: profile_id.clone(),
            state: LoadState::Loading,
            bep_progress: None,
            confirming_delete: false,
            confirming_delete_mod: None,
            launch_error: None,
            notice: None,
            rename_dialog: None,
            export_progress: None,
            icon_dialog: None,
            running_count: 0,
            stoppable_count: 0,
            pending_launches: 0,
            log_panel,
            mod_names: mod_catalog_cache::cached_names(),
        };

        view.spawn_load(cx);

        // Reload when the window regains focus, so a DLL dropped into
        // BepInEx/plugins via the file manager shows up without leaving and
        // reopening the page.
        cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.spawn_load(cx);
            }
        })
        .detach();

        // Subscribe to backend events for *this* profile.
        let id_for_events = profile_id.clone();
        let mut rx = events::subscribe();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                match event {
                    BackendEvent::BepInExProgress(p)
                        if matches!(p.target_type, BepInExTargetType::Profile)
                            && p.target_id == id_for_events =>
                    {
                        let done = p.stage == "complete";
                        let _ = this.update(cx, |this, cx| {
                            this.bep_progress = if done { None } else { Some(p) };
                            cx.notify();
                        });
                        if done {
                            let _ = this.update(cx, |this, cx| this.spawn_load(cx));
                        }
                    }
                    BackendEvent::GameStateChanged(payload) => {
                        let running = payload
                            .profile_instance_counts
                            .get(&id_for_events)
                            .copied()
                            .unwrap_or(0);
                        let stoppable = payload
                            .stoppable_profile_instance_counts
                            .get(&id_for_events)
                            .copied()
                            .unwrap_or(0);
                        let _ = this.update(cx, |this, cx| {
                            // A real instance appearing settles one pending launch.
                            if running > this.running_count {
                                this.pending_launches = this
                                    .pending_launches
                                    .saturating_sub(running - this.running_count);
                            }
                            this.running_count = running;
                            this.stoppable_count = stoppable;
                            cx.notify();
                            // Game state change ≈ new log content / mod changes.
                            this.refresh_disk_state(cx);
                        });
                    }
                    BackendEvent::ProfileStatsUpdated(id) if id == id_for_events => {
                        let _ = this.update(cx, |this, cx| this.spawn_load(cx));
                    }
                    BackendEvent::ZipProgress(p) if matches!(p.op, ZipOp::Export) => {
                        let _ = this.update(cx, |this, cx| {
                            this.export_progress = Some(p.progress);
                            cx.notify();
                        });
                    }
                    _ => {}
                }
            }
        })
        .detach();

        view
    }

    /// Title shown in the app title bar — the profile name once loaded.
    pub fn title(&self) -> SharedString {
        match &self.state {
            LoadState::Loaded(profile) => profile.name.clone().into(),
            _ => "Profile".into(),
        }
    }

    pub(super) fn spawn_load(&self, cx: &mut Context<Self>) {
        let id = self.profile_id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { profile_service::get_profile_by_id(&id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(Some(p)) => LoadState::Loaded(p),
                    Ok(None) => LoadState::NotFound,
                    Err(e) => LoadState::Failed(e.to_string()),
                };
                cx.notify();
                this.refresh_disk_state(cx);
                this.fetch_missing_mod_names(cx);
            });
        })
        .detach();
    }

    /// Resolve API display names for any mods we haven't seen yet. Successful
    /// lookups are shared across detail-page instances for the app session.
    fn fetch_missing_mod_names(&self, cx: &mut Context<Self>) {
        let LoadState::Loaded(profile) = &self.state else {
            return;
        };
        let cached = mod_catalog_cache::cached_names();
        let pending: Vec<String> = profile
            .mods
            .iter()
            // Custom mods aren't in the catalog — don't look them up.
            .filter(|m| !m.is_custom())
            .map(|m| m.mod_id.clone())
            .filter(|id| !cached.contains_key(id) && !self.mod_names.contains_key(id))
            .collect();
        if !cached.is_empty() {
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.mod_names.extend(cached);
                    cx.notify();
                });
            })
            .detach();
        }
        if pending.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let tasks: Vec<_> = pending
                .into_iter()
                .map(|mod_id| {
                    let id_for_fetch = mod_id.clone();
                    let task = cx.background_executor().spawn(async move {
                        mod_catalog_cache::fetch(&id_for_fetch).map(|m| m.name)
                    });
                    (mod_id, task)
                })
                .collect();
            for (mod_id, task) in tasks {
                if let Some(name) = task.await {
                    let _ = this.update(cx, |this, cx| {
                        this.mod_names.insert(mod_id, name);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Reload the on-disk log shown on this page. Cheap to call repeatedly —
    /// runs on the background executor and pushes the new content into the
    /// `LogPanel` entity, so we never touch the disk inside `render()`.
    fn refresh_disk_state(&self, cx: &mut Context<Self>) {
        let LoadState::Loaded(profile) = &self.state else {
            return;
        };
        let path = profile.path.clone();
        let log_panel = self.log_panel.clone();
        cx.spawn(async move |_this, cx| {
            let log = cx
                .background_executor()
                .spawn(async move { profile_service::get_profile_log(&path, "LogOutput.log") })
                .await;
            log_panel.update(cx, |panel, cx| {
                panel.set_content(log, cx);
            });
        })
        .detach();
    }

    fn install_bepinex(&mut self, cx: &mut Context<Self>) {
        let id = self.profile_id.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = profile_service::install_bepinex_for_profile(&id) {
                    warn!("install_bepinex_for_profile failed: {e}");
                }
            })
            .detach();
    }

    fn toggle_mod(&mut self, mod_id: String, enabled: bool, cx: &mut Context<Self>) {
        // Optimistic UI update; reverted by a reload if the on-disk op fails.
        if let LoadState::Loaded(profile) = &mut self.state
            && let Some(entry) = profile.mods.iter_mut().find(|m| m.mod_id == mod_id)
        {
            entry.enabled = enabled;
        }
        cx.notify();

        let profile_id = self.profile_id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { profile_service::set_mod_enabled(&profile_id, &mod_id, enabled) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("toggle mod failed: {e}");
                    this.launch_error = Some(format!("Toggle failed: {e}"));
                    this.spawn_load(cx);
                }
            });
        })
        .detach();
    }

    fn delete_mod(&mut self, mod_id: String, cx: &mut Context<Self>) {
        self.confirming_delete_mod = None;
        // Optimistically drop the row; the reload afterwards confirms it (or
        // brings it back if the on-disk op failed).
        if let LoadState::Loaded(profile) = &mut self.state {
            profile.mods.retain(|m| m.mod_id != mod_id);
        }
        cx.notify();

        let profile_id = self.profile_id.clone();
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        profile_service::uninstall_mod_from_profile(&profile_id, &mod_id)
                    })
                    .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("delete mod failed: {e}");
                    this.launch_error = Some(format!("Remove mod failed: {e}"));
                }
                this.spawn_load(cx);
            });
        })
        .detach();
    }

    fn open_profile_folder(&self) {
        let LoadState::Loaded(profile) = &self.state else {
            return;
        };
        if let Err(e) = open_folder(Path::new(&profile.path)) {
            warn!("open profile folder failed: {e}");
        }
    }

    fn launch(&mut self, cx: &mut Context<Self>) {
        let LoadState::Loaded(profile) = &self.state else {
            return;
        };
        let profile = profile.clone();
        self.launch_error = None;
        self.pending_launches += 1;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { launch_service::launch_modded_for_profile(profile) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("launch failed: {e}");
                    // No instance will appear for this one — undo the optimistic bump.
                    this.pending_launches = this.pending_launches.saturating_sub(1);
                    this.launch_error = Some(e.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let id = self.profile_id.clone();
        self.launch_error = None;
        // Drop any launches still waiting to be prepared, both in the UI and
        // in the backend so they abort instead of spawning.
        self.pending_launches = 0;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    launch_service::cancel_pending_launches(&id);
                    game_runtime::stop_profile_instances(&id)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("stop failed: {e}");
                    this.launch_error = Some(e.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn delete_profile(&mut self, cx: &mut Context<Self>) {
        let id = self.profile_id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { profile_service::delete_profile(&id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("delete_profile failed: {e}");
                    this.confirming_delete = false;
                    cx.notify();
                } else {
                    cx.emit(LibraryDetailEvent::Close);
                }
            });
        })
        .detach();
    }

    fn open_rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = match &self.state {
            LoadState::Loaded(profile) => profile.name.clone(),
            _ => String::new(),
        };
        let state = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("Profile name");
            s.set_value(name, window, cx);
            s
        });
        state.read(cx).focus_handle(cx).focus(window, cx);
        cx.subscribe_in(
            &state,
            window,
            |this, state, event: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_rename(state.read(cx).value().to_string(), cx);
                }
            },
        )
        .detach();
        self.rename_dialog = Some(state);
        cx.notify();
    }

    fn submit_rename(&mut self, name: String, cx: &mut Context<Self>) {
        let id = self.profile_id.clone();
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.rename_dialog = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { profile_service::rename_profile(&id, &name) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.launch_error = Some(format!("Rename failed: {e}"));
                    cx.notify();
                }
                this.spawn_load(cx);
            });
        })
        .detach();
    }

    /// Open the native file picker and copy the chosen plugin .dll(s) into
    /// this profile's `BepInEx/plugins`. The reload afterwards surfaces them
    /// as custom mod entries.
    fn add_custom_mods(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add BepInEx plugin (.dll)".into()),
        });
        let profile_id = self.profile_id.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut added = Vec::new();
                    for path in paths {
                        added.push(profile_service::import_mod_to_profile(
                            &profile_id,
                            &path.to_string_lossy(),
                        )?);
                    }
                    Ok::<_, crate::backend::error::AppError>(added)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(added) => {
                        this.notice = Some(format!("Added {}", added.join(", ")));
                        this.launch_error = None;
                    }
                    Err(e) => {
                        this.launch_error = Some(format!("Add mod failed: {e}"));
                        this.notice = None;
                    }
                }
                cx.notify();
                this.spawn_load(cx);
            });
        })
        .detach();
    }

    /// Write a `.url` shortcut for this profile onto the desktop. It opens the
    /// app via the `starlight://` scheme, which auto-launches the profile.
    #[cfg(windows)]
    fn create_desktop_shortcut(&mut self, cx: &mut Context<Self>) {
        let id = self.profile_id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { profile_shortcut_service::create_desktop_shortcut(&id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(path) => {
                        this.notice = Some(format!("Created shortcut at {path}"));
                        this.launch_error = None;
                    }
                    Err(e) => {
                        this.launch_error = Some(format!("Shortcut failed: {e}"));
                        this.notice = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Open the native save dialog and export this profile to the chosen .zip.
    fn export_profile(&mut self, cx: &mut Context<Self>) {
        let LoadState::Loaded(profile) = &self.state else {
            return;
        };
        let id = self.profile_id.clone();
        let suggested = format!("{}.zip", profile.name);
        let receiver = cx.prompt_for_new_path(&default_export_dir(), Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = receiver.await else {
                return;
            };
            let dest = path.to_string_lossy().into_owned();
            let dest_for_task = dest.clone();
            let _ = this.update(cx, |this, cx| {
                this.export_progress = Some(0.0);
                this.notice = None;
                this.launch_error = None;
                cx.notify();
            });
            let result = cx
                .background_executor()
                .spawn(async move { profile_service::export_profile_zip(&id, &dest_for_task) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.export_progress = None;
                match result {
                    Ok(()) => this.notice = Some(format!("Exported profile to {dest}")),
                    Err(e) => this.launch_error = Some(format!("Export failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// Starting directory for the export save dialog (user home, else cwd).
fn default_export_dir() -> std::path::PathBuf {
    std::env::home_dir().unwrap_or_else(|| ".".into())
}

fn open_folder(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        Command::new("explorer").arg(path).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(path).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(path).spawn()?;
    }
    Ok(())
}

impl Render for LibraryDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let body: AnyElement = match &self.state {
            LoadState::Loading => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(Skeleton::new().w_1_3().h(px(28.0)).rounded_md())
                .child(Skeleton::new().w_2_3().h_4().rounded_md())
                .child(Skeleton::new().w_1_2().h_4().rounded_md())
                .child(Skeleton::new().w_full().h(px(120.0)).rounded_lg())
                .into_any_element(),
            LoadState::NotFound => div()
                .text_color(theme.danger)
                .child("Profile not found")
                .into_any_element(),
            LoadState::Failed(e) => div()
                .text_color(theme.danger)
                .child(format!("Failed: {e}"))
                .into_any_element(),
            LoadState::Loaded(profile) => {
                // Each section is built by a helper so its (large) element
                // temporaries live in that helper's stack frame, not all in
                // render()'s — one flat frame overflows the stack in debug
                // builds on Windows.
                let profile = profile.clone();
                let hero = self.render_hero(&profile, &theme, cx);
                let mods_section = self.render_mods_section(&profile, &theme, cx);
                let danger_zone = self.render_danger_zone(&theme, cx);
                let has_log = self.log_panel.read(cx).has_content();

                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(hero)
                    .child(mods_section)
                    .children(has_log.then(|| self.log_panel.clone().into_any_element()))
                    .child(danger_zone)
                    .into_any_element()
            }
        };

        page_root("library-detail-page", &theme)
            .gap_4()
            .overflow_y_scroll()
            .children(self.export_progress.map(|p| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child(format!("Exporting… {p:.0}%")),
                    )
                    .child(Progress::new("export-progress").value(p as f32))
            }))
            .child(body)
            .children(self.rename_dialog.clone().map(|input| {
                dialog_overlay(
                    input,
                    "Rename Profile",
                    "Save",
                    theme.clone(),
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(input) = this.rename_dialog.clone() {
                            let name = input.read(cx).value().to_string();
                            this.submit_rename(name, cx);
                        }
                    }),
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.rename_dialog = None;
                        cx.notify();
                    }),
                )
            }))
            .children(
                self.icon_dialog
                    .as_ref()
                    .and_then(|s| match &self.state {
                        LoadState::Loaded(profile) => Some((s, profile.clone())),
                        _ => None,
                    })
                    .map(|(state, profile)| {
                        render_icon_dialog(state, &profile, &self.mod_names, theme.clone(), cx)
                    }),
            )
    }
}

impl LibraryDetailView {
    /// Top-right hero action: Install BepInEx until it's present, then
    /// Launch / Stop. `None` while an install is in flight (the progress row
    /// covers that state).
    fn render_primary_controls(
        &self,
        bep_installed: bool,
        installing: bool,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let running = self.running_count + self.pending_launches;
        // Pending launches can be cancelled by Stop, so count them as
        // stoppable too.
        let stoppable = self.stoppable_count + self.pending_launches;
        let allow_multi = app_settings::get(cx).allow_multi_instance_launch;

        if installing {
            None
        } else if !bep_installed {
            Some(
                Button::new("install-bepinex")
                    .primary()
                    .large()
                    .icon(Icon::new(AppIcon::Download))
                    .label("Install BepInEx")
                    .on_click(cx.listener(|this, _, _window, cx| this.install_bepinex(cx)))
                    .into_any_element(),
            )
        } else {
            let mut row = div()
                .flex()
                .gap_2()
                .items_center()
                .justify_end()
                .flex_nowrap();
            if running == 0 {
                row = row.child(
                    Button::new("launch")
                        .success()
                        .large()
                        .icon(Icon::new(IconName::Play))
                        .label("Launch")
                        .on_click(cx.listener(|this, _, _window, cx| this.launch(cx))),
                );
            } else {
                let stop_label = if stoppable > 1 {
                    format!("Stop ({stoppable})")
                } else {
                    "Stop".to_string()
                };
                let mut stop_btn = Button::new("stop")
                    .danger()
                    .large()
                    .icon(Icon::new(IconName::Close))
                    .label(stop_label);
                if stoppable == 0 {
                    // Only UWP instances — can't stop those.
                    stop_btn = stop_btn.disabled(true);
                } else {
                    stop_btn = stop_btn.on_click(cx.listener(|this, _, _window, cx| this.stop(cx)));
                }
                if allow_multi {
                    row = row.child(div().text_sm().text_color(theme.text_muted).child(format!(
                        "{running} instance{} running",
                        if running == 1 { "" } else { "s" }
                    )));
                    row = row.child(
                        Button::new("launch-another")
                            .success()
                            .large()
                            .icon(Icon::new(IconName::Play))
                            .label("Launch another")
                            .on_click(cx.listener(|this, _, _window, cx| this.launch(cx))),
                    );
                }
                row = row.child(stop_btn);
            }
            Some(row.into_any_element())
        }
    }

    /// Quiet toolbar of secondary profile actions shown at the bottom of the
    /// hero card.
    fn render_manage_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        let manage_buttons = div()
            .flex()
            .gap_1()
            .flex_wrap()
            .child(
                Button::new("rename-profile-action")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Replace))
                    .label("Rename")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_rename_dialog(window, cx);
                    })),
            )
            .child(
                Button::new("edit-icon-action")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Palette))
                    .label("Edit Icon")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.open_icon_dialog(cx);
                    })),
            )
            .child(
                Button::new("open-profile-folder-action")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::FolderOpen))
                    .label("Open Folder")
                    .on_click(cx.listener(|this, _, _window, _cx| {
                        this.open_profile_folder();
                    })),
            )
            .child(
                Button::new("export-profile-action")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::HardDrive))
                    .label("Export ZIP")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.export_profile(cx);
                    })),
            );

        #[cfg(windows)]
        let manage_buttons = manage_buttons.child(
            Button::new("create-shortcut-action")
                .ghost()
                .small()
                .icon(Icon::new(IconName::ExternalLink))
                .label("Desktop Shortcut")
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.create_desktop_shortcut(cx);
                })),
        );

        manage_buttons.into_any_element()
    }

    fn render_hero(
        &self,
        profile: &ProfileEntry,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bep_installed = profile.bepinex_installed.is_some();
        let platform = app_settings::get(cx).game_platform;
        let bep_incompatible = profile
            .bepinex_installed
            .is_some_and(|arch| arch != platform.bepinex_arch());
        let installing = self.bep_progress.is_some();
        let primary_controls = self.render_primary_controls(bep_installed, installing, theme, cx);
        let manage_buttons = self.render_manage_buttons(cx);

        let launch_err = self
            .launch_error
            .clone()
            .map(|msg| div().text_sm().text_color(theme.danger).child(msg));

        let notice = self
            .notice
            .clone()
            .map(|msg| div().text_sm().text_color(theme.success).child(msg));

        let progress_row = self.bep_progress.as_ref().map(|p| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.text_muted)
                        .child(format!("{} — {:.0}%", p.message, p.progress)),
                )
                .child(Progress::new("bep-progress").value(p.progress as f32))
        });

        let title_col = div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .truncate()
                    .child(profile.name.clone()),
            )
            .child(div().text_sm().text_color(theme.text_muted).child(format!(
                "{} played",
                format::play_time(profile.total_play_time),
            )))
            .children((!bep_installed).then(|| {
                div()
                    .mt_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(theme.warning)
                    .child(Icon::new(IconName::TriangleAlert).xsmall())
                    .child("BepInEx not installed")
            }))
            .children(bep_incompatible.then(|| {
                div()
                    .mt_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(theme.warning)
                    .child(Icon::new(IconName::TriangleAlert).xsmall())
                    .child(format!(
                        "This BepInEx install is incompatible with {}",
                        platform.display_name()
                    ))
            }));

        div()
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .rounded_lg()
            .bg(theme.sidebar_background)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .flex_wrap()
                    .child(profile_icon(profile, 80.0, theme))
                    .child(title_col)
                    .children(primary_controls.map(|c| div().flex_none().child(c))),
            )
            .children(progress_row)
            .children(launch_err)
            .children(notice)
            // Secondary profile actions live in a quiet toolbar under a
            // divider so the launch action above stays the focal point.
            .child(
                div()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(manage_buttons),
            )
            .into_any_element()
    }

    fn render_mods_section(
        &self,
        profile: &ProfileEntry,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mod_names = self.mod_names.clone();
        {
            let entries: Vec<AnyElement> = profile
                .mods
                .iter()
                .enumerate()
                .map(|(ix, m)| {
                    let display = mod_display_name(m, &mod_names);
                    let is_last = ix + 1 == profile.mods.len();
                    let name_color = if m.enabled {
                        theme.text
                    } else {
                        theme.text_muted
                    };
                    let mod_id = m.mod_id.clone();
                    let enabled = m.enabled;
                    let has_file = m.file.is_some();
                    let confirming_mod_delete = self
                        .confirming_delete_mod
                        .as_deref()
                        .is_some_and(|id| id == m.mod_id);
                    // Custom mods have no catalog entry, so no thumbnail to fetch.
                    let thumbnail: AnyElement = if m.is_custom() {
                        div()
                            .w(px(32.0))
                            .h(px(32.0))
                            .flex_none()
                            .rounded_md()
                            .bg(theme.hover)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.text_muted)
                            .child(Icon::new(IconName::File))
                            .into_any_element()
                    } else {
                        img(api::mod_thumbnail_url(&m.mod_id))
                            .w(px(32.0))
                            .h(px(32.0))
                            .flex_none()
                            .rounded_md()
                            .object_fit(ObjectFit::Cover)
                            .bg(theme.hover)
                            .into_any_element()
                    };
                    let version_label = if m.is_custom() {
                        "Custom".to_string()
                    } else {
                        m.version.clone()
                    };
                    let mut row = div().flex().items_center().gap_3().px_3().py_2().hover({
                        let hover_bg = theme.hover;
                        move |s| s.bg(hover_bg)
                    });
                    if !is_last {
                        row = row.border_b_1().border_color(theme.border);
                    }
                    row.child(thumbnail)
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(name_color)
                                .child(display),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.text_muted)
                                .child(version_label),
                        )
                        // Mods imported without a known filename can't be toggled on disk.
                        .children(has_file.then(|| {
                            let mod_id = mod_id.clone();
                            Switch::new(SharedString::from(format!("mod-toggle-{ix}")))
                                .checked(enabled)
                                .on_click(cx.listener(move |this, checked: &bool, _window, cx| {
                                    this.toggle_mod(mod_id.clone(), *checked, cx)
                                }))
                        }))
                        .child(if confirming_mod_delete {
                            let confirm_id = mod_id.clone();
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "mod-delete-confirm-{ix}"
                                    )))
                                    .danger()
                                    .label("Remove")
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.delete_mod(confirm_id.clone(), cx)
                                    })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "mod-delete-cancel-{ix}"
                                    )))
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.confirming_delete_mod = None;
                                        cx.notify();
                                    })),
                                )
                                .into_any_element()
                        } else {
                            Button::new(SharedString::from(format!("mod-delete-{ix}")))
                                .ghost()
                                .icon(Icon::new(IconName::Delete))
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.confirming_delete_mod = Some(mod_id.clone());
                                    cx.notify();
                                }))
                                .into_any_element()
                        })
                        .into_any_element()
                })
                .collect();
            let list: AnyElement = if entries.is_empty() {
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child("No mods installed. Install from Explore, or add a BepInEx plugin .dll.")
                    .into_any_element()
            } else {
                div().children(entries).into_any_element()
            };
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(section_heading(&format!("Mods · {}", profile.mods.len())))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    Button::new("install-mods")
                                        .icon(Icon::new(AppIcon::Compass))
                                        .label("Install Mods")
                                        .on_click(cx.listener(|_, _, _window, cx| {
                                            cx.emit(LibraryDetailEvent::OpenExplore)
                                        })),
                                )
                                .child(
                                    Button::new("add-custom-mod")
                                        .icon(Icon::new(IconName::Plus))
                                        .label("Add DLL")
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.add_custom_mods(cx)
                                        })),
                                ),
                        ),
                )
                .child(
                    div()
                        .rounded_lg()
                        .bg(theme.sidebar_background)
                        .border_1()
                        .border_color(theme.border)
                        // Clip row hover backgrounds to the rounded corners.
                        .overflow_hidden()
                        .child(list),
                )
                .into_any_element()
        }
    }

    fn render_danger_zone(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let delete_controls: AnyElement = if self.confirming_delete {
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("confirm-delete")
                        .danger()
                        .icon(Icon::new(IconName::Delete))
                        .label("Delete")
                        .on_click(cx.listener(|this, _, _window, cx| this.delete_profile(cx))),
                )
                .child(
                    Button::new("cancel-delete")
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.confirming_delete = false;
                            cx.notify();
                        })),
                )
                .into_any_element()
        } else {
            Button::new("delete-profile")
                .danger()
                .outline()
                .icon(Icon::new(IconName::Delete))
                .label("Delete Profile")
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.confirming_delete = true;
                    cx.notify();
                }))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_heading("Danger zone"))
            .child(
                div()
                    .rounded_lg()
                    .bg(theme.sidebar_background)
                    .border_1()
                    .border_color(theme.danger.alpha(0.45))
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .flex_wrap()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Delete this profile"),
                            )
                            .child(div().text_sm().text_color(theme.text_muted).child(
                                "Removes the profile, its mods and logs from disk. \
                                             This can't be undone.",
                            )),
                    )
                    .child(delete_controls),
            )
            .into_any_element()
    }
}

fn section_heading(text: &str) -> impl IntoElement {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.to_string())
}

/// The label to show for a mod row. Falls back to the on-disk filename when
/// the API name hasn't been resolved (or doesn't exist), and finally to the
/// raw BepInEx GUID if we don't even have a filename.
fn mod_display_name(m: &ProfileModEntry, names: &HashMap<String, String>) -> String {
    if let Some(name) = names.get(&m.mod_id) {
        return name.clone();
    }
    if let Some(file) = m.file.as_deref().filter(|s| !s.is_empty()) {
        return file.to_string();
    }
    m.mod_id.clone()
}

fn dialog_overlay(
    input: Entity<InputState>,
    title: &'static str,
    confirm: &'static str,
    theme: crate::theme::Theme,
    on_confirm: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    crate::views::modal_overlay(
        &theme,
        px(420.0),
        [
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title)
                .into_any_element(),
            Input::new(&input).into_any_element(),
            div()
                .flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("dialog-cancel")
                        .label("Cancel")
                        .on_click(on_cancel),
                )
                .child(
                    Button::new("dialog-confirm")
                        .primary()
                        .label(confirm)
                        .on_click(on_confirm),
                )
                .into_any_element(),
        ],
    )
}
