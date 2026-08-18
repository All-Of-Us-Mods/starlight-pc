pub mod api;
pub mod directories;
pub mod error;
pub mod events;
pub mod services;
pub mod single_instance;
pub mod state;

use gpui::App;
use log::debug;

/// Attach a default log-only subscriber to the event bus. Views that care
/// about specific events register their own subscribers via [`events::subscribe`].
pub fn init(cx: &mut App) {
    // Instance copies belong to the process that launched from them, so any
    // still on disk now are leftovers from a crash.
    cx.background_executor()
        .spawn(async { services::profile_instance_service::cleanup_stale_copies() })
        .detach();

    let mut rx = events::subscribe();
    cx.background_executor()
        .spawn(async move {
            while let Ok(event) = rx.recv().await {
                debug!("backend event: {:?}", event);
            }
        })
        .detach();
}
