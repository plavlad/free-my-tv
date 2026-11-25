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
    /// and `TVRCNowPlayingMetadata` for Apple `TVRemoteCore` / `TVRemoteUI`.
    ///
    /// Critical iOS 18–27 `TVRemoteUI` requirements:
    /// - `identifier` (`NSString`): non-empty, otherwise `-[TVRUINowPlayingController setNowPlayingInfo:]` returns early.
    /// - `playerIdentifier` (`NSString`): non-empty for `tvrui_hasActivePlayer`.
    /// - `extendedDescription` (`NSString`): non-empty, otherwise `-[TVRCNowPlayingMetadata isMissingCriticalMetadata]` returns YES and hides the player.
    /// - `canonicalID` (`NSString`): non-empty for `tvrui_hasCanonicalID`.
    pub fn build_now_playing_keyed_archive(&self) -> Vec<u8> {
        let has_art = self.artwork.as_ref().map(|b| !b.is_empty()).unwrap_or(false);
        let playback_rate_val = if self.is_playing {
            self.rate.max(1.0) as f64
        } else {
            0.0
        };
        let playback_state_val = if self.title.is_empty() {
            3i64 // Stopped
        } else if self.is_playing {
            1i64 // Playing
        } else {
            2i64 // Paused
        };
        let bundle_id = if self.app_bundle_id.is_empty() {
            "com.google.android.youtube.tv".to_string()
        } else {
            self.app_bundle_id.clone()
        };
        let title = if self.title.is_empty() {
            "Now Playing".to_string()
        } else {
            self.title.clone()
        };
        let artist_or_desc = if !self.artist.is_empty() {
            self.artist.clone()
        } else if !self.album.is_empty() {
            self.album.clone()
        } else if !self.app_name.is_empty() {
            self.app_name.clone()
        } else {
            "Android TV".to_string()
        };
        let genre_str = if !self.artist.is_empty() {
            self.artist.clone()
        } else if !self.app_name.is_empty() {
            self.app_name.clone()
        } else {
            "Media".to_string()
        };
        let track_id = format!("{bundle_id}:{title}");
        let canonical_id = track_id.clone();
        let player_id = bundle_id.clone();

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
        // 2: identifier (NSString)
        // 3: playerIdentifier (NSString)
        // 4: playbackRate (real)
        // 5: playbackState (int)
        // 6: TVRCNowPlayingMetadata dict
        // 7: title (NSString)
        // 8: extendedDescription (NSString)
        // 9: genre (NSString)
        // 10: canonicalID (NSString)
        // 11: bundleID (NSString)
        // 12: duration (real)
        // 13: timeOffset (real)
        // 14: timestamp (real)
        // 15: TVRCNowPlayingMetadata class dict
        // 16 (if has_art): imageData (NSData)
        // 16 or 17: TVRCNowPlayingInfo class dict
        let class_meta_idx: u64 = 15;
        let image_data_idx: u64 = if has_art { 16 } else { 0 };
        let class_info_idx: u64 = if has_art { 17 } else { 16 };

        let mut objects: Vec<PlistValue> = Vec::new();
        objects.push(PlistValue::String("$null".into()));

        let mut info_dict = Dictionary::new();
        info_dict.insert("$class".into(), PlistValue::Uid(Uid::new(class_info_idx)));
        info_dict.insert("captionsEnabled".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("expectsTimedMetadata".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("hasValidCaptionOptions".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("identifier".into(), PlistValue::Uid(Uid::new(2)));
        info_dict.insert("imageData".into(), PlistValue::Uid(Uid::new(image_data_idx)));
        info_dict.insert("imageDataIsPlaceholder".into(), PlistValue::Uid(Uid::new(0)));
        info_dict.insert("metadata".into(), PlistValue::Uid(Uid::new(6)));
        info_dict.insert("playbackRate".into(), PlistValue::Uid(Uid::new(4)));
        info_dict.insert("playbackState".into(), PlistValue::Uid(Uid::new(5)));
        info_dict.insert("playerIdentifier".into(), PlistValue::Uid(Uid::new(3)));
        info_dict.insert("rawTimedMetadata".into(), PlistValue::Uid(Uid::new(0)));
        objects.push(PlistValue::Dictionary(info_dict));

        objects.push(PlistValue::String(track_id));
        objects.push(PlistValue::String(player_id));
        objects.push(PlistValue::Real(playback_rate_val));
        objects.push(PlistValue::Integer(playback_state_val.into()));

        let mut meta_dict = Dictionary::new();
        meta_dict.insert("$class".into(), PlistValue::Uid(Uid::new(class_meta_idx)));
        meta_dict.insert("audioLanguage".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("bundleID".into(), PlistValue::Uid(Uid::new(11)));
        meta_dict.insert("canonicalID".into(), PlistValue::Uid(Uid::new(10)));
        meta_dict.insert("currentlyPlayingSongID".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("duration".into(), PlistValue::Uid(Uid::new(12)));
        meta_dict.insert("episodeNumber".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("episodeTitle".into(), PlistValue::Uid(Uid::new(0)));
        meta_dict.insert("extendedDescription".into(), PlistValue::Uid(Uid::new(8)));
        meta_dict.insert("genre".into(), PlistValue::Uid(Uid::new(9)));
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
        meta_dict.insert("timeOffset".into(), PlistValue::Uid(Uid::new(13)));
        meta_dict.insert("timestamp".into(), PlistValue::Uid(Uid::new(14)));
        meta_dict.insert("title".into(), PlistValue::Uid(Uid::new(7)));
        objects.push(PlistValue::Dictionary(meta_dict));

        objects.push(PlistValue::String(title));
        objects.push(PlistValue::String(artist_or_desc));
        objects.push(PlistValue::String(genre_str));
        objects.push(PlistValue::String(canonical_id));
        objects.push(PlistValue::String(bundle_id));
        objects.push(PlistValue::Real(self.duration));
        objects.push(PlistValue::Real(self.elapsed));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_archive_populates_critical_tvremoteui_fields() {
        let media = MediaInfo {
            title: "Test Video".into(),
            artist: "Channel Author".into(),
            album: "".into(),
            app_name: "org.smarttube.beta".into(),
            app_bundle_id: "org.smarttube.beta".into(),
            duration: 615.0,
            elapsed: 42.5,
            rate: 1.0,
            is_playing: true,
            artwork: Some(vec![0xFF, 0xD8, 0xFF, 0xE0]),
            timestamp: 1795550000.0,
        };

        let archive = media.build_now_playing_keyed_archive();
        let plist_val = PlistValue::from_reader(std::io::Cursor::new(&archive))
            .expect("valid binary plist");
        let root = plist_val.as_dictionary().unwrap();
        let objects = root.get("$objects").unwrap().as_array().unwrap();
        let info = objects[1].as_dictionary().unwrap();

        let resolve_str = |d: &Dictionary, k: &str| -> String {
            let uid = d.get(k).and_then(PlistValue::as_uid).unwrap().get() as usize;
            objects[uid].as_string().unwrap().to_string()
        };

        assert_eq!(
            resolve_str(info, "identifier"),
            "org.smarttube.beta:Test Video"
        );
        assert_eq!(resolve_str(info, "playerIdentifier"), "org.smarttube.beta");

        let meta_uid = info.get("metadata").and_then(PlistValue::as_uid).unwrap().get() as usize;
        let meta = objects[meta_uid].as_dictionary().unwrap();
        assert_eq!(resolve_str(meta, "title"), "Test Video");
        assert_eq!(resolve_str(meta, "extendedDescription"), "Channel Author");
        assert_eq!(resolve_str(meta, "genre"), "Channel Author");
        assert_eq!(
            resolve_str(meta, "canonicalID"),
            "org.smarttube.beta:Test Video"
        );
        assert_eq!(resolve_str(meta, "bundleID"), "org.smarttube.beta");

        let img_uid = info.get("imageData").and_then(PlistValue::as_uid).unwrap().get() as usize;
        assert_eq!(objects[img_uid].as_data().unwrap(), &[0xFF, 0xD8, 0xFF, 0xE0]);
    }
}
