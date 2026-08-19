//! Backend → frontend event bus.
//!
//! Backend services publish progress / state events via [`publish`].
//! GPUI views subscribe with [`subscribe`] and `.recv().await` on gpui's
//! background executor.

use async_broadcast::{InactiveReceiver, Receiver, Sender, broadcast};
use std::sync::LazyLock;

use crate::backend::services::bepinex_service::BepInExProgress;
use crate::backend::services::mod_download_service::ModDownloadProgress;
use crate::backend::services::profile_service::ZipProgress;
use crate::backend::state::game_runtime::GameStatePayload;

#[derive(Clone, Debug)]
pub enum BackendEvent {
    BepInExProgress(BepInExProgress),
    ModDownloadProgress(ModDownloadProgress),
    GameStateChanged(GameStatePayload),
    /// Progress of an in-flight profile import/export.
    ZipProgress(ZipProgress),
    /// A profile's persisted stats (last_launched / total_play_time) just
    /// changed. Views can use this to reload the profile entry.
    ProfileStatsUpdated(String),
    /// A second app instance forwarded its startup to us (single-instance
    /// guard) — bring the main window to the front.
    ActivateWindow,
    /// A `starlight://profile/{id}/edit` deep link asked for a profile's page.
    OpenProfilePage(String),
}

const CHANNEL_CAPACITY: usize = 256;

struct Bus {
    tx: Sender<BackendEvent>,
    // Hold an inactive receiver so the channel stays alive even if all
    // subscribers drop. New subscribers can still receive future events.
    _keepalive: InactiveReceiver<BackendEvent>,
}

static EVENT_BUS: LazyLock<Bus> = LazyLock::new(|| {
    let (mut tx, rx) = broadcast::<BackendEvent>(CHANNEL_CAPACITY);
    // Drop the oldest event when full instead of blocking the publisher.
    tx.set_overflow(true);
    Bus {
        tx,
        _keepalive: rx.deactivate(),
    }
});

pub fn publish(event: BackendEvent) {
    // try_broadcast returns Err if there are no subscribers (and we're not
    // keeping a live receiver) — fine, just drop.
    let _ = EVENT_BUS.tx.try_broadcast(event);
}

pub fn subscribe() -> Receiver<BackendEvent> {
    EVENT_BUS.tx.new_receiver()
}
