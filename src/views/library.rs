use gpui::*;
use log::warn;

use std::path::PathBuf;

use crate::backend::events::{self, BackendEvent};
use crate::backend::services::launch_service;
use crate::backend::services::profile_service::{self, ProfileEntry, ZipOp};
use crate::backend::state::game_runtime;
use crate::settings as app_settings;
use crate::theme::ThemeExt;
use crate::ui::file_drop::{self, DroppedFiles};
use crate::ui::format;
use crate::ui::icon::AppIcon;
use crate::ui::profile_icon::profile_icon;
use gpui_component::alert::Alert;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::progress::Progress;
use gpui_component::skeleton::Skeleton;
use gpui_component::{Disableable, Icon, IconName, WindowExt};

#[derive(Clone, Debug)]
pub enum LibraryEvent {
    Open(String),
    /// The "game path not configured" banner asks to open the Settings tab.
    OpenSettings,
}

impl EventEmitter<LibraryEvent> for LibraryView {}

pub struct LibraryView {
    state: LoadState,
    create_dialog: Option<Entity<InputState>>,
    error: Option<String>,
    running_count: usize,
    stoppable_count: usize,
    /// 0–100 while a profile import is running; `None` otherwise.
    import_progress: Option<f64>,
    /// Bumped per profile-list load. Concurrent work (a mixed drop installs a
    /// plugin and imports an archive at once) refreshes more than once, and the
    /// reads can finish out of order — a load only applies if it's still the
    /// newest one, so an older snapshot can't hide a just-imported profile.
    load_generation: u64,
}

enum LoadState {
    Loading,
    Loaded(Vec<ProfileEntry>),
    Failed(String),
}

impl LibraryView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let initial = game_runtime::current_state();
        let mut view = Self {
            state: LoadState::Loading,
            create_dialog: None,
            error: None,
            running_count: initial.running_count,
            stoppable_count: initial.stoppable_running_count,
            import_progress: None,
            load_generation: 0,
        };
        view.load_profiles(cx);

        // The setup banner reads the settings global — re-render when the
        // path gets configured (auto-detect or manually in Settings).
        cx.observe_global::<crate::settings::SettingsGlobal>(|_, cx| cx.notify())
            .detach();

        let mut rx = events::subscribe();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                match event {
                    BackendEvent::GameStateChanged(payload) => {
                        let _ = this.update(cx, |this, cx| {
                            this.running_count = payload.running_count;
                            this.stoppable_count = payload.stoppable_running_count;
                            cx.notify();
                        });
                    }
                    BackendEvent::ZipProgress(p) if matches!(p.op, ZipOp::Import) => {
                        let _ = this.update(cx, |this, cx| {
                            this.import_progress = Some(p.progress);
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

    /// Fetch profiles on the background executor and replace `state`. They
    /// arrive already sorted (last launched first) from `get_profiles`.
    fn load_profiles(&mut self, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { profile_service::get_profiles() })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A load started after this one has a fresher snapshot; drop
                // ours rather than overwriting it.
                if generation != this.load_generation {
                    return;
                }
                this.state = match result {
                    Ok(profiles) => LoadState::Loaded(profiles),
                    Err(e) => {
                        warn!("Failed to load profiles: {e}");
                        LoadState::Failed(e.to_string())
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.state = LoadState::Loading;
        cx.notify();
        self.load_profiles(cx);
    }

    fn open_create_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = cx.new(|cx| InputState::new(window, cx).placeholder("Profile name"));
        state.read(cx).focus_handle(cx).focus(window, cx);
        cx.subscribe_in(
            &state,
            window,
            |this, state, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let name = state.read(cx).value().to_string();
                    this.submit_create(name, cx);
                    // An empty name is rejected and leaves the state in place;
                    // only a submit that took closes the dialog.
                    if this.create_dialog.is_none() {
                        window.close_dialog(cx);
                    }
                }
            },
        )
        .detach();
        // The input entity is kept here so the dialog builder (which only
        // holds a handle to this view) can read the typed name back out.
        self.create_dialog = Some(state.clone());
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let input = state.clone();
            let on_ok = view.clone();
            let on_close = view.clone();
            dialog
                .title("New Profile")
                .w(px(360.0))
                .child(Input::new(&input))
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new().child(Button::new("cancel-create").label("Cancel")),
                        )
                        .child(
                            DialogAction::new()
                                .child(Button::new("confirm-create").primary().label("Create")),
                        ),
                )
                .on_ok(move |_, _window, cx| {
                    on_ok.update(cx, |this, cx| {
                        if let Some(input) = this.create_dialog.clone() {
                            let name = input.read(cx).value().to_string();
                            this.submit_create(name, cx);
                        }
                        // Stays open while the name is still empty.
                        this.create_dialog.is_none()
                    })
                })
                .on_close(move |_, _window, cx| {
                    on_close.update(cx, |this, cx| {
                        this.create_dialog = None;
                        cx.notify();
                    });
                })
        });
        cx.notify();
    }

    /// Open the native file picker and import the chosen profile .zip.
    fn import_profile(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let paths = paths
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            let _ = this.update(cx, |this, cx| {
                this.error = None;
                this.import_archives(paths, cx);
            });
        })
        .detach();
    }

    /// Import profile .zips, one after another so their progress bars don't
    /// fight over the shared indicator.
    fn import_archives(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        // Deliberately not clearing `error`: a mixed drop sets a warning about
        // the files this import ignores, and it has to survive.
        self.import_progress = Some(0.0);
        cx.notify();
        cx.spawn(async move |this, cx| {
            for path in paths {
                let result = cx
                    .background_executor()
                    .spawn(async move { profile_service::import_profile_zip(&path) })
                    .await;
                if let Err(e) = result {
                    warn!("import profile zip failed: {e}");
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(format!("Import failed: {e}"));
                        cx.notify();
                    });
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.import_progress = None;
                this.refresh(cx);
            });
        })
        .detach();
    }

    /// Copy dropped plugin .dlls into `profile_id`'s `BepInEx/plugins`.
    fn add_plugins_to_profile(
        &mut self,
        profile_id: String,
        paths: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut added = Vec::new();
                    for path in paths {
                        added.push(profile_service::import_mod_to_profile(&profile_id, &path)?);
                    }
                    Ok::<_, crate::backend::error::AppError>(added)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("add dropped mod failed: {e}");
                    this.error = Some(format!("Add mod failed: {e}"));
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    /// Files dropped on the page background: archives import as new profiles,
    /// but a plugin has no profile to go into from here.
    fn on_drop_on_page(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let dropped = DroppedFiles::classify(paths);
        // Whatever the last drop said no longer applies.
        self.error = None;
        if dropped.is_empty() {
            self.error = Some(file_drop::UNSUPPORTED_DROP.to_string());
            cx.notify();
            return;
        }
        if !dropped.plugins.is_empty() {
            self.error = Some("Drop a mod .dll onto a profile to install it".to_string());
        }
        cx.notify();
        self.import_archives(dropped.archives, cx);
    }

    /// Files dropped on one profile's card: plugins go into that profile,
    /// archives still import as new profiles.
    fn on_drop_on_profile(
        &mut self,
        profile_id: String,
        paths: &[PathBuf],
        cx: &mut Context<Self>,
    ) {
        let dropped = DroppedFiles::classify(paths);
        self.error = None;
        if dropped.is_empty() {
            self.error = Some(file_drop::UNSUPPORTED_DROP.to_string());
            cx.notify();
            return;
        }
        self.add_plugins_to_profile(profile_id, dropped.plugins, cx);
        self.import_archives(dropped.archives, cx);
    }

    fn submit_create(&mut self, name: String, cx: &mut Context<Self>) {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.create_dialog = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { profile_service::create_profile(&trimmed) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("Create profile failed: {e}");
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    fn launch_vanilla(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { launch_service::launch_vanilla_from_settings() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("vanilla launch failed: {e}");
                    this.error = Some(format!("Vanilla launch failed: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn stop_all(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { game_runtime::stop_all_tracked_instances() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    warn!("stop all failed: {e}");
                    this.error = Some(format!("Stop failed: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running_count;
        let stoppable = self.stoppable_count;

        let launch_or_stop = if running == 0 {
            Button::new("launch-vanilla")
                .icon(Icon::new(IconName::Play))
                .label("Launch Vanilla")
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.launch_vanilla(cx);
                }))
        } else {
            let label = if running > 1 {
                format!("Stop all ({running})")
            } else {
                "Stop".to_string()
            };
            let mut btn = Button::new("stop-all")
                .danger()
                .icon(Icon::new(IconName::Close))
                .label(label);
            if stoppable == 0 {
                // Only UWP instances tracked — can't stop those from here.
                btn = btn.disabled(true);
            } else {
                btn = btn.on_click(cx.listener(|this, _, _window, cx| {
                    this.stop_all(cx);
                }));
            }
            btn
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .pb_6()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .child("Library"),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(launch_or_stop)
                    .child(
                        Button::new("import-profile")
                            .icon(Icon::new(AppIcon::Download))
                            .label("Import Profile")
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.import_profile(cx);
                            })),
                    )
                    .child(
                        Button::new("create-profile")
                            .primary()
                            .icon(Icon::new(IconName::Plus))
                            .label("Create Profile")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_create_dialog(window, cx);
                            })),
                    ),
            )
    }

    fn render_profile_card(
        &self,
        profile: &ProfileEntry,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = profile.id.clone();
        let emit_id = id.clone();
        let drop_id = id.clone();
        let accent = theme.primary;
        div()
            .id(SharedString::from(id))
            .flex()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(theme.sidebar_background)
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover))
            // Dropping a .dll here installs it into this profile.
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(accent))
            .on_drop(
                cx.listener(move |this, dropped: &ExternalPaths, _window, cx| {
                    this.on_drop_on_profile(drop_id.clone(), dropped.paths(), cx);
                }),
            )
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cx.emit(LibraryEvent::Open(emit_id.clone()));
            }))
            .child(profile_icon(profile, 96.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(profile.name.clone()),
                    )
                    .children(profile.bepinex_installed.is_none().then(|| {
                        div()
                            .text_xs()
                            .text_color(theme.warning)
                            .child("BepInEx not installed")
                    }))
                    .child(div().text_xs().text_color(theme.text_muted).child(format!(
                        "{} mods · {} played",
                        profile.mods.len(),
                        format::play_time(profile.total_play_time),
                    )))
                    .child(div().text_xs().text_color(theme.text_muted).child(format!(
                        "Last launched {}",
                        format::last_launched(profile.last_launched_at)
                    ))),
            )
    }
}

fn profile_card_skeleton(theme: &crate::theme::Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .rounded_lg()
        .bg(theme.sidebar_background)
        .border_1()
        .border_color(theme.border)
        .child(Skeleton::new().w_2_3().h_5().rounded_md())
        .child(Skeleton::new().w_1_2().h_4().rounded_md())
        .child(Skeleton::new().w_5_6().h_3().rounded_md())
        .child(Skeleton::new().w_1_3().h_3().rounded_md())
}

impl Render for LibraryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body: AnyElement = match &self.state {
            LoadState::Loading => {
                let placeholders: Vec<AnyElement> = (0..4)
                    .map(|_| profile_card_skeleton(&theme).into_any_element())
                    .collect();
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_4()
                    .children(placeholders)
                    .into_any_element()
            }
            LoadState::Failed(message) => Alert::error(
                "profiles-load-failed",
                format!("Failed to load profiles: {message}"),
            )
            .into_any_element(),
            LoadState::Loaded(profiles) if profiles.is_empty() => div()
                .text_color(theme.text_muted)
                .child("No profiles yet. Click \"Create Profile\" to make one, or drop an exported profile .zip here.")
                .into_any_element(),
            LoadState::Loaded(profiles) => {
                let cards: Vec<AnyElement> = profiles
                    .iter()
                    .map(|p| self.render_profile_card(p, &theme, cx).into_any_element())
                    .collect();
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_4()
                    .children(cards)
                    .into_any_element()
            }
        };

        // First-run nudge: launching can't work until the game path is set
        // (startup auto-detect may have already filled it in).
        let setup_banner = app_settings::get(cx)
            .among_us_path
            .trim()
            .is_empty()
            .then(|| {
                div()
                    .mb_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Alert::warning(
                            "library-setup-banner",
                            "Among Us path isn't configured — profiles can't launch yet.",
                        )
                        .flex_1(),
                    )
                    .child(
                        Button::new("open-settings-banner")
                            .primary()
                            .label("Open Settings")
                            .on_click(cx.listener(|_, _, _window, cx| {
                                cx.emit(LibraryEvent::OpenSettings);
                            })),
                    )
            });

        crate::views::page_root("library-page", &theme)
            .relative()
            .overflow_y_scroll()
            // Dropping an exported profile .zip anywhere on the page imports it.
            // Cards handle their own drops first (the listener consumes the drag).
            .drag_over::<ExternalPaths>({
                let hover = theme.hover;
                move |style, _, _, _| style.bg(hover)
            })
            .on_drop(cx.listener(|this, dropped: &ExternalPaths, _window, cx| {
                this.on_drop_on_page(dropped.paths(), cx);
            }))
            .child(self.render_header(cx))
            .children(setup_banner)
            .children(self.error.clone().map(|message| {
                Alert::error("library-error", message)
                    .mb_4()
                    .on_close(cx.listener(|this, _, _window, cx| {
                        this.error = None;
                        cx.notify();
                    }))
            }))
            .children(self.import_progress.map(|p| {
                div()
                    .mb_4()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child(format!("Importing… {p:.0}%")),
                    )
                    .child(Progress::new("import-progress").value(p as f32))
            }))
            .child(body)
    }
}
