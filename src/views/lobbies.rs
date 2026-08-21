//! Public lobby browser. Polls the optional `/x-api/games` endpoint (see
//! `hpllp013.yaml`) on every region the user has enabled in Among Us'
//! `regionInfo.json`, aggregates the active games, and lets the user copy a
//! join code or launch straight into a lobby — picking an existing profile or
//! a temporary one, with the lobby's required mods installed automatically.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{prelude::FluentBuilder as _, *};
use log::warn;
use rust_i18n::t;

use crate::backend::api::{self, Game, LobbyMod};
use crate::backend::error::{AppError, AppResult};
use crate::backend::events::{self, BackendEvent};
use crate::backend::services::mod_install_service::{self, InstallModInput};
use crate::backend::services::profile_service::{self, ProfileEntry, ProfileModEntry};
use crate::backend::services::{launch_service, region_service};
use crate::backend::state::game_runtime::{self, GameStatePayload};
use crate::backend::state::mod_catalog_cache;
use crate::theme::{Theme, ThemeExt};
use crate::views::{page_root, section_label};
use gpui_component::alert::Alert;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::clipboard::Clipboard;
use gpui_component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_component::radio::Radio;
use gpui_component::skeleton::Skeleton;
use gpui_component::tag::Tag;
use gpui_component::{Disableable, Icon, IconName, Sizable, WindowExt};

/// How often the lobby list re-polls every enabled region.
const REFRESH_INTERVAL_SECS: u64 = 12;

pub struct LobbiesView {
    state: LoadState,
    /// Profiles offered in the launch dialog; refreshed alongside the lobbies.
    profiles: Vec<ProfileEntry>,
    launch_dialog: Option<LaunchDialog>,
    notice: Option<String>,
    /// True while a poll is in flight (drives the header spinner without
    /// flashing the list back to skeletons).
    refreshing: bool,
    /// Temporary profiles created for "Temporary profile" launches, pending
    /// deletion once their game exits. Value is whether we've yet observed the
    /// profile with a running instance (so we don't delete it before its game
    /// even started).
    temp_cleanup: HashMap<String, bool>,
    /// Mod ids with a catalog lookup currently in flight from this view, so a
    /// later refresh doesn't kick off a duplicate fetch. Resolved info itself
    /// lives in the shared `mod_catalog_cache`, not here.
    mod_lookup_pending: HashSet<String>,
    /// The auto-refresh loop; dropped (and thus cancelled) with the view.
    _refresh: Task<()>,
}

enum LoadState {
    Loading,
    Loaded(Vec<LobbyRow>),
    /// `regionInfo.json` could not be read; holds the reason, which is usually
    /// actionable (on Linux, an unset Wine prefix or Proton compat data path).
    RegionsUnavailable(String),
}

#[derive(Clone)]
struct LobbyRow {
    game: Game,
    /// Display name for the lobby's region (from the server's own region list,
    /// falling back to the enabled region's name).
    region_label: String,
    /// Host + port of the enabled region this lobby was found on, used to
    /// point Among Us at the right region before launching. Scheme-agnostic —
    /// see `region_service::region_server_host_port`.
    server_host: String,
    server_port: u16,
}

struct LaunchDialog {
    lobby: LobbyRow,
    target: LaunchTarget,
    busy: bool,
    error: Option<String>,
}

#[derive(Clone, PartialEq)]
enum LaunchTarget {
    Existing(String),
    Temporary,
}

/// Display fields for one row of the launch dialog's profile picker.
struct TargetOption<'a> {
    target: LaunchTarget,
    title: &'a str,
    subtitle: &'a str,
    /// Per-profile mod install preview (see `install_summary`); empty to hide.
    detail: &'a str,
    detail_color: Hsla,
}

impl LobbiesView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Subscribed up front (before any launch can happen) so the event that
        // marks a freshly-launched temp profile as running is never missed.
        let mut rx = events::subscribe();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let BackendEvent::GameStateChanged(payload) = event else {
                    continue;
                };
                if this
                    .update(cx, |this, cx| {
                        this.reap_finished_temp_profiles(&payload, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let refresh = cx.spawn(async move |this, cx| {
            loop {
                // Bail out if the view is gone (also covered by Task drop).
                if this
                    .update(cx, |this, cx| {
                        this.refreshing = true;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }

                let servers = cx
                    .background_executor()
                    .spawn(async { region_service::lobby_servers() })
                    .await;

                match servers {
                    Err(e) => {
                        let _ = this.update(cx, |this, cx| {
                            this.state = LoadState::RegionsUnavailable(e.to_string());
                            this.refreshing = false;
                            cx.notify();
                        });
                    }
                    Ok(servers) => {
                        // Poll every enabled region concurrently; a server that
                        // errors or doesn't implement the endpoint is skipped.
                        let tasks: Vec<_> = servers
                            .into_iter()
                            .map(|srv| {
                                let host = srv.host.clone();
                                let port = srv.port;
                                let task = cx
                                    .background_executor()
                                    .spawn(async move { api::fetch_lobbies(&host, port) });
                                (srv, task)
                            })
                            .collect();

                        let mut rows: Vec<LobbyRow> = Vec::new();
                        for (srv, task) in tasks {
                            let Ok(result) = task.await else {
                                continue;
                            };
                            for game in result.games {
                                // Skip finished games — they can't be joined.
                                if game.status.as_deref() == Some("Ended") {
                                    continue;
                                }
                                let region_label = game
                                    .region_id
                                    .as_ref()
                                    .and_then(|id| {
                                        result
                                            .regions
                                            .iter()
                                            .find(|r| r.id.as_deref() == Some(id.as_str()))
                                    })
                                    .and_then(|r| r.name.clone())
                                    .unwrap_or_else(|| srv.region_name.clone());
                                rows.push(LobbyRow {
                                    game,
                                    region_label,
                                    server_host: srv.host.clone(),
                                    server_port: srv.port,
                                });
                            }
                        }
                        // Open lobbies first, then fuller rooms first.
                        rows.sort_by(|a, b| {
                            let open = |g: &Game| u8::from(g.status.as_deref() == Some("Lobby"));
                            open(&b.game)
                                .cmp(&open(&a.game))
                                .then(b.game.player_count.cmp(&a.game.player_count))
                        });

                        let mod_ids: Vec<String> = rows
                            .iter()
                            .flat_map(|row| row.game.mods.iter())
                            .filter_map(|m| m.id.clone())
                            .collect();

                        let _ = this.update(cx, |this, cx| {
                            this.state = LoadState::Loaded(rows);
                            this.refreshing = false;
                            this.ensure_mod_info(mod_ids, cx);
                            cx.notify();
                        });
                    }
                }

                // Keep the launch dialog's profile list current.
                let profiles = cx
                    .background_executor()
                    .spawn(async { profile_service::get_profiles().unwrap_or_default() })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.profiles = profiles;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }

                cx.background_executor()
                    .timer(Duration::from_secs(REFRESH_INTERVAL_SECS))
                    .await;
            }
        });

        Self {
            state: LoadState::Loading,
            profiles: Vec::new(),
            launch_dialog: None,
            notice: None,
            refreshing: false,
            temp_cleanup: HashMap::new(),
            mod_lookup_pending: HashSet::new(),
            _refresh: refresh,
        }
    }

    fn copy_code(&self, code: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(code));
    }

    /// Start watching a temporary profile so it's deleted once its game exits.
    /// Seeds the "seen running" flag from the current snapshot in case the
    /// launch's `GameStateChanged` event already fired before this call (the
    /// background launch thread registers the process, and may finish, before
    /// this runs on the main thread).
    fn track_temp_profile(&mut self, profile_id: String) {
        let already_running = game_runtime::current_state()
            .profile_instance_counts
            .contains_key(&profile_id);
        self.temp_cleanup.insert(profile_id, already_running);
    }

    /// Stop watching a temporary profile without deleting it here — used when
    /// the caller has already deleted it (e.g. a launch failure cleanup), so
    /// a later `GameStateChanged` doesn't attempt a redundant delete.
    fn forget_temp_profile(&mut self, profile_id: &str) {
        self.temp_cleanup.remove(profile_id);
    }

    /// Delete any temporary profile whose tracked instance count has dropped
    /// back to zero after having been seen running at least once.
    fn reap_finished_temp_profiles(&mut self, payload: &GameStatePayload, cx: &mut Context<Self>) {
        let mut finished = Vec::new();
        self.temp_cleanup.retain(|id, seen_running| {
            if payload.profile_instance_counts.contains_key(id) {
                *seen_running = true;
                true
            } else if *seen_running {
                finished.push(id.clone());
                false
            } else {
                true
            }
        });
        for id in finished {
            cx.spawn(async move |_this, cx| {
                let id_for_delete = id.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { profile_service::delete_profile(&id_for_delete) })
                    .await;
                if let Err(e) = result {
                    warn!("failed to delete temporary lobby profile {id}: {e}");
                }
            })
            .detach();
        }
    }

    /// Kick off background catalog lookups (via the shared `mod_catalog_cache`,
    /// also used by the Library's profile detail page) for any of `mod_ids`
    /// not already cached or in flight, so `render_row` can correlate a
    /// lobby's required mods to the Starlight catalog (name + thumbnail),
    /// falling back to the bare id when a mod isn't in the catalog.
    fn ensure_mod_info(&mut self, mod_ids: Vec<String>, cx: &mut Context<Self>) {
        let missing: Vec<String> = mod_ids
            .into_iter()
            .filter(|id| mod_catalog_cache::get(id).is_none())
            .filter(|id| self.mod_lookup_pending.insert(id.clone()))
            .collect();
        if missing.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let tasks: Vec<_> = missing
                .iter()
                .cloned()
                .map(|id| {
                    cx.background_executor()
                        .spawn(async move { mod_catalog_cache::fetch(&id) })
                })
                .collect();
            for task in tasks {
                task.await;
            }
            let _ = this.update(cx, |this, cx| {
                for id in &missing {
                    this.mod_lookup_pending.remove(id);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_launch_dialog(&mut self, lobby: LobbyRow, window: &mut Window, cx: &mut Context<Self>) {
        // Preselect the most-recently-launched profile that already has every
        // required mod installed (`self.profiles` is sorted last-launched
        // first); otherwise fall back to the most-recently-launched profile,
        // or a temporary one if there are no profiles at all.
        let required_mods = &lobby.game.mods;
        let target = self
            .profiles
            .iter()
            .find(|p| preview_mod_installs(required_mods, &p.mods).fully_satisfied())
            .or_else(|| self.profiles.first())
            .map(|p| LaunchTarget::Existing(p.id.clone()))
            .unwrap_or(LaunchTarget::Temporary);
        self.launch_dialog = Some(LaunchDialog {
            lobby,
            target,
            busy: false,
            error: None,
        });
        self.notice = None;
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let view = view.clone();
            // The dialog lives in the window's dialog layer and is rebuilt
            // every frame, so its state is read back out of the view here
            // instead of being captured when it opened.
            let Some((code, busy)) = view.read(cx).launch_dialog.as_ref().map(|state| {
                (
                    state
                        .lobby
                        .game
                        .code
                        .clone()
                        .unwrap_or_else(|| "------".to_string()),
                    state.busy,
                )
            }) else {
                return dialog;
            };
            let body = launch_dialog_body(&view, cx);
            let on_ok = view.clone();
            let on_close = view.clone();
            dialog
                .title(t!("lobbies.dialog_title", code = code).to_string())
                .w(px(460.0))
                // A launch in flight can't be cancelled, so every way out of
                // the dialog is closed off until it finishes or fails.
                .close_button(!busy)
                .overlay_closable(!busy)
                .keyboard(!busy)
                .child(body)
                .footer(
                    DialogFooter::new()
                        .child(if busy {
                            Button::new("launch-cancel")
                                .label(t!("common.cancel"))
                                .disabled(true)
                                .into_any_element()
                        } else {
                            DialogClose::new()
                                .child(Button::new("launch-cancel").label(t!("common.cancel")))
                                .into_any_element()
                        })
                        .child(
                            DialogAction::new().child(
                                Button::new("launch-confirm")
                                    .primary()
                                    .icon(Icon::new(IconName::Play))
                                    .label(if busy {
                                        t!("lobbies.launching")
                                    } else {
                                        t!("lobbies.launch")
                                    })
                                    .disabled(busy),
                            ),
                        ),
                )
                // The launch is asynchronous: the dialog stays up (showing
                // progress, or a failure) until `submit_launch` closes it.
                .on_ok(move |_, window, cx| {
                    on_ok.update(cx, |this, cx| this.submit_launch(window, cx));
                    false
                })
                .on_close(move |_, _window, cx| {
                    on_close.update(cx, |this, cx| {
                        this.launch_dialog = None;
                        cx.notify();
                    });
                })
        });
        cx.notify();
    }

    fn submit_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.launch_dialog.as_mut() else {
            return;
        };
        if dialog.busy {
            return;
        }
        let window_handle = window.window_handle();
        dialog.busy = true;
        dialog.error = None;
        let lobby = dialog.lobby.clone();
        let target = dialog.target.clone();
        cx.notify();

        let code = lobby.game.code.clone().unwrap_or_default();
        let server_host = lobby.server_host.clone();
        let server_port = lobby.server_port;
        // Mods with an id but no version can't be installed (we don't know
        // which version), but still count toward the "skipped" total so the
        // launch summary doesn't silently omit them.
        let mut required: Vec<InstallModInput> = Vec::new();
        let mut versionless = 0usize;
        for m in &lobby.game.mods {
            let Some(id) = m.id.clone() else { continue };
            match m.version.clone() {
                Some(version) => required.push(InstallModInput {
                    mod_id: id,
                    version,
                }),
                None => versionless += 1,
            }
        }

        cx.spawn(async move |this, cx| {
            // Resolve (or create) the target profile first and start watching
            // it for cleanup immediately if it's temporary — before any
            // further work that could fail, and before the game itself could
            // even exit, closing the race where a fast-exiting game leaves a
            // temp profile unwatched (and so never reaped).
            let resolved = cx
                .background_executor()
                .spawn(async move { resolve_launch_profile(target) })
                .await;
            let (profile, temp_profile_id) = match resolved {
                Ok(resolved) => resolved,
                Err(e) => {
                    warn!("failed to resolve launch profile: {e}");
                    let _ = this.update(cx, |this, cx| {
                        if let Some(d) = this.launch_dialog.as_mut() {
                            d.busy = false;
                            d.error = Some(e.to_string());
                        }
                        cx.notify();
                    });
                    return;
                }
            };
            if let Some(id) = temp_profile_id.clone() {
                let _ = this.update(cx, |this, cx| {
                    this.track_temp_profile(id);
                    cx.notify();
                });
            }

            let outcome = cx
                .background_executor()
                .spawn(async move {
                    launch_into_lobby_for_profile(
                        profile,
                        required,
                        versionless,
                        &server_host,
                        server_port,
                    )
                })
                .await;

            let launched = outcome.is_ok();
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(summary) => {
                        this.launch_dialog = None;
                        let mut message = String::new();
                        if !code.is_empty() {
                            this.copy_code(code.clone(), cx);
                            message = t!("lobbies.code_copied", code = code).to_string();
                        }
                        message.push_str(&summary);
                        this.notice = Some(message);
                    }
                    Err(e) => {
                        warn!("launch into lobby failed: {e}");
                        // The launch never succeeded, so there's no game
                        // process to wait for — clean up a temp profile right
                        // away instead of leaving it tracked with nothing to
                        // ever transition it out of "pending".
                        if let Some(id) = temp_profile_id {
                            this.forget_temp_profile(&id);
                            cx.spawn(async move |_this, cx| {
                                let id_for_delete = id.clone();
                                let result = cx
                                    .background_executor()
                                    .spawn(async move { profile_service::delete_profile(&id_for_delete) })
                                    .await;
                                if let Err(e) = result {
                                    warn!(
                                        "failed to clean up temp profile {id} after launch error: {e}"
                                    );
                                }
                            })
                            .detach();
                        }
                        if let Some(d) = this.launch_dialog.as_mut() {
                            d.busy = false;
                            d.error = Some(e.to_string());
                        }
                    }
                }
                cx.notify();
            });
            // The dialog itself lives in the window, so dismissing it takes a
            // window update rather than just clearing `launch_dialog`.
            if launched {
                let _ = window_handle.update(cx, |_, window, cx| window.close_dialog(cx));
            }
        })
        .detach();
    }

    fn render_lobbies(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        match &self.state {
            LoadState::Loading => div()
                .flex()
                .flex_col()
                .gap_2()
                .children((0..4).map(|_| {
                    Skeleton::new()
                        .w_full()
                        .h(px(64.0))
                        .rounded_lg()
                        .into_any_element()
                }))
                .into_any_element(),
            LoadState::RegionsUnavailable(reason) => Alert::warning(
                "lobbies-regions-unavailable",
                t!("lobbies.regions_unavailable", reason = reason).to_string(),
            )
            .title(t!("lobbies.regions_unavailable_title"))
            .into_any_element(),
            LoadState::Loaded(rows) if rows.is_empty() => div()
                .text_sm()
                .text_color(theme.text_muted)
                .child(t!("lobbies.empty").to_string())
                .into_any_element(),
            LoadState::Loaded(rows) => div()
                .flex()
                .flex_col()
                .gap_2()
                .children(
                    rows.iter()
                        .enumerate()
                        .map(|(ix, row)| self.render_row(ix, row, theme, cx)),
                )
                .into_any_element(),
        }
    }

    fn render_row(
        &self,
        ix: usize,
        row: &LobbyRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let game = &row.game;
        let code = game.code.clone().unwrap_or_default();
        let host = game
            .host_name
            .clone()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| t!("lobbies.unknown_host").to_string());
        let players = format!(
            "{}/{}",
            game.player_count.unwrap_or(0),
            game.max_players.unwrap_or(0)
        );
        let meta_line = [
            players,
            map_name(game.map_id),
            row.region_label.clone(),
        ]
        .join(" · ");

        let is_open = game.status.as_deref() == Some("Lobby");
        let status_text = game
            .status
            .clone()
            .unwrap_or_else(|| t!("common.unknown").to_string());
        let status_color = if is_open {
            theme.success
        } else {
            theme.warning
        };

        let copy_code = code.clone();
        let row_for_launch = row.clone();

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
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_family("ui-monospace, monospace")
                                    .font_weight(FontWeight::BOLD)
                                    .child(if code.is_empty() {
                                        "------".to_string()
                                    } else {
                                        code.clone()
                                    }),
                            )
                            .child(div().text_xs().text_color(status_color).child(status_text))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.text_muted)
                                    .child(host),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child(meta_line),
                    )
                    .when(!game.mods.is_empty(), |s| {
                        s.child(mod_chip_row(&game.mods, theme))
                    }),
            )
            // Nothing to copy for a lobby the server didn't give a code.
            .children((!copy_code.is_empty()).then(|| {
                // `Clipboard::on_copied` hands the value over by move, so it
                // can't go through `cx.listener` (which takes events by ref).
                let view = cx.entity();
                Clipboard::new(SharedString::from(format!("copy-code-{ix}")))
                    .value(copy_code.clone())
                    .tooltip(t!("lobbies.copy_code").to_string())
                    .on_copied(move |_, _window, cx| {
                        view.update(cx, |this, cx| {
                            this.notice = Some(t!("lobbies.code_copied_notice").to_string());
                            cx.notify();
                        });
                    })
            }))
            .child(
                Button::new(SharedString::from(format!("launch-lobby-{ix}")))
                    .primary()
                    .xsmall()
                    .icon(Icon::new(IconName::Play))
                    .label(t!("lobbies.launch"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_launch_dialog(row_for_launch.clone(), window, cx)
                    })),
            )
            .into_any_element()
    }
}

/// Body of the launch dialog — the region line, the profile picker and the
/// lobby's required mods. Built from the view's `launch_dialog` state, which
/// the dialog layer re-reads on every frame.
fn launch_dialog_body(view: &Entity<LobbiesView>, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let this = view.read(cx);
    let Some(dialog) = this.launch_dialog.as_ref() else {
        return div().into_any_element();
    };
    let required_mods = &dialog.lobby.game.mods;
    let no_mods: Vec<ProfileModEntry> = Vec::new();

    let mut option_rows: Vec<AnyElement> = this
        .profiles
        .iter()
        .map(|p| {
            let bep_subtitle = if p.bepinex_installed.is_some() {
                t!("lobbies.modded_profile").to_string()
            } else {
                t!("lobbies.bepinex_will_install").to_string()
            };
            let preview = preview_mod_installs(required_mods, &p.mods);
            let (detail, detail_color) = install_summary(&preview, &theme);
            render_target_option(
                view,
                TargetOption {
                    target: LaunchTarget::Existing(p.id.clone()),
                    title: &p.name,
                    subtitle: &bep_subtitle,
                    detail: &detail,
                    detail_color,
                },
                &dialog.target,
                &theme,
            )
        })
        .collect();
    let temp_preview = preview_mod_installs(required_mods, &no_mods);
    let (temp_detail, temp_detail_color) = install_summary(&temp_preview, &theme);
    option_rows.push(render_target_option(
        view,
        TargetOption {
            target: LaunchTarget::Temporary,
            title: t!("lobbies.temporary_profile").as_ref(),
            subtitle: t!("lobbies.temporary_profile_subtitle").as_ref(),
            detail: &temp_detail,
            detail_color: temp_detail_color,
        },
        &dialog.target,
        &theme,
    ));

    let mut items: Vec<AnyElement> = vec![
        div()
            .text_xs()
            .text_color(theme.text_muted)
            .child(t!("lobbies.region", region = dialog.lobby.region_label).to_string())
            .into_any_element(),
        section_label(t!("lobbies.profile"), &theme).into_any_element(),
        div()
            .id("launch-profile-list")
            .flex()
            .flex_col()
            .gap_2()
            .max_h(px(220.0))
            .overflow_y_scroll()
            .children(option_rows)
            .into_any_element(),
    ];
    if required_mods.is_empty() {
        items.push(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child(t!("lobbies.no_mods_required").to_string())
                .into_any_element(),
        );
    } else {
        items.push(section_label(t!("lobbies.required_mods"), &theme).into_any_element());
        items.push(mod_chip_row(required_mods, &theme));
    }
    if let Some(err) = &dialog.error {
        items.push(
            Alert::error("launch-error", err.clone())
                .small()
                .into_any_element(),
        );
    }

    div()
        .flex()
        .flex_col()
        .gap_3()
        .children(items)
        .into_any_element()
}

/// One row of the launch dialog's profile picker: a radio plus the profile's
/// name, BepInEx state and mod-install preview.
fn render_target_option(
    view: &Entity<LobbiesView>,
    option: TargetOption,
    selected: &LaunchTarget,
    theme: &Theme,
) -> AnyElement {
    let TargetOption {
        target,
        title,
        subtitle,
        detail,
        detail_color,
    } = option;
    let is_selected = &target == selected;
    let id = match &target {
        LaunchTarget::Existing(pid) => format!("target-{pid}"),
        LaunchTarget::Temporary => "target-temporary".to_string(),
    };
    let border = if is_selected {
        theme.primary
    } else {
        theme.border
    };
    // The whole row is the hit target, and so is the radio inside it (which
    // would otherwise swallow clicks aimed straight at it).
    let pick = move |view: &Entity<LobbiesView>, target: &LaunchTarget| {
        let view = view.clone();
        let target = target.clone();
        move |cx: &mut App| {
            let target = target.clone();
            view.update(cx, |this, cx| {
                if let Some(d) = this.launch_dialog.as_mut() {
                    d.target = target;
                }
                cx.notify();
            });
        }
    };
    let on_row_click = pick(view, &target);
    let on_radio_click = pick(view, &target);
    div()
        .id(SharedString::from(id.clone()))
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_lg()
        .bg(theme.background)
        .border_1()
        .border_color(border)
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover))
        .on_click(move |_, _window, cx| on_row_click(cx))
        .child(
            Radio::new(SharedString::from(format!("{id}-radio")))
                .checked(is_selected)
                .on_click(move |_, _window, cx| on_radio_click(cx)),
        )
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
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(subtitle.to_string()),
                )
                .when(!detail.is_empty(), |s| {
                    s.child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(detail_color)
                            .child(detail.to_string()),
                    )
                }),
        )
        .into_any_element()
}

impl Render for LobbiesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        page_root("lobbies-page", &theme)
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
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_2xl().font_weight(FontWeight::BOLD).child(t!("nav.lobbies")))
                            .when(self.refreshing, |s| {
                                s.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.text_muted)
                                        .child(t!("lobbies.refreshing").to_string()),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child(t!("lobbies.description").to_string()),
                    ),
            )
            .children(self.notice.clone().map(|message| {
                Alert::success("lobbies-notice", message).on_close(cx.listener(
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
                    .child(section_label(t!("lobbies.active"), &theme))
                    .child(self.render_lobbies(&theme, cx)),
            )
    }
}

/// A wrapped row of [`render_mod_chip`]s for a lobby's required mods.
fn mod_chip_row(mods: &[LobbyMod], theme: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .gap_1p5()
        .children(mods.iter().map(|m| render_mod_chip(m, theme)))
        .into_any_element()
}

/// A small icon + label for one of a lobby's required mods, correlated
/// against the shared Starlight catalog cache by id when possible. Falls
/// back to the bare mod id with a default icon when the catalog has no match
/// (or the lookup hasn't resolved yet), or to a generic "Unknown mod" label
/// when the server didn't even send an id for this entry.
fn render_mod_chip(lobby_mod: &LobbyMod, theme: &Theme) -> AnyElement {
    let resolved = lobby_mod
        .id
        .as_deref()
        .and_then(mod_catalog_cache::get)
        .flatten();

    let label = match (lobby_mod.id.as_deref(), &resolved, &lobby_mod.version) {
        (_, Some(info), Some(version)) => format!("{} {version}", info.name),
        (_, Some(info), None) => info.name.clone(),
        (Some(id), None, Some(version)) => format!("{id} {version}"),
        (Some(id), None, None) => id.to_string(),
        (None, _, Some(version)) => t!("lobbies.unknown_mod_version", version = version).to_string(),
        (None, _, None) => t!("lobbies.unknown_mod").to_string(),
    };

    let icon: AnyElement = match (&resolved, lobby_mod.id.as_deref()) {
        (Some(_), Some(id)) => img(api::mod_thumbnail_url(id))
            .w(px(14.0))
            .h(px(14.0))
            .flex_none()
            .rounded_sm()
            .object_fit(ObjectFit::Contain)
            .into_any_element(),
        _ => Icon::new(IconName::File)
            .size(px(12.0))
            .text_color(theme.text_muted)
            .into_any_element(),
    };

    Tag::secondary()
        .small()
        .outline()
        .child(icon)
        .child(
            div()
                .max_w(px(160.0))
                .truncate()
                .text_color(theme.text_muted)
                .child(label),
        )
        .into_any_element()
}

/// What launching `required` mods into a profile already holding `installed`
/// would do: the catalog names of mods that would be newly installed, how many
/// required mods aren't in the Starlight catalog (and so would be skipped —
/// see `mod_install_service::plan_lobby_mods`), and how many still-missing
/// mods haven't had their catalog lookup resolve yet (so we genuinely don't
/// know if they're installable). Only covers the lobby's directly-required
/// mods, not their transitive dependencies (which need a network round-trip
/// to resolve and so aren't known until launch).
struct ModInstallPreview {
    to_install: Vec<String>,
    unavailable: usize,
    pending: usize,
}

impl ModInstallPreview {
    /// Whether this profile is confirmed to already have every resolvable
    /// required mod — `false` while any mod's catalog status is still unknown,
    /// rather than optimistically assuming it'll turn out installed.
    fn fully_satisfied(&self) -> bool {
        self.to_install.is_empty() && self.pending == 0
    }
}

fn preview_mod_installs(required: &[LobbyMod], installed: &[ProfileModEntry]) -> ModInstallPreview {
    let mut to_install = Vec::new();
    let mut unavailable = 0;
    let mut pending = 0;
    for m in required {
        let Some(id) = &m.id else { continue };
        let already_installed = installed.iter().any(|p| {
            p.mod_id == *id
                && match &m.version {
                    Some(v) => &p.version == v,
                    None => true,
                }
        });
        if already_installed {
            continue;
        }
        match mod_catalog_cache::get(id) {
            Some(Some(info)) => to_install.push(info.name),
            Some(None) => unavailable += 1,
            // Not resolved yet — unknown, not "will install"; the chip list
            // and this preview both update once the lookup completes.
            None => pending += 1,
        }
    }
    ModInstallPreview {
        to_install,
        unavailable,
        pending,
    }
}

/// Human-readable label for a [`ModInstallPreview`], plus the color to show
/// it in (the theme's success color when nothing needs to change).
fn install_summary(preview: &ModInstallPreview, theme: &Theme) -> (String, Hsla) {
    if preview.fully_satisfied() && preview.unavailable == 0 {
        return (
            t!("lobbies.all_installed").to_string(),
            theme.success,
        );
    }
    let mut parts = Vec::new();
    if !preview.to_install.is_empty() {
        const MAX_NAMES: usize = 3;
        let mut names = preview.to_install.clone();
        let extra = names.len().saturating_sub(MAX_NAMES);
        names.truncate(MAX_NAMES);
        let mut text = t!("lobbies.will_install", names = names.join(", ")).to_string();
        if extra > 0 {
            text.push_str(t!("lobbies.more", count = extra).as_ref());
        }
        parts.push(text);
    }
    if preview.pending > 0 {
        parts.push(t!("lobbies.checking", count = preview.pending).to_string());
    }
    if preview.unavailable > 0 {
        parts.push(t!("lobbies.not_in_catalog", count = preview.unavailable).to_string());
    }
    (parts.join(" · "), theme.text_muted)
}

/// Map id → Among Us map name (see `MapNames.cs`). Map names are game
/// content and stay untranslated; only the fallback is localized.
fn map_name(map_id: Option<u32>) -> String {
    match map_id {
        Some(0) => "The Skeld".into(),
        Some(1) => "MIRA HQ".into(),
        Some(2) => "Polus".into(),
        Some(3) => "Dleks".into(),
        Some(4) => "The Airship".into(),
        Some(5) => "The Fungle".into(),
        _ => t!("lobbies.unknown_map").to_string(),
    }
}

/// Resolve a launch target to a concrete profile: an existing profile by id,
/// or a freshly-created temporary one. Returns the temp profile's id again
/// (`None` for an existing profile) so the caller can start watching it for
/// cleanup right away, before doing anything else that could fail or race the
/// launched game's own exit. Blocking; run on the background executor.
fn resolve_launch_profile(target: LaunchTarget) -> AppResult<(ProfileEntry, Option<String>)> {
    match target {
        LaunchTarget::Existing(id) => {
            let profile = profile_service::get_profile_by_id(&id)?
                .ok_or_else(|| AppError::validation(t!("lobbies.profile_gone").to_string()))?;
            Ok((profile, None))
        }
        LaunchTarget::Temporary => {
            let profile = create_temp_profile()?;
            let id = profile.id.clone();
            Ok((profile, Some(id)))
        }
    }
}

/// Install the lobby's required mods into `profile`, point Among Us at the
/// lobby's region, and launch. `versionless` is the count of required mods
/// the lobby sent with no version (uninstallable, but still reported as
/// skipped rather than silently dropped). Blocking; run on the background
/// executor.
fn launch_into_lobby_for_profile(
    profile: ProfileEntry,
    required: Vec<InstallModInput>,
    versionless: usize,
    server_host: &str,
    server_port: u16,
) -> AppResult<String> {
    if profile.bepinex_installed.is_none() {
        profile_service::install_bepinex_for_profile(&profile.id)?;
    }

    let mut skipped = versionless;
    let mut failed = 0usize;
    if !required.is_empty() {
        let (installable, unresolved) = mod_install_service::plan_lobby_mods(&required);
        skipped += unresolved.len();
        // Skip mods already present at the exact version the lobby wants.
        let missing: Vec<InstallModInput> = installable
            .into_iter()
            .filter(|m| {
                !profile
                    .mods
                    .iter()
                    .any(|p| p.mod_id == m.mod_id && p.version == m.version)
            })
            .collect();
        // Install one mod at a time: install_mods_for_profile rolls back its
        // whole batch on a single failure, which is right for one coherent
        // "install this mod" user action but wrong here — a lobby launch
        // wants each required mod to be independently best-effort, so one
        // flaky download doesn't sink mods that already succeeded.
        for item in missing {
            let mod_id = item.mod_id.clone();
            if let Err(e) = mod_install_service::install_mods_for_profile(
                &profile.id,
                std::slice::from_ref(&item),
            ) {
                warn!("failed to install mod {mod_id} for lobby launch: {e}");
                failed += 1;
            }
        }
    }

    let region_set =
        region_service::select_region_by_host_port(server_host, server_port).unwrap_or(false);

    // Reload so the launch sees the freshly installed BepInEx / mods.
    let profile = profile_service::get_profile_by_id(&profile.id)?
        .ok_or_else(|| AppError::validation(t!("lobbies.profile_gone_launch").to_string()))?;
    launch_service::launch_modded_for_profile(profile)?;

    let mut summary = if region_set {
        t!("lobbies.launched_region_set").to_string()
    } else {
        t!("lobbies.launched").to_string()
    };
    if skipped > 0 {
        summary.push_str(t!("lobbies.skipped_catalog", count = skipped).as_ref());
    }
    if failed > 0 {
        summary.push_str(t!("lobbies.skipped_failed", count = failed).as_ref());
    }
    Ok(summary)
}

/// Create a fresh throwaway profile for a one-off lobby launch, uniquely named
/// so repeated temporary launches don't collide. The caller (`LobbiesView`)
/// deletes it once the launched game exits.
fn create_temp_profile() -> AppResult<ProfileEntry> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    profile_service::create_profile(&format!("Temporary Lobby {millis}"))
}
