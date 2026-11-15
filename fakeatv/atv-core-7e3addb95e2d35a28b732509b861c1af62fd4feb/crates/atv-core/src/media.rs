use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Current media playback metadata.
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub app_name: String,
    pub app_bundle_id: String,
    pub duration: f64,
    pub elapsed: f64,
    pub rate: f32,
    pub is_playing: bool,
    pub artwork: Option<Vec<u8>>,
    pub timestamp: f64,
}

/// Central hub managing media state and real-time broadcasting.
pub struct MediaHub {
    current: RwLock<Option<MediaInfo>>,
    tx: broadcast::Sender<Arc<MediaInfo>>,
}

impl MediaHub {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(32);
        Arc::new(Self {
            current: RwLock::new(None),
            tx,
        })
    }

    pub async fn update(&self, info: MediaInfo) {
        let arc_info = Arc::new(info.clone());
        {
            let mut guard = self.current.write().await;
            *guard = Some(info);
        }
        let _ = self.tx.send(arc_info);
    }

    pub async fn get(&self) -> Option<MediaInfo> {
        let guard = self.current.read().await;
        guard.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<MediaInfo>> {
        self.tx.subscribe()
    }
}
