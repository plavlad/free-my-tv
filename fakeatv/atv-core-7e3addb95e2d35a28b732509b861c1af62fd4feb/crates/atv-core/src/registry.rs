//! Device registry and active session tracking for Apple TV Companion protocol.
//!
//! Maintains known devices, their stable `remote_session_id`, and manages
//! concurrent Companion Link connections (such as iOS `sharingd` standby links
//! alongside an active `tvremoted` session) while preempting stale remote
//! sessions on reconnect.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::info;

/// Information about a previously seen device.
#[derive(Clone, Debug)]
pub struct DeviceRecord {
    pub device_id: String,
    pub name: String,
    pub model: String,
    pub remote_session_id: u32,
    pub last_seen: Instant,
}

/// Registry of paired/known companion devices.
/// Retains device state and session IDs across reconnections with a 24-hour TTL.
pub struct DeviceRegistry {
    devices: HashMap<String, DeviceRecord>,
    aliases: HashMap<String, String>,
    ttl: Duration,
}

impl DeviceRegistry {
    pub fn new(ttl: Duration) -> Self {
        Self {
            devices: HashMap::new(),
            aliases: HashMap::new(),
            ttl,
        }
    }

    /// Evicts records that have been idle longer than the TTL.
    pub fn clean_expired(&mut self) {
        let now = Instant::now();
        let ttl = self.ttl;
        let expired: Vec<String> = self
            .devices
            .iter()
            .filter(|(_, d)| now.duration_since(d.last_seen) > ttl)
            .map(|(k, _)| k.clone())
            .collect();

        for k in expired {
            if let Some(dev) = self.devices.remove(&k) {
                info!(
                    "[REGISTRY] expired device {k} ({}) after 24h idle",
                    dev.name
                );
            }
        }
        self.aliases.retain(|_, v| self.devices.contains_key(v));
    }

    /// Looks up an existing device or registers a new one.
    /// Returns `(remote_session_id, is_renewed)`.
    pub fn get_or_register(
        &mut self,
        primary_id: &str,
        alias_id: Option<&str>,
        name: &str,
        model: &str,
    ) -> (u32, bool) {
        self.clean_expired();
        let now = Instant::now();

        let target_key = if self.devices.contains_key(primary_id) {
            Some(primary_id.to_string())
        } else if let Some(canonical) = self.aliases.get(primary_id) {
            Some(canonical.clone())
        } else if let Some(canonical) = alias_id.and_then(|a| self.aliases.get(a)) {
            Some(canonical.clone())
        } else {
            None
        };

        if let Some(key) = target_key {
            let dev = self.devices.get_mut(&key).unwrap();
            dev.last_seen = now;
            if !name.is_empty() {
                dev.name = name.to_string();
            }
            if !model.is_empty() {
                dev.model = model.to_string();
            }
            if let Some(alias) = alias_id {
                self.aliases.insert(alias.to_string(), key.clone());
            }
            info!(
                "[REGISTRY] renewed device {key} ({}) session={}",
                dev.name, dev.remote_session_id
            );
            (dev.remote_session_id, true)
        } else {
            let remote_session_id: u32 = rand::random();
            let record = DeviceRecord {
                device_id: primary_id.to_string(),
                name: name.to_string(),
                model: model.to_string(),
                remote_session_id,
                last_seen: now,
            };
            self.devices.insert(primary_id.to_string(), record);
            if let Some(alias) = alias_id {
                self.aliases.insert(alias.to_string(), primary_id.to_string());
            }
            info!(
                "[REGISTRY] registered new device {primary_id} ({name}) session={remote_session_id}"
            );
            (remote_session_id, false)
        }
    }

    /// Looks up an existing session ID by primary ID, alias ID, or IP address.
    pub fn find_session_id(&mut self, identifier: &str) -> Option<u32> {
        self.clean_expired();
        if let Some(dev) = self.devices.get(identifier) {
            Some(dev.remote_session_id)
        } else if let Some(canonical) = self.aliases.get(identifier) {
            self.devices.get(canonical).map(|d| d.remote_session_id)
        } else {
            None
        }
    }

    /// Associates an IP address alias with a primary device ID.
    pub fn associate_ip(&mut self, ip: &str, primary_id: &str) {
        if self.devices.contains_key(primary_id) {
            self.aliases.insert(ip.to_string(), primary_id.to_string());
        } else if let Some(canonical) = self.aliases.get(primary_id) {
            let canon = canonical.clone();
            self.aliases.insert(ip.to_string(), canon);
        }
    }
}

/// Metadata for an active companion TCP connection.
pub struct ActiveConnection {
    pub id: u64,
    pub peer_ip: IpAddr,
    pub device_id: Option<String>,
    pub session_established: Arc<AtomicBool>,
    pub pairing_in_progress: Arc<AtomicBool>,
    pub abort_tx: watch::Sender<bool>,
}

/// Tracks live companion connections so that:
/// - iOS `sharingd`/`rapportd` background probes NEVER kill an established `tvremoted`
///   remote session or an in-progress PIN pairing flow;
/// - when a new `tvremoted` remote session establishes (`_sessionStart` / `TVRCSessionStart`),
///   any stale/half-open prior remote session from the same device or IP is immediately
///   superseded without delay;
/// - stale idle standby connections beyond a small cap per IP are trimmed to prevent leaks.
#[derive(Default)]
pub struct ActiveConnectionTracker {
    next_id: u64,
    connections: Vec<ActiveConnection>,
}

impl ActiveConnectionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new incoming connection.
    /// Does NOT abort active established remote sessions or active PIN pairing connections
    /// so parallel iOS daemons (`sharingd`, `rapportd`, `tvremoted`) coexist cleanly.
    pub fn register(
        &mut self,
        peer_ip: IpAddr,
        session_established: Arc<AtomicBool>,
        pairing_in_progress: Arc<AtomicBool>,
    ) -> (u64, watch::Receiver<bool>) {
        // Keep at most 2 unestablished standby connections per IP (e.g. sharingd + rapportd),
        // evicting only the oldest unestablished non-pairing standby if exceeded.
        let unestablished_ids: Vec<u64> = self
            .connections
            .iter()
            .filter(|c| {
                c.peer_ip == peer_ip
                    && !c.session_established.load(Ordering::SeqCst)
                    && !c.pairing_in_progress.load(Ordering::SeqCst)
            })
            .map(|c| c.id)
            .collect();

        if unestablished_ids.len() >= 2 {
            let excess = unestablished_ids.len() - 1;
            for old_id in unestablished_ids.into_iter().take(excess) {
                if let Some(conn) = self.connections.iter().find(|c| c.id == old_id) {
                    info!(
                        "[TRACKER] trimming excess standby connection {old_id} from {peer_ip}"
                    );
                    let _ = conn.abort_tx.send(true);
                }
            }
        }

        self.next_id += 1;
        let id = self.next_id;
        let (abort_tx, abort_rx) = watch::channel(false);
        self.connections.push(ActiveConnection {
            id,
            peer_ip,
            device_id: None,
            session_established,
            pairing_in_progress,
            abort_tx,
        });

        (id, abort_rx)
    }

    /// Associates an identified device with a connection.
    /// Does not kill established sessions from other daemons on the same device.
    pub fn set_device_id(&mut self, conn_id: u64, device_id: &str) {
        if let Some(conn) = self.connections.iter_mut().find(|c| c.id == conn_id) {
            conn.device_id = Some(device_id.to_string());
        }
    }

    /// Called when a connection actually starts a remote control session
    /// (`_sessionStart` or `TVRCSessionStart`). Immediately supersedes any older
    /// established remote session from the same device or IP so reconnections take 0ms.
    pub fn on_session_established(&mut self, conn_id: u64) {
        let target = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .map(|c| (c.peer_ip, c.device_id.clone()));

        let Some((peer_ip, device_id)) = target else {
            return;
        };

        for conn in &self.connections {
            if conn.id == conn_id || conn.pairing_in_progress.load(Ordering::SeqCst) {
                continue;
            }
            let same_device = device_id.is_some() && conn.device_id == device_id;
            let same_ip = conn.peer_ip == peer_ip;
            if (same_device || same_ip) && conn.session_established.load(Ordering::SeqCst) {
                info!(
                    "[TRACKER] superseding previous remote session {} for new session {} ({peer_ip})",
                    conn.id, conn_id
                );
                let _ = conn.abort_tx.send(true);
            }
        }
    }

    /// Removes a terminated connection.
    pub fn unregister(&mut self, conn_id: u64) {
        self.connections.retain(|c| c.id != conn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_device_registry_session_renewal_and_aliasing() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(3600));

        let (sid1, renewed1) = reg.get_or_register(
            "EE5426A1-FB95-4329-83A4-41F2D767DC28",
            Some("18:3E:E4:EE:ED:40"),
            "MP",
            "iPhone18,1",
        );
        assert!(!renewed1);

        let (sid2, renewed2) = reg.get_or_register(
            "EE5426A1-FB95-4329-83A4-41F2D767DC28",
            Some("C3:DA:FA:17:6F:DF"),
            "MP",
            "iPhone18,1",
        );
        assert!(renewed2);
        assert_eq!(sid1, sid2, "Session ID must be preserved across reconnections");

        let (sid3, renewed3) = reg.get_or_register("C3:DA:FA:17:6F:DF", None, "MP", "iPhone18,1");
        assert!(renewed3);
        assert_eq!(sid1, sid3, "Alias must point to original session ID");
    }

    #[test]
    fn test_background_probe_does_not_abort_established_remote_session() {
        let mut tracker = ActiveConnectionTracker::new();
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 33, 204));

        // 1. tvremoted connects and establishes session
        let est_remote = Arc::new(AtomicBool::new(true));
        let pair_remote = Arc::new(AtomicBool::new(false));
        let (id_remote, abort_remote) =
            tracker.register(ip, est_remote.clone(), pair_remote.clone());
        tracker.set_device_id(id_remote, "IPHONE-UUID");
        tracker.on_session_established(id_remote);

        // 2. sharingd opens a background probe from the exact same IP
        let est_probe = Arc::new(AtomicBool::new(false));
        let pair_probe = Arc::new(AtomicBool::new(false));
        let (id_probe, abort_probe) = tracker.register(ip, est_probe, pair_probe);
        tracker.set_device_id(id_probe, "IPHONE-UUID");

        // Active remote MUST NOT be killed by sharingd probe!
        assert!(
            !*abort_remote.borrow(),
            "Background probe must NOT abort established tvremoted session"
        );
        assert!(!*abort_probe.borrow());

        // 3. New tvremoted reconnects and establishes a new session -> supersedes old tvremoted
        let est_new = Arc::new(AtomicBool::new(true));
        let pair_new = Arc::new(AtomicBool::new(false));
        let (id_new, abort_new) = tracker.register(ip, est_new, pair_new);
        tracker.set_device_id(id_new, "IPHONE-UUID");
        tracker.on_session_established(id_new);

        assert!(
            *abort_remote.borrow(),
            "Old remote session must be superseded when new remote session establishes"
        );
        assert!(
            !*abort_probe.borrow(),
            "Standby sharingd connection should remain alive for RTI notifications"
        );
        assert!(!*abort_new.borrow());

        tracker.unregister(id_remote);
        tracker.unregister(id_probe);
        tracker.unregister(id_new);
        assert_eq!(tracker.connections.len(), 0);
    }

    #[test]
    fn test_pairing_connection_is_protected_from_eviction() {
        let mut tracker = ActiveConnectionTracker::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 50));

        let est1 = Arc::new(AtomicBool::new(false));
        let pair1 = Arc::new(AtomicBool::new(true)); // PIN pairing in progress
        let (id1, abort1) = tracker.register(ip, est1, pair1);

        // Multiple background probes arrive while user is typing PIN
        let (id2, _) = tracker.register(
            ip,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (id3, _) = tracker.register(
            ip,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (id4, _) = tracker.register(
            ip,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );

        assert!(
            !*abort1.borrow(),
            "In-progress PIN pairing must never be aborted by background connections"
        );

        tracker.unregister(id1);
        tracker.unregister(id2);
        tracker.unregister(id3);
        tracker.unregister(id4);
    }

    #[test]
    fn test_device_registry_ip_association_and_lookup() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(3600));
        let (sid, _) = reg.get_or_register("device-123", None, "Living Room TV", "iPhone16,1");
        reg.associate_ip("192.168.1.100", "device-123");

        assert_eq!(reg.find_session_id("192.168.1.100"), Some(sid));
        assert_eq!(reg.find_session_id("device-123"), Some(sid));
        assert_eq!(reg.find_session_id("192.168.1.200"), None);
    }
}
