//! Media playback state representation, broadcast hub, and Companion Link OPACK
//! / NSKeyedArchiver serialization for Apple TV Remote (iOS 18+ and iOS 27+).

use std::sync::Arc;
use plist::{Dictionary, Uid, Value as PlistValue};
use tokio::sync::{broadcast, RwLock};

use crate::opack::Value;

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

impl MediaInfo {
    /// Build binary NSKeyedArchiver payload representing `TVRCNowPlayingInfo`
    /// and `TVRCNowPlayingMetadata` for Apple `TVRemoteCore`.
    pub fn build_now_playing_keyed_archive(&self) -> Vec<u8> {
        let has_art = self.artwork.as_ref().map(|b| !b.is_empty()).unwrap_or(false);
        let playback_rate_val = if self.is_playing {
            self.rate.max(1.0) as f64
        } else {
            0.0
        };
        let playback_state_val = if self.is_playing { 1i64 } else { 2i64 }; // 1 = Playing, 2 = Paused
        let bundle_id = if self.app_bundle_id.is_empty() {
            "com.google.android.youtube.tv".to_string()
        } else {
            self.app_bundle_id.clone()
        };
        let title = if self.title.is_empty() {
            "Unknown Title".to_string()
        } else {
            self.title.clone()
        };

        let now_epoch = if self.timestamp > 0.0 {
            self.timestamp
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        };

        // $objects layout:
        // 0: "$null"
        // 1: TVRCNowPlayingInfo dict
        // 2: playbackRate (real)
        // 3: playbackState (int)
        // 4: TVRCNowPlayingMetadata dict
        // 5: title (string)
        // 6: duration (real)
        // 7: timeOffset (real)
        // 8: bundleID (string)
        // 9: timestamp (real)
        // 10: TVRCNowPlayingMetadata class dict
        // 11 (if has_art): imageData (NSData)
        // 11 or 12: TVRCNowPlayingInfo class dict
        let class_meta_idx: u64 = 10;
        let image_data_idx: u64 = if has_art { 11 } else { 0 };
        let class_info_idx: u64 = if has_art { 12 } else { 11 };

        let mut objects: Vec<PlistValue> = Vec::new();
        objects.push(PlistValue::String("$null".into()));

        let mut info_dict = Dictionary::new();
        info_dict.insert("$class".into(), PlistValue::Uid(Uid::new(class_info_idx)));
        info_dict.insert("captionsEnabled".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("expectsTimedMetadata".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("hasValidCaptionOptions".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("identifier".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("imageData".into(), PlistValue::Uid(Uid::new(image_data_idx)));
        info_dict.insert("imageDataIsPlaceholder".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("metadata".into(), PlistValue::Uid(Uid::new(4)));
        info_dict.insert("playbackRate".into(), PlistValue::Uid(Uid::new(2)));
        info_dict.insert("playbackState".into(), PlistValue::Uid(Uid::new(3)));
        info_dict.insert("playerIdentifier".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("rawTimedMetadata".into(), PlistValue::Uid(Uid::new(0)));
        objects.push(PlistValue::Dictionary(info_dict));

        objects.push(PlistValue::Real(playback_rate_val));
        objects.push(PlistValue::Integer(playback_state_val.into()));

        let mut meta_dict = Dictionary::new();
        meta_dict.insert("$class".into(), PlistValue::Uid(Uid::new(class_meta_idx)));
        meta_dict.insert("audioLanguage".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("bundleID".into(), PlistValue::Uid(Uid::new(8)));
        meta_dict.insert("canonicalID".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("currentlyPlayingSongID".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("duration".into(), PlistValue::Uid(Uid::new(6)));
        meta_dict.insert("episodeNumber".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("episodeTitle".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("extendedDescription".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("genre".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("iTunesStoreIdentifier".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("imageURLTemplate".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("isAppleOriginal".into(), PlistValue::Boolean(false));
        meta_dict.insert("kind".into(), PlistValue::Integer(0.into()));
        meta_dict.insert("mainContentStartTime".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("productPageURL".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("programID".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("ratingDescription".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("releaseDate".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("rottenTomatoesReview".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("seasonNumber".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("showID".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("showProductPageURL".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("timeOffset".into(), PlistValue::Uid(Uid::new(7)));
        meta_dict.insert("timestamp".into(), PlistValue::Uid(Uid::new(9)));
        meta_dict.insert("title".into(), PlistValue::Uid(Uid::new(5)));
        objects.push(PlistValue::Dictionary(meta_dict));

        objects.push(PlistValue::String(title));
        objects.push(PlistValue::Real(self.duration));
        objects.push(PlistValue::Real(self.elapsed));
        objects.push(PlistValue::String(bundle_id));
        objects.push(PlistValue::Real(now_epoch));

        let mut class_meta = Dictionary::new();
        class_meta.insert(
            "$classes".into(),
            PlistValue::Array(vec![
                PlistValue::String("TVRCNowPlayingMetadata".into()),
                PlistValue::String("NSObject".into()),
            ]),
        );
        class_meta.insert(
            "$classname".into(),
            PlistValue::String("TVRCNowPlayingMetadata".into()),
        );
        objects.push(PlistValue::Dictionary(class_meta));

        if has_art {
            let art = self.artwork.as_ref().unwrap();
            objects.push(PlistValue::Data(art.clone()));
        }

        let mut class_info = Dictionary::new();
        class_info.insert(
            "$classes".into(),
            PlistValue::Array(vec![
                PlistValue::String("TVRCNowPlayingInfo".into()),
                PlistValue::String("NSObject".into()),
            ]),
        );
        class_info.insert(
            "$classname".into(),
            PlistValue::String("TVRCNowPlayingInfo".into()),
        );
        objects.push(PlistValue::Dictionary(class_info));

        let mut top = Dictionary::new();
        top.insert("root".into(), PlistValue::Uid(Uid::new(1)));

        let mut root = Dictionary::new();
        root.insert("$version".into(), PlistValue::Integer(100000.into()));
        root.insert("$archiver".into(), PlistValue::String("NSKeyedArchiver".into()));
        root.insert("$top".into(), PlistValue::Dictionary(top));
        root.insert("$objects".into(), PlistValue::Array(objects));

        let mut buf = Vec::new();
        let _ = PlistValue::Dictionary(root).to_writer_binary(&mut buf);
        buf
    }

    /// Convert media state to OPACK dictionary for Companion Link `NowPlayingInfo`
    /// events and `FetchCurrentNowPlayingInfoEvent` responses.
    pub fn to_opack_value(&self) -> Value {
        let now_epoch = if self.timestamp > 0.0 {
            self.timestamp
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        };
        let playback_state = if self.is_playing { 1u64 } else { 2u64 };
        let eff_rate = if self.is_playing {
            self.rate.max(1.0) as f64
        } else {
            0.0
        };

        let keyed_archive = self.build_now_playing_keyed_archive();

        let mut entries = vec![
            Value::kv("NowPlayingInfoKey", Value::Bytes(keyed_archive)),
            Value::kv("kMRMediaRemoteNowPlayingInfoTitle", Value::Str(self.title.clone())),
            Value::kv("kMRMediaRemoteNowPlayingInfoArtist", Value::Str(self.artist.clone())),
            Value::kv("kMRMediaRemoteNowPlayingInfoAlbum", Value::Str(self.album.clone())),
            Value::kv("kMRMediaRemoteNowPlayingInfoDuration", Value::Double(self.duration)),
            Value::kv("kMRMediaRemoteNowPlayingInfoElapsedTime", Value::Double(self.elapsed)),
            Value::kv("kMRMediaRemoteNowPlayingInfoPlaybackRate", Value::Double(eff_rate)),
            Value::kv("kMRMediaRemoteNowPlayingInfoPlaybackState", Value::Int(playback_state)),
            Value::kv("kMRMediaRemoteNowPlayingInfoMediaType", Value::Int(2)),
            Value::kv("kMRMediaRemoteNowPlayingInfoIsAlwaysLive", Value::Bool(false)),
            Value::kv("kMRMediaRemoteNowPlayingInfoUniqueIdentifier", Value::Int(1)),
            Value::kv("kMRMediaRemoteNowPlayingInfoTimestamp", Value::Double(now_epoch)),
            Value::kv("kMRMediaRemoteNowPlayingInfoIsMusicApp", Value::Bool(false)),
            Value::kv("kMRMediaRemoteNowPlayingInfoHasArtwork", Value::Bool(self.artwork.is_some())),
            Value::kv("title", Value::Str(self.title.clone())),
            Value::kv("artist", Value::Str(self.artist.clone())),
            Value::kv("album", Value::Str(self.album.clone())),
            Value::kv("duration", Value::Double(self.duration)),
            Value::kv("elapsed", Value::Double(self.elapsed)),
            Value::kv("rate", Value::Double(eff_rate)),
            Value::kv("isPlaying", Value::Bool(self.is_playing)),
            Value::kv("playbackState", Value::Int(playback_state)),
            Value::kv("appName", Value::Str(self.app_name.clone())),
            Value::kv("bundleId", Value::Str(self.app_bundle_id.clone())),
        ];

        if let Some(ref art) = self.artwork {
            if !art.is_empty() {
                entries.push(Value::kv(
                    "kMRMediaRemoteNowPlayingInfoArtworkData",
                    Value::Bytes(art.clone()),
                ));
                entries.push(Value::kv(
                    "kMRMediaRemoteNowPlayingInfoArtworkMIMEType",
                    Value::Str("image/jpeg".into()),
                ));
                entries.push(Value::kv(
                    "kMRMediaRemoteNowPlayingInfoArtworkDataWidth",
                    Value::Int(512),
                ));
                entries.push(Value::kv(
                    "kMRMediaRemoteNowPlayingInfoArtworkDataHeight",
                    Value::Int(512),
                ));
            }
        }

        Value::Dict(entries)
    }
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
