// No console window on release builds; debug builds keep it for `cargo run`
// log output. Diagnostics survive via the log file (see `init_logging`).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use gpui::*;

mod app;
mod backend;
mod settings;
mod theme;
mod ui;
mod views;
mod workspace;

use backend::deeplink::{self, DeepLink};
use backend::single_instance;

actions!(starlight, [Quit]);

/// Writes every log line to stderr (visible under `cargo run`) and to the
/// on-disk log file (the only place logs go once the console is hidden).
struct TeeWriter(std::fs::File);

impl std::io::Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        self.0.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        self.0.flush()
    }
}

/// Set up logging to stderr + `{app_data}/logs/starlight.log`, and a panic
/// hook that records the panic (with backtrace) in the same file so users
/// have something to attach to a bug report.
fn init_logging() {
    let log_path = backend::directories::app_data_dir()
        .ok()
        .map(|dir| dir.join("logs").join("starlight.log"));

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Some(path) = &log_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Cheap rotation: keep one previous generation.
        if let Ok(meta) = std::fs::metadata(path)
            && meta.len() > 2 * 1024 * 1024
        {
            let _ = std::fs::rename(path, path.with_extension("log.old"));
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            builder.target(env_logger::Target::Pipe(Box::new(TeeWriter(file))));
        }
    }
    builder.init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!("panic: {info}\n{backtrace}");
        default_hook(info);
    }));
}

/// Act on a parsed deep link. Links that are pure backend work run here;
/// anything that needs the UI goes onto the event bus for the workspace.
fn handle_deep_link(link: DeepLink) {
    match link {
        DeepLink::LaunchProfile(profile_id) => {
            std::thread::spawn(move || launch_profile_by_id(profile_id));
        }
        link => deeplink::dispatch_to_ui(link),
    }
}

/// Parse a `starlight://` URL and act on it, logging links we can't handle.
fn handle_deep_link_url(url: &str) {
    match deeplink::parse(url) {
        Ok(link) => handle_deep_link(link),
        Err(e) => log::warn!("ignoring deep link '{url}': {e}"),
    }
}

/// Launch a profile by id in the background (deep links / forwarded opens).
fn launch_profile_by_id(profile_id: String) {
    use backend::services::{launch_service, profile_service};
    let result = profile_service::get_profile_by_id(&profile_id)
        .and_then(|profile| {
            profile.ok_or_else(|| {
                backend::error::AppError::validation(format!("Profile '{profile_id}' not found"))
            })
        })
        .and_then(launch_service::launch_modded_for_profile);
    if let Err(e) = result {
        log::warn!("deep-link profile launch failed: {e}");
    }
}

fn main() {
    init_logging();
    log::info!("starlight {} starting", env!("CARGO_PKG_VERSION"));

    #[cfg(windows)]
    backend::services::update_service::cleanup_leftover_old_exe();

    #[cfg(windows)]
    if let Err(e) = backend::services::profile_shortcut_service::register_deep_link_scheme() {
        log::warn!("failed to register starlight:// url scheme: {e}");
    }

    // The shell opens the app as `starlight.exe starlight://…` for a registered
    // deep link (desktop shortcuts use `starlight://profile/{id}`). Keep the URL
    // whole: it's forwarded verbatim to a running instance and parsed there.
    let deep_link_url = std::env::args()
        .skip(1)
        .find(|arg| arg.starts_with(&format!("{}://", deeplink::SCHEME)));
    let request = deep_link_url
        .clone()
        .map(single_instance::Message::DeepLink);

    // If an instance is already running, hand it our deep link (or just ask
    // it to come to the front) and exit instead of opening a second window.
    let listener = match single_instance::acquire(request) {
        single_instance::Instance::Forwarded => {
            log::info!("forwarded to the running instance; exiting");
            return;
        }
        single_instance::Instance::Primary(listener) => listener,
    };

    let http = std::sync::Arc::new(
        reqwest_client::ReqwestClient::user_agent("Starlight").expect("http client"),
    );

    gpui_platform::application()
        .with_assets(ui::icon::EmbeddedAssets)
        .with_http_client(http)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
            ui::log_language::register();
            // Settings first — the theme preset comes from them.
            settings::init(cx);
            theme::init(cx);
            backend::init(cx);

            // Serve open/activate requests forwarded by later instances.
            // Launching is thread-safe backend work; window raising goes
            // through the event bus to the workspace.
            if let Some(listener) = listener {
                single_instance::serve(listener, |message| {
                    match message {
                        single_instance::Message::DeepLink(url) => handle_deep_link_url(&url),
                        single_instance::Message::Activate => {}
                    }
                    backend::events::publish(backend::events::BackendEvent::ActivateWindow);
                });
            }

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.set_menus(vec![Menu {
                name: "Starlight".into(),
                disabled: false,
                items: vec![MenuItem::action("Quit Starlight", Quit)],
            }]);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1024.), px(768.)),
                    cx,
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Starlight".into()),
                    ..gpui_component::TitleBar::title_bar_options()
                }),
                // Keep the window from shrinking small enough that the nav +
                // settings sidebars crowd the content off-screen.
                window_min_size: Some(size(px(820.0), px(600.0))),
                app_id: Some("starlight".into()),
                // GPUI defaults to server-side decorations. On Windows and
                // macOS that's what we want — `title_bar_options` makes the
                // native titlebar transparent and we draw into it. On Linux
                // there's no equivalent, so a compositor that honours the
                // request (KWin, wlroots) draws a second titlebar above ours;
                // ask for CSD instead. Both backends degrade on their own if
                // CSD isn't available (X11 without a compositor falls back to
                // server-side).
                #[cfg(target_os = "linux")]
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            };

            cx.open_window(options, |window, cx| {
                let workspace = cx.new(|cx| workspace::Workspace::new(window, cx));
                let workspace_view: AnyView = workspace.into();
                cx.new(|cx| gpui_component::Root::new(workspace_view, window, cx))
            })
            .unwrap();

            // The workspace is already subscribed to the bus, so a link we
            // opened with reaches it exactly like one forwarded by a second
            // instance would.
            if let Some(url) = &deep_link_url {
                handle_deep_link_url(url);
            }
        });
}
