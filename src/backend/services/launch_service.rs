use crate::backend::error::{AppError, AppResult};
use crate::backend::services::core_service;
use crate::backend::services::profile_instance_service;
use crate::backend::services::profile_service::ProfileEntry;
#[cfg(windows)]
use crate::backend::services::xbox_service;
use crate::backend::state::game_runtime::{self, LaunchInstance};
use log::{debug, info};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Serializes modded launches from prep through spawn, so two launches fired
/// in quick succession can't pick the same instance slot or race each other's
/// preparation of the shared game directory. Instances themselves don't wait
/// for each other: each extra concurrent launch of a profile runs from its own
/// copy of the profile (see [`profile_instance_service`]), which is what keeps
/// the BepInEx state they'd otherwise fight over separate.
static LAUNCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Per-profile cancellation counter. A queued launch records the value when it
/// starts waiting on [`LAUNCH_LOCK`]; if [`cancel_pending_launches`] has bumped
/// it by the time the lock is acquired, the launch aborts instead of spawning.
/// Lets the Stop button cancel launches still waiting to be prepared.
static CANCEL_GENERATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn cancel_generation(profile_id: &str) -> u64 {
    CANCEL_GENERATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(profile_id)
        .copied()
        .unwrap_or(0)
}

/// Cancel any launches for `profile_id` still queued behind the launch lock.
/// They abort (without spawning) when they reach the front of the queue.
pub fn cancel_pending_launches(profile_id: &str) {
    *CANCEL_GENERATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(profile_id.to_string())
        .or_insert(0) += 1;
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum LinuxRunner {
    Wine {
        binary: String,
        prefix: String,
    },
    Proton {
        binary: String,
        #[serde(rename = "compatDataPath")]
        compat_data_path: String,
        #[serde(rename = "steamClientPath")]
        steam_client_path: String,
        #[serde(rename = "useSteamRun")]
        use_steam_run: bool,
    },
    /// Launch through the Steam client instead of running Proton ourselves.
    Steam,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchModdedArgs {
    pub game_exe: String,
    pub profile_id: String,
    pub profile_path: String,
    pub bepinex_dll: String,
    pub dotnet_dir: String,
    pub coreclr_path: String,
    pub platform: String,
    /// Whether an already-running instance of this profile may push this
    /// launch onto its own copy of the profile directory. Off when
    /// multi-instance launching is disabled — then a second launch just runs
    /// from the profile like the first.
    pub allow_instance_copy: bool,
    #[cfg(target_os = "linux")]
    pub runner: LinuxRunner,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchVanillaArgs {
    pub game_exe: String,
    pub platform: String,
    #[cfg(target_os = "linux")]
    pub runner: LinuxRunner,
}

#[cfg(not(any(windows, target_os = "linux")))]
fn build_game_command(_game_exe: &str) -> AppResult<Command> {
    Err(AppError::Platform(
        "Launching the game is not supported on this platform".to_string(),
    ))
}

#[cfg(windows)]
fn set_dll_directory(path: &str) -> AppResult<()> {
    use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { SetDllDirectoryW(PCWSTR(wide.as_ptr())) }
        .map_err(|e| AppError::process(format!("SetDllDirectory failed: {e}")))
}

#[cfg(any(windows, target_os = "linux"))]
fn build_game_command(
    game_exe: &str,
    #[cfg(target_os = "linux")] runner: &LinuxRunner,
) -> AppResult<Command> {
    #[cfg(windows)]
    {
        Ok(Command::new(game_exe))
    }

    #[cfg(target_os = "linux")]
    {
        const STEAM_RUN: &str = "steam-run";

        let cmd = match runner {
            LinuxRunner::Wine { binary, prefix } => {
                let mut cmd = Command::new(binary);
                cmd.env("WINEPREFIX", prefix).arg(game_exe);
                cmd
            }
            LinuxRunner::Proton {
                binary,
                compat_data_path,
                steam_client_path,
                use_steam_run,
            } => {
                let mut cmd = if *use_steam_run {
                    let mut steam = Command::new(STEAM_RUN);
                    steam.arg(binary);
                    steam
                } else {
                    Command::new(binary)
                };

                cmd.env("STEAM_COMPAT_DATA_PATH", compat_data_path)
                    .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", steam_client_path)
                    .env("WINEPREFIX", format!("{compat_data_path}/pfx"))
                    .arg("waitforexitandrun")
                    .arg(game_exe);
                cmd
            }
            // Steam launches branch to `steam -applaunch` before reaching here.
            LinuxRunner::Steam => unreachable!("Steam launches via steam -applaunch"),
        };

        Ok(cmd)
    }
}

#[cfg(target_os = "linux")]
fn to_wine_path(path: &str) -> String {
    if path.starts_with('/') {
        format!("Z:{}", path.replace('/', "\\"))
    } else {
        path.to_string()
    }
}

#[cfg(target_os = "linux")]
fn prepare_linux_winhttp_proxy(game_dir: &Path, profile_path: &str) -> AppResult<()> {
    let profile_dir = PathBuf::from(profile_path);
    let src_dll = profile_dir.join("winhttp.dll");
    let dst_dll = game_dir.join("winhttp.dll");
    let dst_ini = game_dir.join("doorstop_config.ini");

    if !src_dll.exists() {
        return Err(AppError::validation(
            "winhttp.dll not found in profile. Please wait for BepInEx installation to complete.",
        ));
    }

    fs::copy(&src_dll, &dst_dll)?;

    if dst_ini.exists() {
        fs::remove_file(dst_ini)?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn cleanup_linux_doorstop_files(game_dir: &Path) -> AppResult<()> {
    let dll_path = game_dir.join("winhttp.dll");
    let ini_path = game_dir.join("doorstop_config.ini");

    if dll_path.exists() {
        fs::remove_file(dll_path)?;
    }
    if ini_path.exists() {
        fs::remove_file(ini_path)?;
    }
    Ok(())
}

/// Borrow auth arguments from an Epic-launcher-started instance (see
/// [`crate::backend::services::epic_launch_service`]) and append them
/// verbatim, so our locally spawned copy authenticates like a normal Epic
/// launch.
#[cfg(windows)]
fn attach_epic_launch_args(cmd: &mut Command, platform: &str) -> AppResult<()> {
    use std::os::windows::process::CommandExt as _;

    if platform != "epic" {
        return Ok(());
    }

    let args = crate::backend::services::epic_launch_service::acquire_launch_args()?;
    cmd.raw_arg(args);
    Ok(())
}

/// Epic auth-argument capture needs the Epic launcher, which only exists on
/// Windows. Elsewhere the game launches without auth arguments.
#[cfg(not(windows))]
fn attach_epic_launch_args(_cmd: &mut Command, _platform: &str) -> AppResult<()> {
    Ok(())
}

/// Resolve the Xbox app id, caching it in settings so the PowerShell lookup
/// only runs once.
#[cfg(windows)]
fn ensure_xbox_app_id(settings: &core_service::AppSettings) -> AppResult<String> {
    if let Some(app_id) = settings
        .xbox_app_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(app_id.to_string());
    }

    let app_id = xbox_service::get_xbox_app_id()?;
    core_service::update_settings(core_service::AppSettingsPatch {
        xbox_app_id: Some(Some(app_id.clone())),
        ..Default::default()
    })?;
    Ok(app_id)
}

fn launch_process(
    mut cmd: Command,
    profile_id: Option<String>,
    instance: LaunchInstance,
) -> AppResult<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .map_err(|e| AppError::process(format!("Failed to launch game: {e}")))?;
    game_runtime::register_launched_process(child, profile_id, instance)
}

/// Pick the directory this launch runs from: the profile itself when no
/// instance of it is running, otherwise a fresh copy in the lowest free slot.
/// Runs under [`LAUNCH_LOCK`] so concurrent launches can't claim the same slot.
fn prepare_launch_dir(args: &LaunchModdedArgs) -> AppResult<(PathBuf, LaunchInstance)> {
    let profile_dir = PathBuf::from(&args.profile_path);
    if !args.allow_instance_copy {
        return Ok((profile_dir, LaunchInstance::default()));
    }

    let used = game_runtime::used_instance_slots(&args.profile_id);
    let slot = (0..).find(|slot| !used.contains(slot)).expect("free slot");
    if slot == 0 {
        return Ok((profile_dir, LaunchInstance::default()));
    }

    let copy = profile_instance_service::create(&args.profile_id, &profile_dir, slot)?;
    Ok((
        copy.clone(),
        LaunchInstance {
            slot,
            temporary_dir: Some(copy),
        },
    ))
}

/// Rebase a path that sits inside the profile directory onto the directory
/// this launch actually runs from (an instance copy mirrors the profile's
/// layout, so the tail of the path is unchanged).
fn rebase_into_launch_dir(path: &str, profile_dir: &Path, launch_dir: &Path) -> String {
    Path::new(path)
        .strip_prefix(profile_dir)
        .map(|tail| launch_dir.join(tail))
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .to_string()
}

/// Among Us' Steam app id.
const STEAM_APP_ID: &str = "945360";

/// Drop a `steam_appid.txt` next to the game so Steamworks can identify the app
/// when the game isn't started by the Steam client itself (modded launches run
/// the exe directly). Steam-only; other platforms ignore the file. Best-effort:
/// a read-only game dir shouldn't block the launch.
fn ensure_steam_appid_file(game_dir: &Path) {
    let path = game_dir.join("steam_appid.txt");
    if fs::read_to_string(&path).is_ok_and(|s| s.trim() == STEAM_APP_ID) {
        return;
    }
    if let Err(e) = fs::write(&path, STEAM_APP_ID) {
        debug!("failed to write {}: {e}", path.display());
    }
}

/// Write a Doorstop `doorstop_config.ini` into the game dir. Used by the
/// Steam-launch path, where we can't pass `--doorstop-*` args on the command
/// line — Doorstop reads this file at startup instead. Paths are wine paths.
#[cfg(target_os = "linux")]
fn write_doorstop_ini(
    game_dir: &Path,
    target_assembly: &str,
    corlib_dir: &str,
    coreclr_path: &str,
) -> AppResult<()> {
    let ini = format!(
        "[General]\n\
         enabled = true\n\
         target_assembly = {target_assembly}\n\
         \n\
         [Il2Cpp]\n\
         coreclr_path = {coreclr_path}\n\
         corlib_dir = {corlib_dir}\n"
    );
    fs::write(game_dir.join("doorstop_config.ini"), ini)?;
    Ok(())
}

/// Write a disabled `doorstop_config.ini` so a vanilla Steam launch can't
/// inject mods even when the winhttp.dll proxy is still present (the Steam
/// launch option loads it unconditionally).
#[cfg(target_os = "linux")]
fn clear_doorstop_ini(game_dir: &Path) -> AppResult<()> {
    fs::write(
        game_dir.join("doorstop_config.ini"),
        "[General]\nenabled = false\n",
    )?;
    Ok(())
}

/// Spawn `steam -applaunch` and reap the short-lived invoker in the background.
/// Steam reparents the actual game, so we don't track this child — running
/// state is watched separately via [`game_runtime::register_steam_launch`].
#[cfg(target_os = "linux")]
fn spawn_steam(mut cmd: Command) -> AppResult<()> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::process(format!("Failed to launch via Steam: {e}")))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Modded launch handed to the Steam client instead of running Proton
/// ourselves. Steam owns the process, so Steamworks initializes (online play)
/// and the game runs inside the Steam Linux Runtime (audio). Doorstop config
/// goes through `doorstop_config.ini` since `steam -applaunch` can't forward
/// `--doorstop-*` args.
#[cfg(target_os = "linux")]
fn launch_modded_via_steam(args: &LaunchModdedArgs, game_dir: &Path) -> AppResult<()> {
    prepare_linux_winhttp_proxy(game_dir, &args.profile_path)?;
    write_doorstop_ini(
        game_dir,
        &to_wine_path(&args.bepinex_dll),
        &to_wine_path(&args.dotnet_dir),
        &to_wine_path(&args.coreclr_path),
    )?;

    let mut cmd = Command::new("steam");
    cmd.arg("-applaunch")
        .arg(STEAM_APP_ID)
        // Only reaches Proton if this call cold-starts Steam. If Steam is
        // already running, set this once in the game's Steam launch options:
        //   WINEDLLOVERRIDES="winhttp=n,b" %command%
        .env("WINEDLLOVERRIDES", "winhttp=n,b");
    spawn_steam(cmd)?;

    game_runtime::register_steam_launch(Some(args.profile_id.clone()))
}

pub fn launch_modded(args: LaunchModdedArgs) -> AppResult<()> {
    info!("game_launch_modded: game_exe={}", args.game_exe);

    let game_dir = PathBuf::from(&args.game_exe)
        .parent()
        .ok_or_else(|| AppError::validation("Invalid game path"))?
        .to_path_buf();

    // Hold the launch lock from prep through spawn (see LAUNCH_LOCK).
    let cancel_gen = cancel_generation(&args.profile_id);
    let _launch_guard = LAUNCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if cancel_generation(&args.profile_id) != cancel_gen {
        info!("launch cancelled while queued: profile={}", args.profile_id);
        return Ok(());
    }

    // Steam runner: let the Steam client launch the game so online play
    // (Steamworks) and audio (Steam Linux Runtime) work, injecting via
    // doorstop_config.ini instead of command-line args. Steam only ever runs
    // one instance, so this path never uses an instance copy.
    #[cfg(target_os = "linux")]
    if matches!(args.runner, LinuxRunner::Steam) {
        return launch_modded_via_steam(&args, &game_dir);
    }

    // Second and later concurrent launches of this profile run from their own
    // copy of it, so the instances don't fight over BepInEx's cache/interop.
    let profile_dir = PathBuf::from(&args.profile_path);
    let (launch_dir, instance) = prepare_launch_dir(&args)?;
    let bepinex_dll = rebase_into_launch_dir(&args.bepinex_dll, &profile_dir, &launch_dir);
    let dotnet_dir = rebase_into_launch_dir(&args.dotnet_dir, &profile_dir, &launch_dir);
    let coreclr_path = rebase_into_launch_dir(&args.coreclr_path, &profile_dir, &launch_dir);
    let launch_dir_str = launch_dir.to_string_lossy().to_string();

    // An instance copy is only useful to the process it was made for: drop it
    // again if the spawn fails.
    let copy_to_clean_up = instance.temporary_dir.clone();
    let result = spawn_modded(
        &args,
        &game_dir,
        &launch_dir_str,
        LaunchPaths {
            bepinex_dll,
            dotnet_dir,
            coreclr_path,
        },
        instance,
    );
    if result.is_err()
        && let Some(directory) = &copy_to_clean_up
    {
        profile_instance_service::release(directory);
    }
    result
}

/// Doorstop paths, already rebased onto the directory the launch runs from.
struct LaunchPaths {
    bepinex_dll: String,
    dotnet_dir: String,
    coreclr_path: String,
}

fn spawn_modded(
    args: &LaunchModdedArgs,
    game_dir: &Path,
    launch_dir: &str,
    paths: LaunchPaths,
    instance: LaunchInstance,
) -> AppResult<()> {
    #[cfg(windows)]
    set_dll_directory(launch_dir)?;

    #[cfg(target_os = "linux")]
    prepare_linux_winhttp_proxy(game_dir, launch_dir)?;

    let mut cmd = build_game_command(
        &args.game_exe,
        #[cfg(target_os = "linux")]
        &args.runner,
    )?;

    #[cfg(target_os = "linux")]
    let bepinex_dll = to_wine_path(&paths.bepinex_dll);
    #[cfg(not(target_os = "linux"))]
    let bepinex_dll = paths.bepinex_dll;

    #[cfg(target_os = "linux")]
    let dotnet_dir = to_wine_path(&paths.dotnet_dir);
    #[cfg(not(target_os = "linux"))]
    let dotnet_dir = paths.dotnet_dir;

    #[cfg(target_os = "linux")]
    let coreclr_path = to_wine_path(&paths.coreclr_path);
    #[cfg(not(target_os = "linux"))]
    let coreclr_path = paths.coreclr_path;

    cmd.current_dir(game_dir)
        .args(["--doorstop-enabled", "true"])
        .args(["--doorstop-target-assembly", &bepinex_dll])
        .args(["--doorstop-clr-corlib-dir", &dotnet_dir])
        .args(["--doorstop-clr-runtime-coreclr-path", &coreclr_path]);

    #[cfg(target_os = "linux")]
    {
        cmd.env("WINEDLLOVERRIDES", "winhttp=n,b");
    }

    attach_epic_launch_args(&mut cmd, &args.platform)?;
    launch_process(cmd, Some(args.profile_id.clone()), instance)
}

pub fn launch_vanilla(args: LaunchVanillaArgs) -> AppResult<()> {
    info!("game_launch_vanilla: game_exe={}", args.game_exe);

    let game_dir = PathBuf::from(&args.game_exe)
        .parent()
        .ok_or_else(|| AppError::validation("Invalid game path"))?
        .to_path_buf();

    // Steam runner: disable doorstop via the ini (clears any prior modded
    // config) and hand the launch to the Steam client.
    #[cfg(target_os = "linux")]
    if matches!(args.runner, LinuxRunner::Steam) {
        clear_doorstop_ini(&game_dir)?;
        let mut cmd = Command::new("steam");
        cmd.arg("-applaunch").arg(STEAM_APP_ID);
        spawn_steam(cmd)?;
        return game_runtime::register_steam_launch(None);
    }

    // Strip any modded-launch leftovers from the game directory so the
    // doorstop loader can't accidentally inject a previous profile's
    // BepInEx into a vanilla wine/proton session.
    #[cfg(target_os = "linux")]
    cleanup_linux_doorstop_files(&game_dir)?;

    let mut cmd = build_game_command(
        &args.game_exe,
        #[cfg(target_os = "linux")]
        &args.runner,
    )?;

    cmd.current_dir(&game_dir)
        .args(["--doorstop-enabled", "false"]);

    attach_epic_launch_args(&mut cmd, &args.platform)?;
    launch_process(cmd, None, LaunchInstance::default())
}

/// Self-contained vanilla launch: reads app settings, resolves the game
/// path and platform, builds the Linux runner if needed, and dispatches
/// [`launch_vanilla`]. Vanilla launches are profile-less by design.
pub fn launch_vanilla_from_settings() -> AppResult<()> {
    let settings = core_service::get_settings()?;
    let game_path = settings.among_us_path.trim();
    if game_path.is_empty() {
        return Err(AppError::validation(
            "Among Us path is not set. Configure it in Settings.",
        ));
    }

    let game_exe = PathBuf::from(game_path).join(GAME_EXE_NAME);
    if !game_exe.exists() {
        return Err(AppError::validation(format!(
            "{GAME_EXE_NAME} not found at {}",
            game_exe.display()
        )));
    }

    #[cfg(windows)]
    if matches!(settings.game_platform, core_service::GamePlatform::Xbox) {
        let app_id = ensure_xbox_app_id(&settings)?;
        xbox_service::cleanup_xbox_files(game_exe.parent().expect("game_exe has a parent"))?;
        return xbox_service::launch_xbox(&app_id);
    }

    let platform = match settings.game_platform {
        core_service::GamePlatform::Steam => "steam",
        core_service::GamePlatform::Epic => "epic",
        core_service::GamePlatform::Xbox => "xbox",
    }
    .to_string();

    if matches!(settings.game_platform, core_service::GamePlatform::Steam) {
        ensure_steam_appid_file(game_exe.parent().expect("game_exe has a parent"));
    }

    #[cfg(target_os = "linux")]
    let runner = build_linux_runner_from_settings(&settings)?;

    launch_vanilla(LaunchVanillaArgs {
        game_exe: game_exe.to_string_lossy().to_string(),
        platform,
        #[cfg(target_os = "linux")]
        runner,
    })
}

#[cfg(target_os = "linux")]
fn build_linux_runner_from_settings(
    settings: &crate::backend::services::core_service::AppSettings,
) -> AppResult<LinuxRunner> {
    use crate::backend::services::core_service::LinuxRunnerKind;

    // Steam runs through the Steam client, so it needs no runner binary.
    if matches!(settings.linux_runner_kind, LinuxRunnerKind::Steam) {
        return Ok(LinuxRunner::Steam);
    }

    let binary = settings.linux_runner_binary.trim();
    if binary.is_empty() {
        return Err(AppError::validation(
            "Linux runner binary is required in Settings.",
        ));
    }
    Ok(match settings.linux_runner_kind {
        LinuxRunnerKind::Wine => LinuxRunner::Wine {
            binary: binary.to_string(),
            prefix: settings.linux_wine_prefix.clone(),
        },
        LinuxRunnerKind::Proton => LinuxRunner::Proton {
            binary: binary.to_string(),
            compat_data_path: settings.linux_proton_compat_data_path.clone(),
            steam_client_path: settings.linux_proton_steam_client_path.clone(),
            use_steam_run: settings.linux_proton_use_steam_run,
        },
        LinuxRunnerKind::Steam => unreachable!("handled above"),
    })
}

const GAME_EXE_NAME: &str = "Among Us.exe";

#[cfg(any(windows, target_os = "linux"))]
const CORECLR_FILE: &str = "coreclr.dll";
#[cfg(target_os = "macos")]
const CORECLR_FILE: &str = "libcoreclr.dylib";

/// Self-contained modded launch for the given profile. Reads app settings,
/// validates the game executable, BepInEx DLL, and dotnet runtime, then
/// dispatches [`launch_modded`].
pub fn launch_modded_for_profile(profile: ProfileEntry) -> AppResult<()> {
    let settings = core_service::get_settings()?;
    let game_path = settings.among_us_path.trim();
    if game_path.is_empty() {
        return Err(AppError::validation(
            "Among Us path is not set. Configure it in Settings.",
        ));
    }

    let game_exe = PathBuf::from(game_path).join(GAME_EXE_NAME);
    if !game_exe.exists() {
        return Err(AppError::validation(format!(
            "{GAME_EXE_NAME} not found at {}",
            game_exe.display()
        )));
    }

    let profile_path = PathBuf::from(&profile.path);
    let bepinex_dll = profile_path
        .join("BepInEx")
        .join("core")
        .join("BepInEx.Unity.IL2CPP.dll");
    if !bepinex_dll.exists() {
        return Err(AppError::validation(
            "BepInEx DLL not found. Install BepInEx for this profile first.",
        ));
    }
    let dotnet_dir = profile_path.join("dotnet");
    let coreclr_path = dotnet_dir.join(CORECLR_FILE);
    if !coreclr_path.exists() {
        return Err(AppError::validation(format!(
            "dotnet runtime not found at {}",
            coreclr_path.display()
        )));
    }

    #[cfg(windows)]
    if matches!(settings.game_platform, core_service::GamePlatform::Xbox) {
        let app_id = ensure_xbox_app_id(&settings)?;
        let game_dir = game_exe.parent().expect("game_exe has a parent");
        xbox_service::prepare_xbox_launch(&profile_path, game_dir)?;
        xbox_service::launch_xbox(&app_id)?;
        if let Err(e) = crate::backend::services::profile_service::update_last_launched(&profile.id)
        {
            debug!("update_last_launched failed for Xbox launch: {e}");
        } else {
            crate::backend::events::publish(
                crate::backend::events::BackendEvent::ProfileStatsUpdated(profile.id.clone()),
            );
        }
        return Ok(());
    }

    let platform = match settings.game_platform {
        core_service::GamePlatform::Steam => "steam",
        core_service::GamePlatform::Epic => "epic",
        core_service::GamePlatform::Xbox => "xbox",
    }
    .to_string();

    if matches!(settings.game_platform, core_service::GamePlatform::Steam) {
        ensure_steam_appid_file(game_exe.parent().expect("game_exe has a parent"));
    }

    #[cfg(target_os = "linux")]
    let runner = build_linux_runner_from_settings(&settings)?;

    launch_modded(LaunchModdedArgs {
        game_exe: game_exe.to_string_lossy().to_string(),
        profile_id: profile.id.clone(),
        profile_path: profile.path.clone(),
        bepinex_dll: bepinex_dll.to_string_lossy().to_string(),
        dotnet_dir: dotnet_dir.to_string_lossy().to_string(),
        coreclr_path: coreclr_path.to_string_lossy().to_string(),
        platform,
        // A launch that lands on an already-running profile only gets its own
        // copy of it when multiple instances are allowed.
        allow_instance_copy: settings.allow_multi_instance_launch,
        #[cfg(target_os = "linux")]
        runner,
    })
}
