//! Sběrnice událostí (pro WebSocket API, logování, exporty).

use crate::model::QueueItem;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TrackStarted { item: QueueItem, channel: String },
    TrackEnded { item_id: String, played_ms: i64 },
    QueueChanged,
    ModeChanged { mode: String },
    LibraryChanged { added: usize, updated: usize, missing: usize },
    AnalysisProgress { pending: usize },
    EncoderStatus { id: String, state: String, message: String },
    /// Upozornění pro obsluhu. `code` je klíč pro překlad v UI (cs/en).
    Alert { level: String, code: String, message: String },
    ConfigChanged,
}

#[derive(Clone, Default)]
pub struct EventBus {
    subs: Arc<Mutex<Vec<Sender<Event>>>>,
}

impl EventBus {
    pub fn new() -> Self { Self::default() }

    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = bounded(2048);
        self.subs.lock().push(tx);
        rx
    }

    pub fn emit(&self, ev: Event) {
        let mut subs = self.subs.lock();
        subs.retain(|s| match s.try_send(ev.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn alert(&self, level: &str, code: &str, message: impl Into<String>) {
        let message = message.into();
        match level { "error" => log::error!("{code}: {message}"), "warning" => log::warn!("{code}: {message}"), _ => log::info!("{code}: {message}") }
        self.emit(Event::Alert { level: level.into(), code: code.into(), message });
    }
}
