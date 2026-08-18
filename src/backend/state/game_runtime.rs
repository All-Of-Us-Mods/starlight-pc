use crate::backend::error::{AppError, AppResult};
use crate::backend::services::{profile_instance_service, profile_service};
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Child;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Which directory a modded launch runs from. Slot 0 is the profile directory
/// itself; higher slots are throwaway copies made by
/// [`profile_instance_service`] so concurrent launches of the same profile
/// don't share BepInEx state. `temporary_dir` is set for those copies and
/// deleted once the instance exits.
#[derive(Default)]
pub struct LaunchInstance {
    pub slot: usize,
    pub temporary_dir: Option<PathBuf>,
}

struct TrackedGameProcess {
    child: Child,
    profile_id: Option<String>,
    launched_at: Instant,
    instance: LaunchInstance,
}

/// A game handed off to the Steam client (`steam -applaunch`). Steam reparents
/// the game outside our process tree and the spawned `steam` invoker exits
/// immediately, so there's no Child to track — we watch `/proc` for the game
/// process instead.
#[cfg(target_os = "linux")]
struct SteamInstance {
    id: u64,
    profile_id: Option<String>,
    launched_at: Instant,
    /// Set once the game process is first observed; until then we're waiting
    /// for Steam → Proton → game to come up.
    seen_running: bool,
}

#[cfg(target_os = "linux")]
static STEAM_INSTANCE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct TrackedState {
    processes: Vec<TrackedGameProcess>,
    uwp_instances: Vec<Option<String>>,
    #[cfg(target_os = "linux")]
    steam_instances: Vec<SteamInstance>,
}

static TRACKED_STATE: LazyLock<Mutex<TrackedState>> =
    LazyLock::new(|| Mutex::new(TrackedState::default()));

#[derive(Clone, Debug, serde::Serialize)]
pub struct GameStatePayload {
    pub running: bool,
    pub running_count: usize,
    pub stoppable_running_count: usize,
    pub profile_instance_counts: HashMap<String, usize>,
    pub stoppable_profile_instance_counts: HashMap<String, usize>,
}

// Proton/Steam reparent the actual game outside our process tree, so we can't
// rely on PID/pgid. Every relevant process (wrapper, proton, wine, the game)
// carries an identifying substring in its cmdline — the profile id (via the
// doorstop args we pass for modded launches) or `Among Us.exe` (passed as
// the game-exe argument for any launch) — so match on that.
/// Iterate `(pid, cmdline)` for every process in `/proc`. Cmdlines are decoded
/// lossily and keep their NUL arg separators — callers only substring-match.
#[cfg(target_os = "linux")]
fn proc_cmdlines() -> impl Iterator<Item = (i32, String)> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            Some((pid, String::from_utf8_lossy(&cmdline).into_owned()))
        })
}

#[cfg(target_os = "linux")]
fn kill_by_cmdline_substring(substring: &str) {
    for (pid, cmdline) in proc_cmdlines() {
        if cmdline.contains(substring) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
}

fn reap_process(mut child: Child) {
    if let Err(e) = child.wait() {
        warn!("Failed to reap game process: {}", e);
    }
}

/// Reap a tracked process, record its session against the owning profile, and
/// drop the instance copy it ran from (if any).
fn reap_and_record(tracked: TrackedGameProcess) {
    let TrackedGameProcess {
        child,
        profile_id,
        launched_at,
        instance,
    } = tracked;
    reap_process(child);
    record_play_time(profile_id, launched_at);
    release_instance_dir(instance.temporary_dir);
}

/// Delete a finished instance's throwaway copy off the caller's thread — it's
/// a recursive delete of a profile-sized directory.
fn release_instance_dir(directory: Option<PathBuf>) {
    let Some(directory) = directory else { return };
    std::thread::spawn(move || profile_instance_service::release(&directory));
}

/// Persist a play session against the owning profile. Offloaded to a detached
/// thread so callers don't block (or hold the tracked-state lock) on disk I/O.
fn record_play_time(profile_id: Option<String>, launched_at: Instant) {
    let Some(id) = profile_id else { return };
    let duration_ms = launched_at.elapsed().as_millis().min(i64::MAX as u128) as i64;
    if duration_ms <= 0 {
        return;
    }
    std::thread::spawn(move || {
        if let Err(e) = profile_service::add_play_time(&id, duration_ms) {
            warn!("add_play_time failed for profile {id}: {e}");
        }
        crate::backend::events::publish(crate::backend::events::BackendEvent::ProfileStatsUpdated(
            id,
        ));
    });
}

fn build_state_payload(state: &TrackedState) -> GameStatePayload {
    let mut profile_instance_counts = HashMap::new();
    let mut stoppable_profile_instance_counts = HashMap::new();
    for tracked in &state.processes {
        if let Some(profile_id) = &tracked.profile_id {
            *profile_instance_counts
                .entry(profile_id.clone())
                .or_insert(0) += 1;
            *stoppable_profile_instance_counts
                .entry(profile_id.clone())
                .or_insert(0) += 1;
        }
    }
    for profile_id in state.uwp_instances.iter().flatten() {
        *profile_instance_counts
            .entry(profile_id.clone())
            .or_insert(0) += 1;
    }

    #[cfg(target_os = "linux")]
    let steam_count = {
        for inst in &state.steam_instances {
            if let Some(profile_id) = &inst.profile_id {
                *profile_instance_counts
                    .entry(profile_id.clone())
                    .or_insert(0) += 1;
                *stoppable_profile_instance_counts
                    .entry(profile_id.clone())
                    .or_insert(0) += 1;
            }
        }
        state.steam_instances.len()
    };
    #[cfg(not(target_os = "linux"))]
    let steam_count = 0;

    let running_count = state.processes.len() + state.uwp_instances.len() + steam_count;
    GameStatePayload {
        running: running_count > 0,
        running_count,
        stoppable_running_count: state.processes.len() + steam_count,
        profile_instance_counts,
        stoppable_profile_instance_counts,
    }
}

fn emit_state_snapshot(state: &TrackedState) {
    let payload = build_state_payload(state);
    crate::backend::events::publish(crate::backend::events::BackendEvent::GameStateChanged(
        payload,
    ));
}

fn prune_finished_processes(state: &mut TrackedState) {
    let mut i = 0;
    while i < state.processes.len() {
        match state.processes[i].child.try_wait() {
            Ok(Some(_)) => {
                let tracked = state.processes.swap_remove(i);
                reap_and_record(tracked);
            }
            Ok(None) => i += 1,
            Err(e) => {
                warn!("Failed to check tracked process state: {}", e);
                let tracked = state.processes.swap_remove(i);
                reap_and_record(tracked);
            }
        }
    }
}

fn monitor_game_process(process_id: u32) {
    std::thread::spawn(move || {
        info!("Monitoring game process state");
        loop {
            std::thread::sleep(Duration::from_millis(500));

            let Ok(mut state) = TRACKED_STATE.lock() else {
                error!("Failed to acquire game process lock");
                break;
            };

            let Some(index) = state
                .processes
                .iter()
                .position(|tracked| tracked.child.id() == process_id)
            else {
                debug!("Monitored process no longer available");
                break;
            };

            match state.processes[index].child.try_wait() {
                Ok(Some(status)) => {
                    info!("Game process exited with status: {:?}", status);
                    let tracked = state.processes.swap_remove(index);
                    emit_state_snapshot(&state);
                    drop(state);
                    reap_and_record(tracked);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("Failed to check game process state: {}", e);
                    let tracked = state.processes.swap_remove(index);
                    emit_state_snapshot(&state);
                    drop(state);
                    reap_and_record(tracked);
                    break;
                }
            }
        }
    });
}

/// Stamp a launch against the profile and notify listeners.
fn mark_launched(profile_id: Option<&str>) {
    let Some(id) = profile_id else { return };
    if let Err(e) = profile_service::update_last_launched(id) {
        warn!("update_last_launched failed for profile {id}: {e}");
    } else {
        crate::backend::events::publish(crate::backend::events::BackendEvent::ProfileStatsUpdated(
            id.to_string(),
        ));
    }
}

/// Launch slots currently taken by running instances of `profile_id`. The
/// launch path picks the lowest free slot so a new instance never reuses the
/// directory of a live one. Instances we can't attribute to a directory (Xbox
/// UWP, Steam-launched) always run from the profile itself, i.e. slot 0.
pub fn used_instance_slots(profile_id: &str) -> HashSet<usize> {
    let mut state = TRACKED_STATE.lock().unwrap_or_else(|e| e.into_inner());
    prune_finished_processes(&mut state);

    let mut slots: HashSet<usize> = state
        .processes
        .iter()
        .filter(|tracked| tracked.profile_id.as_deref() == Some(profile_id))
        .map(|tracked| tracked.instance.slot)
        .collect();

    if state
        .uwp_instances
        .iter()
        .flatten()
        .any(|id| id == profile_id)
    {
        slots.insert(0);
    }

    #[cfg(target_os = "linux")]
    if state
        .steam_instances
        .iter()
        .any(|inst| inst.profile_id.as_deref() == Some(profile_id))
    {
        slots.insert(0);
    }

    slots
}

pub fn register_launched_process(
    child: Child,
    profile_id: Option<String>,
    instance: LaunchInstance,
) -> AppResult<()> {
    mark_launched(profile_id.as_deref());

    let process_id: u32;
    {
        let mut state = TRACKED_STATE
            .lock()
            .map_err(|_| AppError::state("Failed to acquire game process lock"))?;

        prune_finished_processes(&mut state);

        process_id = child.id();
        state.processes.push(TrackedGameProcess {
            child,
            profile_id,
            launched_at: Instant::now(),
            instance,
        });
        emit_state_snapshot(&state);
    }

    monitor_game_process(process_id);
    Ok(())
}

/// True if any process' cmdline mentions the game exe — covers the wrapper,
/// Proton, wine, and the game itself.
#[cfg(target_os = "linux")]
fn is_among_us_running() -> bool {
    proc_cmdlines().any(|(_, cmdline)| cmdline.contains("Among Us.exe"))
}

/// Watch a Steam-launched game by polling `/proc` (we have no Child handle).
/// Waits for the game to appear, then clears the instance when it exits.
#[cfg(target_os = "linux")]
fn monitor_steam_instance(instance_id: u64) {
    const STARTUP_GRACE: Duration = Duration::from_secs(120);
    std::thread::spawn(move || {
        info!("Monitoring Steam-launched game state");
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let running = is_among_us_running();

            let Ok(mut state) = TRACKED_STATE.lock() else {
                error!("Failed to acquire game process lock");
                break;
            };
            let Some(inst) = state
                .steam_instances
                .iter_mut()
                .find(|i| i.id == instance_id)
            else {
                break;
            };

            if inst.seen_running {
                if !running {
                    let profile_id = inst.profile_id.clone();
                    let launched_at = inst.launched_at;
                    state.steam_instances.retain(|i| i.id != instance_id);
                    emit_state_snapshot(&state);
                    drop(state);
                    record_play_time(profile_id, launched_at);
                    break;
                }
            } else if running {
                inst.seen_running = true;
            } else if inst.launched_at.elapsed() > STARTUP_GRACE {
                info!("Steam-launched game never appeared; dropping watch");
                state.steam_instances.retain(|i| i.id != instance_id);
                emit_state_snapshot(&state);
                break;
            }
        }
    });
}

/// Register a Steam-launched game for tracking. Steam only ever runs one
/// instance of the game, so an existing watch is refreshed rather than
/// duplicated.
#[cfg(target_os = "linux")]
pub fn register_steam_launch(profile_id: Option<String>) -> AppResult<()> {
    mark_launched(profile_id.as_deref());

    let mut state = TRACKED_STATE
        .lock()
        .map_err(|_| AppError::state("Failed to acquire game process lock"))?;
    prune_finished_processes(&mut state);

    if let Some(inst) = state.steam_instances.first_mut() {
        inst.profile_id = profile_id;
        inst.launched_at = Instant::now();
        inst.seen_running = false;
        emit_state_snapshot(&state);
    } else {
        let instance_id = STEAM_INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
        state.steam_instances.push(SteamInstance {
            id: instance_id,
            profile_id,
            launched_at: Instant::now(),
            seen_running: false,
        });
        emit_state_snapshot(&state);
        drop(state);
        monitor_steam_instance(instance_id);
    }
    Ok(())
}

fn stop_matching_processes<F>(state: &mut TrackedState, predicate: F) -> AppResult<usize>
where
    F: Fn(&TrackedGameProcess) -> bool,
{
    let mut stopped_count = 0;
    #[cfg_attr(target_os = "linux", allow(unused_mut))]
    let mut errors: Vec<String> = Vec::new();
    let mut i = 0;

    while i < state.processes.len() {
        if !predicate(&state.processes[i]) {
            i += 1;
            continue;
        }

        match state.processes[i].child.try_wait() {
            Ok(Some(_)) => {
                let tracked = state.processes.swap_remove(i);
                reap_and_record(tracked);
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                warn!("Failed to check tracked process state before stop: {}", e);
                let tracked = state.processes.swap_remove(i);
                reap_and_record(tracked);
                continue;
            }
        }

        // On Linux, kill_by_cmdline_substring (called before this loop) already SIGKILLed
        // every matching cmdline; just reap. Other platforms kill the tracked
        // child directly.
        #[cfg(not(target_os = "linux"))]
        {
            let pid = state.processes[i].child.id();
            if let Err(e) = state.processes[i].child.kill() {
                warn!("Failed to kill game process: {}", e);
                errors.push(format!("Failed to stop game process {pid}: {e}"));
                i += 1;
                continue;
            }
        }

        let tracked = state.processes.swap_remove(i);
        reap_and_record(tracked);
        stopped_count += 1;
    }

    if !errors.is_empty() {
        return Err(AppError::process(errors.join("; ")));
    }

    Ok(stopped_count)
}

pub fn stop_profile_instances(profile_id: &str) -> AppResult<usize> {
    #[cfg(target_os = "linux")]
    kill_by_cmdline_substring(profile_id);

    let mut state = TRACKED_STATE
        .lock()
        .map_err(|_| AppError::state("Failed to acquire game process lock"))?;

    prune_finished_processes(&mut state);
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut stopped = stop_matching_processes(&mut state, |tracked| {
        tracked.profile_id.as_deref() == Some(profile_id)
    })?;

    // Steam-launched games carry no profile id in their cmdline, so kill by
    // the game exe name and drop the matching watch.
    #[cfg(target_os = "linux")]
    if state
        .steam_instances
        .iter()
        .any(|i| i.profile_id.as_deref() == Some(profile_id))
    {
        kill_by_cmdline_substring("Among Us.exe");
        let before = state.steam_instances.len();
        state
            .steam_instances
            .retain(|i| i.profile_id.as_deref() != Some(profile_id));
        stopped += before - state.steam_instances.len();
    }

    emit_state_snapshot(&state);
    Ok(stopped)
}

pub fn current_state() -> GameStatePayload {
    let state = TRACKED_STATE.lock().unwrap_or_else(|e| e.into_inner());
    build_state_payload(&state)
}

pub fn stop_all_tracked_instances() -> AppResult<usize> {
    // SIGKILL anything that smells like Among Us first — same trick as the
    // per-profile stop, just with a broader marker so vanilla launches (no
    // profile id in their cmdline) get caught too.
    #[cfg(target_os = "linux")]
    kill_by_cmdline_substring("Among Us.exe");

    let mut state = TRACKED_STATE
        .lock()
        .map_err(|_| AppError::state("Failed to acquire game process lock"))?;

    prune_finished_processes(&mut state);
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut stopped = stop_matching_processes(&mut state, |_| true)?;

    #[cfg(target_os = "linux")]
    {
        stopped += state.steam_instances.len();
        state.steam_instances.clear();
    }

    emit_state_snapshot(&state);
    Ok(stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_aggregates_profile_counts() {
        let mut state = TrackedState::default();
        state.uwp_instances.push(Some("p1".to_string()));
        state.uwp_instances.push(Some("p1".to_string()));
        state.uwp_instances.push(Some("p2".to_string()));

        let payload = build_state_payload(&state);
        assert!(payload.running);
        assert_eq!(payload.running_count, 3);
        assert_eq!(payload.stoppable_running_count, 0);
        assert_eq!(payload.profile_instance_counts.get("p1"), Some(&2));
        assert_eq!(payload.profile_instance_counts.get("p2"), Some(&1));
        assert!(payload.stoppable_profile_instance_counts.is_empty());
    }
}
