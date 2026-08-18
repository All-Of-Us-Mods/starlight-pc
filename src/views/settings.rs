use std::rc::Rc;

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    AxisExt as _, Icon, IconName, Sizable as _, WindowExt,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    notification::Notification,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings},
};
use log::warn;

use crate::backend::events::{self, BackendEvent};
#[cfg(unix)]
use crate::backend::services::core_service::LinuxRunnerKind;
use crate::backend::services::{
    bepinex_service::{self, BepInExTargetType},
    core_service::{self, AppSettingsPatch, GamePlatform},
    finder_service,
};
use crate::settings as app_settings;
use crate::theme::ThemeExt;
use crate::ui::icon::AppIcon;

type PathSetter = Rc<dyn Fn(SharedString, &mut App)>;

pub struct SettingsView;

impl SettingsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.observe_global::<app_settings::SettingsGlobal>(|_, cx| cx.notify())
            .detach();

        // Refresh on cache state changes (download / clear).
        let mut rx = events::subscribe();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if let BackendEvent::BepInExProgress(p) = event
                    && matches!(p.target_type, BepInExTargetType::Cache)
                {
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
            }
        })
        .detach();

        Self
    }
}

/// A setting item whose field is rendered under the label instead of beside
/// it. The horizontal layout caps the label at 60% of the row and clips
/// whatever doesn't fit in the rest, so anything wider than a switch or a
/// short dropdown — text inputs, path pickers, button pairs — has to stack.
fn stacked_item(title: impl Into<SharedString>, field: SettingField<SharedString>) -> SettingItem {
    SettingItem::new(title, field).layout(Axis::Vertical)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Build the download/clear row + status description for one BepInEx cache
/// architecture. The cache is sized once here and reused for both the "Clear"
/// button's visibility and the description, instead of stat-ing the file twice.
fn cache_item(arch: &'static str, label: &'static str) -> SettingItem {
    let (present, status): (bool, SharedString) = match core_service::get_bepinex_cache_path(arch) {
        Ok(path) => match bepinex_service::cache_size(&path) {
            Some(size) => (true, format!("Cached · {}", format_bytes(size)).into()),
            None => (false, "Not cached".into()),
        },
        Err(_) => (false, "Cache path unavailable".into()),
    };
    stacked_item(
        label,
        SettingField::render(move |_, _, _| {
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    Button::new(SharedString::from(format!("cache-{arch}")))
                        .icon(Icon::new(AppIcon::Download))
                        .label("Download")
                        .on_click(move |_, window, cx| download_bepinex_cache(arch, window, cx)),
                )
                .when(present, |row| {
                    row.child(
                        Button::new(SharedString::from(format!("clear-{arch}")))
                            .danger()
                            .icon(Icon::new(IconName::Delete))
                            .label("Clear")
                            .on_click(move |_, window, cx| clear_bepinex_cache(arch, window, cx)),
                    )
                })
        }),
    )
    .description(status)
}

// ---------- patch helpers (used by setter closures) ----------

fn patch_among_us_path(value: SharedString, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            among_us_path: Some(value.to_string()),
            ..Default::default()
        },
    );
}

fn patch_close_on_launch(value: bool, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            close_on_launch: Some(value),
            ..Default::default()
        },
    );
}

fn patch_multi_instance(value: bool, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            allow_multi_instance_launch: Some(value),
            ..Default::default()
        },
    );
}

fn patch_cache_bepinex(value: bool, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            cache_bepinex: Some(value),
            ..Default::default()
        },
    );
}

fn patch_platform(value: SharedString, cx: &mut App) {
    let platform = match value.as_ref() {
        "epic" => GamePlatform::Epic,
        "xbox" => GamePlatform::Xbox,
        _ => GamePlatform::Steam,
    };
    app_settings::update(
        cx,
        AppSettingsPatch {
            game_platform: Some(platform),
            ..Default::default()
        },
    );
}

fn patch_theme_name(value: SharedString, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            theme_name: Some(value.to_string()),
            ..Default::default()
        },
    );
    crate::theme::apply(cx, &value);
}

fn patch_show_stars_background(value: bool, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            show_stars_background: Some(value),
            ..Default::default()
        },
    );
    // The stars layer lives in the workspace, which doesn't observe settings.
    cx.refresh_windows();
}

fn patch_bepinex_url_x64(value: SharedString, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            bepinex_url_x64: Some(value.to_string()),
            ..Default::default()
        },
    );
}

fn patch_bepinex_url_x86(value: SharedString, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            bepinex_url_x86: Some(value.to_string()),
            ..Default::default()
        },
    );
}

#[cfg(unix)]
fn patch_linux_runner_kind(value: SharedString, cx: &mut App) {
    let kind = match value.as_ref() {
        "wine" => LinuxRunnerKind::Wine,
        "steam" => LinuxRunnerKind::Steam,
        _ => LinuxRunnerKind::Proton,
    };
    app_settings::update(
        cx,
        AppSettingsPatch {
            linux_runner_kind: Some(kind),
            ..Default::default()
        },
    );
}

#[cfg(unix)]
fn patch_linux_runner_binary(value: SharedString, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            linux_runner_binary: Some(value.to_string()),
            ..Default::default()
        },
    );
}

#[cfg(unix)]
fn patch_linux_wine_prefix(value: SharedString, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            linux_wine_prefix: Some(value.to_string()),
            ..Default::default()
        },
    );
}

#[cfg(unix)]
fn patch_linux_wine_region_info_path(value: SharedString, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            linux_wine_region_info_path: Some(value.to_string()),
            ..Default::default()
        },
    );
}

#[cfg(unix)]
fn patch_linux_proton_compat_data_path(value: SharedString, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            linux_proton_compat_data_path: Some(value.to_string()),
            ..Default::default()
        },
    );
}

#[cfg(unix)]
fn patch_linux_proton_use_steam_run(value: bool, cx: &mut App) {
    app_settings::update(
        cx,
        AppSettingsPatch {
            linux_proton_use_steam_run: Some(value),
            ..Default::default()
        },
    );
}

// ---------- path input field (Input + Browse button, two-way bound) ----------

struct PathFieldState {
    input: Entity<InputState>,
    _sub: Subscription,
}

/// File-path setting field. The input mirrors the global in real time (so an
/// external write like Auto-detect updates the visible text), edits write back
/// through `set`, and the Browse button opens the platform file picker.
fn path_field(
    key: &'static str,
    directories_only: bool,
    get: fn(&App) -> SharedString,
    set: fn(SharedString, &mut App),
) -> SettingField<SharedString> {
    SettingField::render(move |options, window, cx| {
        let value = get(cx);

        let state_key: SharedString = format!(
            "path-field-{}-{}-{}-{}",
            key, options.page_ix, options.group_ix, options.item_ix
        )
        .into();

        let value_for_init = value.clone();
        let state = window.use_keyed_state(state_key, cx, move |window, cx| {
            let input =
                cx.new(|cx| InputState::new(window, cx).default_value(value_for_init.clone()));
            let _sub = cx.subscribe(&input, move |_, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let v = input.read(cx).value();
                    set(v, cx);
                }
            });
            PathFieldState { input, _sub }
        });

        let input_entity = state.read(cx).input.clone();
        if input_entity.read(cx).value() != value {
            let val = value.clone();
            input_entity.update(cx, |s, cx| s.set_value(val, window, cx));
        }

        let prompt: SharedString = if directories_only {
            "Select folder".into()
        } else {
            "Select file".into()
        };
        let button_id: SharedString = format!(
            "path-browse-{}-{}-{}-{}",
            key, options.page_ix, options.group_ix, options.item_ix
        )
        .into();
        let setter: PathSetter = Rc::new(set);

        let input_el = Input::new(&input_entity)
            .with_size(options.size)
            .map(|this| {
                if options.layout.is_horizontal() {
                    this.w_64()
                } else {
                    this.w_full()
                }
            });

        div().flex().gap_2().child(input_el).child(
            Button::new(button_id)
                .icon(Icon::new(IconName::FolderOpen))
                .label("Browse")
                .with_size(options.size)
                .on_click(move |_, window, cx| {
                    let receiver = cx.prompt_for_paths(PathPromptOptions {
                        files: !directories_only,
                        directories: directories_only,
                        multiple: false,
                        prompt: Some(prompt.clone()),
                    });
                    let setter = setter.clone();
                    window
                        .spawn(cx, async move |cx| {
                            let Ok(Ok(Some(paths))) = receiver.await else {
                                return;
                            };
                            let Some(path) = paths.into_iter().next() else {
                                return;
                            };
                            let s: SharedString = path.to_string_lossy().into_owned().into();
                            let _ = cx.update(|_, cx| setter(s, cx));
                        })
                        .detach();
                }),
        )
    })
}

// ---------- action handlers (Detect / Cache / Clear) ----------

#[cfg(unix)]
fn detect_linux_runtime(window: &mut Window, cx: &mut App) {
    let among_us_path = app_settings::get(cx).among_us_path.clone();
    let path_arg = (!among_us_path.trim().is_empty()).then_some(among_us_path);
    match finder_service::detect_linux_runner(path_arg) {
        Ok(detection) => {
            let kind = match detection.runner_kind.as_str() {
                "wine" => LinuxRunnerKind::Wine,
                _ => LinuxRunnerKind::Proton,
            };
            app_settings::update(
                cx,
                AppSettingsPatch {
                    linux_runner_kind: Some(kind),
                    linux_runner_binary: Some(detection.runner_binary.unwrap_or_default()),
                    linux_wine_prefix: Some(detection.wine_prefix.unwrap_or_default()),
                    linux_proton_compat_data_path: Some(
                        detection.proton_compat_data_path.unwrap_or_default(),
                    ),
                    linux_proton_steam_client_path: Some(
                        detection.proton_steam_client_path.unwrap_or_default(),
                    ),
                    linux_proton_use_steam_run: Some(detection.proton_use_steam_run),
                    ..Default::default()
                },
            );
            window.push_notification(Notification::success("Linux runtime detected"), cx);
        }
        Err(e) => {
            warn!("detect_linux_runner failed: {e}");
            window.push_notification(Notification::error(format!("Detection failed: {e}")), cx);
        }
    }
}

fn detect_among_us(window: &mut Window, cx: &mut App) {
    match finder_service::detect_among_us_installation() {
        Ok(Some(path)) => {
            let detected_platform = finder_service::detect_game_store(&path).ok();
            let platform_enum = detected_platform.as_deref().map(|p| match p {
                "epic" => GamePlatform::Epic,
                "xbox" => GamePlatform::Xbox,
                _ => GamePlatform::Steam,
            });
            app_settings::update(
                cx,
                AppSettingsPatch {
                    among_us_path: Some(path.clone()),
                    game_platform: platform_enum,
                    ..Default::default()
                },
            );
            let msg = match detected_platform.as_deref() {
                Some(p) => format!("Among Us ({p}) detected at {path}"),
                None => format!("Among Us detected at {path}"),
            };
            window.push_notification(Notification::success(msg), cx);
        }
        Ok(None) => {
            window.push_notification(
                Notification::warning("Could not auto-detect Among Us installation"),
                cx,
            );
        }
        Err(e) => {
            warn!("detect_among_us failed: {e}");
            window.push_notification(Notification::error(format!("Detection failed: {e}")), cx);
        }
    }
}

fn download_bepinex_cache(arch: &'static str, window: &mut Window, cx: &mut App) {
    let settings = app_settings::get(cx).clone();
    let cache_path = match core_service::get_bepinex_cache_path(arch) {
        Ok(p) => p,
        Err(e) => {
            window.push_notification(
                Notification::error(format!("Cache path resolution failed: {e}")),
                cx,
            );
            return;
        }
    };
    let url = if arch == "x64" {
        settings.bepinex_url_x64
    } else {
        settings.bepinex_url_x86
    };
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                bepinex_service::download_bepinex_to_cache(url, cache_path, arch.to_string())
            })
            .await;
        let _ = window_handle.update(cx, |_, window, cx| match result {
            Ok(()) => window.push_notification(
                Notification::success(format!("Downloaded BepInEx {arch}")),
                cx,
            ),
            Err(e) => {
                warn!("BepInEx cache download ({arch}) failed: {e}");
                window.push_notification(
                    Notification::error(format!("BepInEx {arch} download failed: {e}")),
                    cx,
                );
            }
        });
    })
    .detach();
}

fn clear_bepinex_cache(arch: &'static str, window: &mut Window, cx: &mut App) {
    match core_service::get_bepinex_cache_path(arch) {
        Ok(path) => match bepinex_service::clear_cache(path, arch.to_string()) {
            Ok(()) => window.push_notification(
                Notification::success(format!("Cleared BepInEx {arch} cache")),
                cx,
            ),
            Err(e) => {
                warn!("clear_bepinex_cache failed: {e}");
                window.push_notification(Notification::error(format!("Clear failed: {e}")), cx);
            }
        },
        Err(e) => {
            window.push_notification(Notification::error(format!("Cache path: {e}")), cx);
        }
    }
}

/// Open the app's data directory (settings, profiles, logs) in the platform
/// file manager — the folder support asks users to look in.
fn open_data_folder() {
    let Ok(dir) = crate::backend::directories::app_data_dir() else {
        return;
    };
    open_in_file_manager(&dir);
}

fn open_in_file_manager(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

// ---------- view ----------

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let game_groups = vec![
            SettingGroup::new().title("Installation").items(vec![
                stacked_item(
                    "Among Us path",
                    path_field(
                        "among-us",
                        true,
                        |cx| app_settings::get(cx).among_us_path.clone().into(),
                        patch_among_us_path,
                    ),
                )
                .description("Folder containing Among Us.exe."),
                stacked_item(
                    "Auto-detect",
                    SettingField::render(|_, _, _| {
                        Button::new("detect-among-us")
                            .icon(Icon::new(AppIcon::Compass))
                            .label("Auto-detect Among Us")
                            .on_click(|_, window, cx| detect_among_us(window, cx))
                    }),
                )
                .description("Search known install locations and set the path above."),
            ]),
            SettingGroup::new().title("Platform").items(vec![
                SettingItem::new(
                    "Game platform",
                    SettingField::dropdown(
                        vec![
                            ("steam".into(), "Steam".into()),
                            ("epic".into(), "Epic".into()),
                            ("xbox".into(), "Xbox".into()),
                        ],
                        |cx| match app_settings::get(cx).game_platform {
                            GamePlatform::Steam => "steam".into(),
                            GamePlatform::Epic => "epic".into(),
                            GamePlatform::Xbox => "xbox".into(),
                        },
                        patch_platform,
                    ),
                )
                .description("Which storefront the game was installed from."),
            ]),
        ];
        let game_page = SettingPage::new("Game")
            .default_open(true)
            .groups(game_groups);

        let launch_items = vec![
            SettingItem::new(
                "Close Starlight when launching",
                SettingField::switch(
                    |cx| app_settings::get(cx).close_on_launch,
                    patch_close_on_launch,
                ),
            )
            .description("Quit the app after starting the game."),
            SettingItem::new(
                "Allow multiple instances",
                SettingField::switch(
                    |cx| app_settings::get(cx).allow_multi_instance_launch,
                    patch_multi_instance,
                ),
            )
            .description(
                "Permit launching more than one game window at a time. Extra instances run \
                 from a temporary copy of the profile, so they can start right away without \
                 clashing over BepInEx files.",
            ),
        ];
        let launch_page = SettingPage::new("Launch")
            .group(SettingGroup::new().title("Behavior").items(launch_items));

        let theme_options: Vec<(SharedString, SharedString)> = crate::theme::theme_names(cx)
            .into_iter()
            .map(|name| (name.clone(), name))
            .collect();

        let appearance_page =
            SettingPage::new("Appearance").group(SettingGroup::new().title("Theme").items(vec![
                SettingItem::new(
                    "Theme",
                    SettingField::scrollable_dropdown(
                        theme_options,
                        |cx| app_settings::get(cx).theme_name.clone().into(),
                        patch_theme_name,
                    ),
                )
                .description(
                    "Drop gpui-component theme JSON files into the themes folder to add \
                     your own.",
                ),
                stacked_item(
                    "Themes folder",
                    SettingField::render(|_, _, _| {
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                Button::new("open-themes-folder")
                                    .icon(Icon::new(IconName::FolderOpen))
                                    .label("Open Themes Folder")
                                    .on_click(|_, _, _| {
                                        open_in_file_manager(&crate::theme::themes_dir())
                                    }),
                            )
                            .child(
                                Button::new("browse-themes")
                                    .icon(Icon::new(IconName::ExternalLink))
                                    .label("Browse Themes")
                                    .on_click(|_, _, cx| {
                                        cx.open_url(
                                            "https://github.com/longbridge/gpui-component/tree/main/themes",
                                        )
                                    }),
                            )
                    }),
                )
                .description(
                    "Bundled Starlight themes are rewritten on every launch, copy one to a \
                     new file before editing it.",
                ),
                SettingItem::new(
                    "Floating stars background",
                    SettingField::switch(
                        |cx| app_settings::get(cx).show_stars_background,
                        patch_show_stars_background,
                    ),
                )
                .description("Show the slowly drifting starfield behind pages."),
            ]));

        let bepinex_page = SettingPage::new("BepInEx").groups(vec![
            SettingGroup::new().title("Cache").items(vec![
                SettingItem::new(
                    "Cache BepInEx downloads",
                    SettingField::switch(
                        |cx| app_settings::get(cx).cache_bepinex,
                        patch_cache_bepinex,
                    ),
                )
                .description("Reuse cached archives across profile installs."),
                cache_item("x64", "x64 cache"),
                cache_item("x86", "x86 cache"),
            ]),
            SettingGroup::new()
                .title("Download URLs")
                .description("Override the default release archive locations.")
                .items(vec![
                    stacked_item(
                        "BepInEx x64 URL",
                        SettingField::input(
                            |cx| app_settings::get(cx).bepinex_url_x64.clone().into(),
                            patch_bepinex_url_x64,
                        ),
                    ),
                    stacked_item(
                        "BepInEx x86 URL",
                        SettingField::input(
                            |cx| app_settings::get(cx).bepinex_url_x86.clone().into(),
                            patch_bepinex_url_x86,
                        ),
                    ),
                ]),
        ]);

        #[cfg(unix)]
        let linux_page = {
            let kind = app_settings::get(cx).linux_runner_kind.clone();

            let auto_detect = SettingItem::new(
                "Auto-detect",
                SettingField::render(|_, _, _| {
                    Button::new("detect-linux-runtime")
                        .icon(Icon::new(AppIcon::Compass))
                        .label("Auto-detect Linux runtime")
                        .on_click(|_, window, cx| detect_linux_runtime(window, cx))
                }),
            )
            .description("Probe Steam/Proton + Wine prefixes from the Among Us path.");

            let runner = SettingItem::new(
                "Runner",
                SettingField::dropdown(
                    vec![
                        ("steam".into(), "Steam".into()),
                        ("proton".into(), "Proton".into()),
                        ("wine".into(), "Wine".into()),
                    ],
                    |cx| match app_settings::get(cx).linux_runner_kind {
                        LinuxRunnerKind::Wine => "wine".into(),
                        LinuxRunnerKind::Proton => "proton".into(),
                        LinuxRunnerKind::Steam => "steam".into(),
                    },
                    patch_linux_runner_kind,
                ),
            )
            .description(
                "Steam launches via the Steam client (Steam must be running). For \
                 modded launches, set the game's Steam launch options to \
                 WINEDLLOVERRIDES=\"winhttp=n,b\" %command%.",
            );

            let runner_binary = stacked_item(
                "Runner binary",
                path_field(
                    "linux-runner-binary",
                    false,
                    |cx| app_settings::get(cx).linux_runner_binary.clone().into(),
                    patch_linux_runner_binary,
                ),
            );

            let wine_prefix = stacked_item(
                "Wine prefix",
                path_field(
                    "linux-wine-prefix",
                    true,
                    |cx| app_settings::get(cx).linux_wine_prefix.clone().into(),
                    patch_linux_wine_prefix,
                ),
            );

            let wine_region_info = stacked_item(
                "RegionInfo.json path",
                path_field(
                    "linux-wine-region-info",
                    false,
                    |cx| {
                        app_settings::get(cx)
                            .linux_wine_region_info_path
                            .clone()
                            .into()
                    },
                    patch_linux_wine_region_info_path,
                ),
            )
            .description(
                "Among Us' RegionInfo.json inside the Wine prefix, used for server \
                 management. Leave empty to derive it from the Wine prefix \
                 (drive_c/users/<your user>/AppData/LocalLow/Innersloth/Among Us).",
            );

            let proton_compat = stacked_item(
                "Proton compat data path",
                path_field(
                    "linux-proton-compat",
                    true,
                    |cx| {
                        app_settings::get(cx)
                            .linux_proton_compat_data_path
                            .clone()
                            .into()
                    },
                    patch_linux_proton_compat_data_path,
                ),
            )
            .description(
                "The game's Steam compatdata folder (steamapps/compatdata/945360). \
                 Also used to locate RegionInfo.json for server management.",
            );

            let steam_run = SettingItem::new(
                "Wrap Proton in steam-run",
                SettingField::switch(
                    |cx| app_settings::get(cx).linux_proton_use_steam_run,
                    patch_linux_proton_use_steam_run,
                ),
            )
            .description(
                "Launch Proton via steam-run (NixOS/non-FHS systems). \
                 Disable to run the Proton binary directly.",
            );

            // Only show the fields the selected runner actually uses.
            let items = match kind {
                LinuxRunnerKind::Steam => vec![auto_detect, runner, proton_compat],
                LinuxRunnerKind::Wine => {
                    vec![
                        auto_detect,
                        runner,
                        runner_binary,
                        wine_prefix,
                        wine_region_info,
                    ]
                }
                LinuxRunnerKind::Proton => {
                    vec![auto_detect, runner, runner_binary, proton_compat, steam_run]
                }
            };

            SettingPage::new("Linux runtime").group(
                SettingGroup::new()
                    .title("Runner")
                    .description("Used when launching the game on Linux.")
                    .items(items),
            )
        };

        let about_page =
            SettingPage::new("About").group(SettingGroup::new().items(vec![SettingItem::render(
                |_, _window, cx| {
                    let theme = cx.global::<crate::theme::Theme>().clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(FontWeight::BOLD)
                                        .child("Starlight PC"),
                                )
                                .child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(theme.hover)
                                        .text_xs()
                                        .text_color(theme.text_muted)
                                        .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_sm()
                                .text_color(theme.text_muted)
                                .child("♡ 2026 All Of Us Mods")
                                .child("|")
                                .child(
                                    div()
                                        .id("about-license-link")
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(theme.text))
                                        .child("GNU GPLv3 License")
                                        .on_click(|_, _, cx| {
                                            cx.open_url("https://www.gnu.org/licenses/gpl-3.0.html")
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    Button::new("about-view-source")
                                        .icon(Icon::new(IconName::ExternalLink))
                                        .label("View Source")
                                        .on_click(|_, _, cx| {
                                            cx.open_url(
                                                "https://github.com/All-Of-Us-Mods/Starlight-PC",
                                            )
                                        }),
                                )
                                .child(
                                    Button::new("about-open-data")
                                        .icon(Icon::new(IconName::FolderOpen))
                                        .label("Open Data Folder")
                                        .on_click(|_, _, _| open_data_folder()),
                                ),
                        )
                },
            )]));

        crate::views::page_root("settings-page", &theme)
            .overflow_y_scroll()
            .gap_4()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .child("Settings"),
            )
            .child(
                Settings::new("starlight-settings")
                    .sidebar_width(px(190.0))
                    .pages({
                        #[cfg_attr(not(unix), allow(unused_mut))]
                        let mut pages = vec![game_page, launch_page, appearance_page, bepinex_page];
                        #[cfg(unix)]
                        pages.push(linux_page);
                        pages.push(about_page);
                        pages
                    }),
            )
    }
}
