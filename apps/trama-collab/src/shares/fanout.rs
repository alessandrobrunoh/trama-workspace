//! Live fan-out of committed share frames to stream sockets. Delivery is a
//! hint: sockets re-read the log on any gap, so a dropped or reordered
//! frame never loses an event.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use trama_collab_protocol::vnext::api::ShareServerFrame;

const CHANNEL_CAPACITY: usize = 256;

#[derive(Default)]
pub(crate) struct ShareFanout {
    channels: Mutex<HashMap<String, broadcast::Sender<Arc<ShareServerFrame>>>>,
}

impl ShareFanout {
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, broadcast::Sender<Arc<ShareServerFrame>>>> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn subscribe(&self, share_id: &str) -> broadcast::Receiver<Arc<ShareServerFrame>> {
        self.lock()
            .entry(share_id.to_owned())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    pub fn release(&self, share_id: &str) {
        let mut channels = self.lock();
        if channels
            .get(share_id)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            channels.remove(share_id);
        }
    }

    pub fn send(&self, share_id: &str, frames: Vec<ShareServerFrame>) {
        if let Some(sender) = self.lock().get(share_id) {
            for frame in frames {
                let _ = sender.send(Arc::new(frame));
            }
        }
    }
}
