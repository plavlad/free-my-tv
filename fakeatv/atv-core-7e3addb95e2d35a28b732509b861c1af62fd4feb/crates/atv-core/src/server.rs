//! Server: Dedicated OS TCP acceptors and Bonjour (mDNS) advertisement.
//!
//! Advertises `_companion-link._tcp.local.` (with `rpFl=0x76fc2` so iOS
//! enables Touch, Remote, and Companion immediately without hanging on
//! "Connecting...") and `_mediaremotetv._tcp.local.` (without a fake AirPlay
//! receiver identity so iOS does not pop up AirPlay errors).
//!
//! Uses dedicated blocking OS acceptor threads (`accept4`) instead of
//! edge-triggered epoll `TcpListener::accept` to completely eliminate accept
//! queue starvation on Android TV Wi-Fi drivers.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(target_os = "macos"))]
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use sha2::{Digest, Sha256};

use crate::delegate::{AtvDelegate, EventKind, TrackpadMode};
use crate::error::{Error, Result};
use crate::identity::{self, DeviceIdentity};
use crate::inspector::InspectorHub;
use crate::media::{MediaHub, MediaInfo};
use crate::registry::{ActiveConnectionTracker, DeviceRegistry};
use crate::session::{CompanionSession, DEVICE_MODEL};
use crate::FrameType;

pub const MRP_PORT: u16 = 49152;
pub const COMPANION_PORT: u16 = 49153;
pub const SOURCE_VERSION: &str = "715.2";
pub const DEFAULT_DEVICE_NAME: &str = "Mac Remote";

/// Keyboard focus update pushed from Android Accessibility Service to iOS RTI clients.
#[derive(Debug, Clone)]
pub struct KeyboardFocusEvent {
    pub focused: bool,
    pub text: String,
    pub title: String,
}

/// Query the local Mac's computer name via `scutil --get ComputerName`.
pub fn detect_mac_computer_name() -> Option<String> {
    let name = identity::detect_computer_name();
    if name == DEFAULT_DEVICE_NAME {
        None
    } else {
        Some(name)
    }
}

/// Default device name: uses local Mac computer name on macOS, otherwise "Mac Remote".
pub fn default_device_name() -> String {
    identity::detect_computer_name()
}

/// Server configuration.
#[derive(Debug, Clone)]
pub struct AtvConfig {
    /// Device name shown in Apple TV Remote.
    pub name: String,
    /// Four-digit pairing PIN (0-9999).
    pub pin: u32,
    /// LAN IPv4 address to advertise; auto-detected when None.
    pub ip: Option<Ipv4Addr>,
    /// Trackpad mode: touch drives pointer deltas instead of direction keys (legacy boolean).
    pub mouse_mode: bool,
    /// Operating mode: Direction, Mouse, or Idle (preview only).
    pub trackpad_mode: TrackpadMode,
    /// Port for Debug Web UI & Inspector (None to disable, default Some(8765)).
    pub ui_port: Option<u16>,
    /// Initial mouse speed multiplier (default 0.5).
    pub mouse_speed: f64,
    /// Hardware / MAC-like identifier (e.g. "84:2F:57:2E:6C:EE").
    pub device_id: Option<String>,
    /// Unique UUID (e.g. "F232C260-003A-5396-99D1-F8164BB03DF2").
    pub server_identifier: Option<String>,
    /// Persistent private key seed (32 bytes).
    pub private_key: Option<[u8; 32]>,
}

impl Default for AtvConfig {
    fn default() -> Self {
        let identity = DeviceIdentity::detect();
        let settings = crate::settings::UserSettings::load();
        let initial_mode = settings
            .trackpad_mode
            .as_deref()
            .and_then(TrackpadMode::from_str_opt)
            .unwrap_or_else(|| {
                if settings.mouse_mode.unwrap_or(false) {
                    TrackpadMode::Mouse
                } else {
                    TrackpadMode::Direction
                }
            });
        Self {
            name: identity.name,
            pin: 1111,
            ip: None,
            mouse_mode: initial_mode.is_mouse(),
            trackpad_mode: initial_mode,
            ui_port: Some(8765),
            mouse_speed: settings.mouse_speed,
            device_id: Some(identity.device_id),
            server_identifier: Some(identity.server_identifier),
            private_key: Some(identity.private_key),
        }
    }
}

/// A running Apple TV simulator. Drop or call `stop()` to shut down.
pub struct AtvServer {
    config: AtvConfig,
    identity: DeviceIdentity,
    ip: Ipv4Addr,
    mrp_port: u16,
    companion_port: u16,
    airplay_port: u16,
    ui_port: Option<u16>,
    inspector: Arc<InspectorHub>,
    media_hub: Arc<MediaHub>,
    keyboard_tx: broadcast::Sender<KeyboardFocusEvent>,
    current_keyboard: Arc<Mutex<Option<KeyboardFocusEvent>>>,
    mouse_mode: Arc<AtomicBool>,
    trackpad_mode: Arc<AtomicU8>,
    delegate: Arc<dyn AtvDelegate>,
    running: Arc<AtomicBool>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    acceptor_threads: Vec<(u16, std::thread::JoinHandle<()>)>,
    #[cfg(target_os = "macos")]
    dns_sd_children: Vec<tokio::process::Child>,
    #[cfg(not(target_os = "macos"))]
    mdns: ServiceDaemon,
    #[cfg(not(target_os = "macos"))]
    mdns_fullnames: Vec<String>,
    #[cfg(not(target_os = "macos"))]
    mdns_services: Vec<ServiceInfo>,
}

impl AtvServer {
    /// Start listeners and advertise Bonjour services.
    pub async fn start(config: AtvConfig, delegate: Arc<dyn AtvDelegate>) -> Result<Self> {
        let trackpad_mode = Arc::new(AtomicU8::new(config.trackpad_mode as u8));
        let mouse_mode = Arc::new(AtomicBool::new(config.trackpad_mode.is_mouse()));
        Self::start_with_trackpad_mode(config, delegate, trackpad_mode, mouse_mode).await
    }

    /// Start listeners with a shared atomic mouse_mode boolean for live mode switching.
    pub async fn start_with_mouse_mode(
        config: AtvConfig,
        delegate: Arc<dyn AtvDelegate>,
        mouse_mode: Arc<AtomicBool>,
    ) -> Result<Self> {
        let initial_u8 = if mouse_mode.load(Ordering::SeqCst) {
            TrackpadMode::Mouse as u8
        } else {
            config.trackpad_mode as u8
        };
        let trackpad_mode = Arc::new(AtomicU8::new(initial_u8));
        Self::start_with_trackpad_mode(config, delegate, trackpad_mode, mouse_mode).await
    }

    /// Start listeners with shared atomic mode trackers.
    pub async fn start_with_trackpad_mode(
        config: AtvConfig,
        delegate: Arc<dyn AtvDelegate>,
        trackpad_mode: Arc<AtomicU8>,
        mouse_mode: Arc<AtomicBool>,
    ) -> Result<Self> {
        if config.pin > 9999 {
            return Err(Error::Protocol("pin must be between 0000 and 9999".into()));
        }
        let identity = DeviceIdentity::from_config(&config);
        let mut config = config;
        config.name = identity.name.clone();
        config.device_id = Some(identity.device_id.clone());
        config.server_identifier = Some(identity.server_identifier.clone());
        config.private_key = Some(identity.private_key);

        let ip = match config.ip {
            Some(ip) => ip,
            None => detect_local_ip()?,
        };

        let (mrp_std_listener, mrp_port) = bind_std_with_fallback(MRP_PORT, "MRP")?;
        let (companion_std_listener, companion_port) =
            bind_std_with_fallback(COMPANION_PORT, "Companion")?;
        let (airplay_std_listener, airplay_port) = bind_std_with_fallback(0, "AirPlay")?;

        let inspector = InspectorHub::new();
        let media_hub = MediaHub::new();
        let (keyboard_tx, _) = broadcast::channel(16);
        let current_keyboard = Arc::new(Mutex::new(None));

        let running = Arc::new(AtomicBool::new(true));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut tasks = Vec::new();
        let mut acceptor_threads = Vec::new();

        let (mrp_rx, mrp_thread) =
            spawn_acceptor_thread(mrp_std_listener, "atv-mrp-accept", mrp_port, running.clone());
        acceptor_threads.push((mrp_port, mrp_thread));

        let (companion_rx, companion_thread) = spawn_acceptor_thread(
            companion_std_listener,
            "atv-comp-accept",
            companion_port,
            running.clone(),
        );
        acceptor_threads.push((companion_port, companion_thread));

        let (airplay_rx, airplay_thread) = spawn_acceptor_thread(
            airplay_std_listener,
            "atv-airp-accept",
            airplay_port,
            running.clone(),
        );
        acceptor_threads.push((airplay_port, airplay_thread));

        tasks.push(tokio::spawn(mrp_loop(
            mrp_rx,
            identity.clone(),
            delegate.clone(),
            media_hub.clone(),
            shutdown_rx.clone(),
        )));
        tasks.push(tokio::spawn(airplay_loop(
            airplay_rx,
            identity.clone(),
            shutdown_rx.clone(),
        )));
        tasks.push(tokio::spawn(companion_loop(
            companion_rx,
            identity.clone(),
            config.pin,
            delegate.clone(),
            inspector.clone(),
            media_hub.clone(),
            keyboard_tx.clone(),
            current_keyboard.clone(),
            trackpad_mode.clone(),
            mouse_mode.clone(),
            shutdown_rx.clone(),
        )));

        let mut actual_ui_port = None;
        if let Some(ui_p) = config.ui_port {
            match TcpListener::bind((Ipv4Addr::UNSPECIFIED, ui_p)).await {
                Ok(ui_listener) => {
                    actual_ui_port = Some(ui_p);
                    tasks.push(tokio::spawn(ui_loop(
                        ui_listener,
                        inspector.clone(),
                        delegate.clone(),
                        media_hub.clone(),
                        trackpad_mode.clone(),
                        mouse_mode.clone(),
                        identity.name.clone(),
                        shutdown_rx.clone(),
                    )));
                }
                Err(e) => {
                    warn!("failed to bind Debug UI on port {ui_p}: {e}");
                }
            }
        }

        let rpha = identity.device_id.replace(':', "").to_uppercase();
        let rphn = {
            let mut h = Sha256::new();
            h.update(b"rpHN:");
            h.update(identity.server_identifier.as_bytes());
            hex::encode(&h.finalize()[..6]).to_uppercase()
        };
        let rpad = {
            let mut h = Sha256::new();
            h.update(b"rpAD:");
            h.update(identity.server_identifier.as_bytes());
            hex::encode(&h.finalize()[..6]).to_uppercase()
        };
        let rphi = {
            let mut h = Sha256::new();
            h.update(b"rpHI:");
            h.update(identity.server_identifier.as_bytes());
            hex::encode(&h.finalize()[..6]).to_uppercase()
        };

        // NOTE:
        // 1. `rpFl=0x76fc2` sets bit 18 (Touch), bit 11 (Remote), and bit 6 (Companion).
        //    Without bit 18/11, iOS TVRCRPCompanionLinkClientWrapper never activates
        //    RPHIDTouchSession and hangs indefinitely on "Connecting...".
        // 2. `_mediaremotetv._tcp` intentionally omits `LocalAirPlayReceiverPairingIdentity`
        //    and we do NOT advertise `_airplay._tcp` so iOS never mistakes Android TV for
        //    an AirPlay speaker or displays "Unable to Connect with AirPlay again".
        let services: Vec<(&str, u16, Vec<(&str, &str)>)> = vec![
            (
                "_mediaremotetv._tcp.local.",
                mrp_port,
                vec![
                    ("Name", identity.name.as_str()),
                    ("UniqueIdentifier", identity.unique_id.as_str()),
                    ("SystemBuildVersion", "22K160"),
                    ("ModelName", "Apple TV"),
                    ("AllowPairing", "YES"),
                ],
            ),
            (
                "_companion-link._tcp.local.",
                companion_port,
                vec![
                    ("rpMac", "1"),
                    ("rpHA", rpha.as_str()),
                    ("rpHN", rphn.as_str()),
                    ("rpVr", SOURCE_VERSION),
                    ("rpMd", DEVICE_MODEL),
                    ("rpFl", "0x76fc2"),
                    ("rpAD", rpad.as_str()),
                    ("rpHI", rphi.as_str()),
                    ("rpBA", identity.device_id.as_str()),
                ],
            ),
        ];

        #[cfg(target_os = "macos")]
        let mut dns_sd_children = Vec::new();
        #[cfg(target_os = "macos")]
        {
            for (service_type, port, properties) in &services {
                let dns_sd_type = service_type
                    .trim_end_matches(".local.")
                    .trim_end_matches(".local");
                let mut cmd = tokio::process::Command::new("/usr/bin/dns-sd");
                cmd.arg("-R")
                    .arg(&identity.name)
                    .arg(dns_sd_type)
                    .arg("local.")
                    .arg(port.to_string());
                for (k, v) in properties {
                    cmd.arg(format!("{k}={v}"));
                }
                cmd.stdout(std::process::Stdio::null());
                cmd.stderr(std::process::Stdio::null());
                match cmd.spawn() {
                    Ok(child) => {
                        dns_sd_children.push(child);
                        info!(
                            "registered {}.{service_type} on {ip}:{port} (via macOS mDNSResponder)",
                            identity.name
                        );
                    }
                    Err(e) => {
                        warn!("failed to spawn /usr/bin/dns-sd for {service_type}: {e}");
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        #[cfg(not(target_os = "macos"))]
        let mdns = ServiceDaemon::new().map_err(|e| Error::Mdns(e.to_string()))?;
        #[cfg(not(target_os = "macos"))]
        let mut mdns_fullnames = Vec::new();
        #[cfg(not(target_os = "macos"))]
        let mut mdns_services = Vec::new();
        #[cfg(not(target_os = "macos"))]
        {
            let host_name = format!("{}.local.", identity.name.replace(' ', "-"));
            for (service_type, port, properties) in services {
                let info = ServiceInfo::new(
                    service_type,
                    &identity.name,
                    &host_name,
                    ip.to_string(),
                    port,
                    &properties[..],
                )
                .map_err(|e| Error::Mdns(e.to_string()))?;
                mdns_fullnames.push(info.get_fullname().to_string());
                mdns_services.push(info.clone());
                mdns.register(info).map_err(|e| Error::Mdns(e.to_string()))?;
                info!("registered {}.{service_type} on {ip}:{port}", identity.name);
            }
        }

        info!("fake Apple TV started");
        info!("Name      : \"{}\"", identity.name);
        info!("Device ID : {}", identity.device_id);
        info!("Server ID : {}", identity.server_identifier);
        info!("PIN       : {:04}", config.pin);
        info!("MRP       : {ip}:{mrp_port}");
        info!("Companion : {ip}:{companion_port} (rpFl=0x76fc2)");
        if let Some(port) = actual_ui_port {
            info!("Debug UI  : http://127.0.0.1:{port}");
        }

        Ok(Self {
            config,
            identity,
            ip,
            mrp_port,
            companion_port,
            airplay_port,
            ui_port: actual_ui_port,
            inspector,
            media_hub,
            keyboard_tx,
            current_keyboard,
            mouse_mode,
            trackpad_mode,
            delegate,
            running,
            shutdown_tx,
            tasks,
            acceptor_threads,
            #[cfg(target_os = "macos")]
            dns_sd_children,
            #[cfg(not(target_os = "macos"))]
            mdns,
            #[cfg(not(target_os = "macos"))]
            mdns_fullnames,
            #[cfg(not(target_os = "macos"))]
            mdns_services,
        })
    }

    pub fn config(&self) -> &AtvConfig {
        &self.config
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    pub fn media_hub(&self) -> Arc<MediaHub> {
        self.media_hub.clone()
    }

    pub async fn update_media_info(&self, info: MediaInfo) {
        self.media_hub.update(info).await;
    }

    /// Notify connected iOS Companion clients of Android TV text-field focus state.
    pub fn set_keyboard_focus(&self, focused: bool, text: String, title: String) {
        let evt = KeyboardFocusEvent {
            focused,
            text,
            title,
        };
        let current_kb = self.current_keyboard.clone();
        let tx = self.keyboard_tx.clone();
        let evt_clone = evt.clone();
        if let Ok(mut guard) = current_kb.try_lock() {
            *guard = Some(evt_clone);
        }
        let _ = tx.send(evt);
    }

    /// Re-announce all Bonjour services via mDNS to refresh router IGMP/multicast caches.
    pub fn reannounce_mdns(&self) {
        #[cfg(not(target_os = "macos"))]
        {
            debug!("reannouncing mDNS services (gratuitous refresh)");
            for info in &self.mdns_services {
                if let Err(e) = self.mdns.register(info.clone()) {
                    warn!("failed to reannounce mDNS service {}: {e}", info.get_fullname());
                }
            }
        }
    }

    /// The LAN address the services advertise.
    pub fn ip(&self) -> Ipv4Addr {
        self.ip
    }

    pub fn mrp_port(&self) -> u16 {
        self.mrp_port
    }

    pub fn companion_port(&self) -> u16 {
        self.companion_port
    }

    pub fn airplay_port(&self) -> u16 {
        self.airplay_port
    }

    pub fn ui_port(&self) -> Option<u16> {
        self.ui_port
    }

    pub fn inspector(&self) -> Arc<InspectorHub> {
        self.inspector.clone()
    }

    pub fn trackpad_mode_arc(&self) -> Arc<AtomicU8> {
        self.trackpad_mode.clone()
    }

    pub fn trackpad_mode(&self) -> TrackpadMode {
        TrackpadMode::from_u8(self.trackpad_mode.load(Ordering::SeqCst))
    }

    pub fn set_trackpad_mode(&self, mode: TrackpadMode) {
        self.trackpad_mode.store(mode as u8, Ordering::SeqCst);
        self.mouse_mode.store(mode.is_mouse(), Ordering::SeqCst);
        self.delegate.on_trackpad_mode_changed(mode);
        self.inspector.emit(
            "mode_changed",
            &format!(
                "{{\"mode\":\"{}\",\"mouse_mode\":{}}}",
                mode.as_str(),
                mode.is_mouse()
            ),
        );
        crate::settings::UserSettings::update_trackpad_mode(mode.as_str());
    }

    pub fn cycle_trackpad_mode(&self) -> TrackpadMode {
        let current = self.trackpad_mode();
        let new_mode = current.next();
        self.set_trackpad_mode(new_mode);
        new_mode
    }

    pub fn mouse_mode_arc(&self) -> Arc<AtomicBool> {
        self.mouse_mode.clone()
    }

    pub fn is_mouse_mode(&self) -> bool {
        self.mouse_mode.load(Ordering::SeqCst)
    }

    pub fn is_idle_mode(&self) -> bool {
        self.trackpad_mode.load(Ordering::SeqCst) == TrackpadMode::Idle as u8
    }

    pub fn set_mouse_mode(&self, enabled: bool) {
        self.set_trackpad_mode(if enabled {
            TrackpadMode::Mouse
        } else {
            TrackpadMode::Direction
        });
    }

    pub fn toggle_mouse_mode(&self) -> bool {
        let new_mode = self.cycle_trackpad_mode();
        new_mode.is_mouse()
    }

    /// Unregister Bonjour services and stop all listeners and acceptor threads cleanly.
    pub async fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.shutdown_tx.send(true);

        #[cfg(target_os = "macos")]
        for mut child in self.dns_sd_children {
            let _ = child.kill().await;
        }

        #[cfg(not(target_os = "macos"))]
        {
            for fullname in &self.mdns_fullnames {
                if let Err(e) = self.mdns.unregister(fullname) {
                    warn!("mDNS unregister failed for {fullname}: {e}");
                }
            }
            if let Err(e) = self.mdns.shutdown() {
                warn!("mDNS shutdown failed: {e}");
            }
        }

        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks {
            let _ = task.await;
        }

        // Wake each blocking OS acceptor thread via a quick loopback connect so join() never hangs
        for (port, handle) in self.acceptor_threads {
            let wake_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let _ = std::net::TcpStream::connect_timeout(&wake_addr, Duration::from_millis(100));
            let _ = handle.join();
        }
    }
}

/// Bind `preferred` as a blocking `std::net::TcpListener` with `SO_REUSEADDR`.
/// Falls back to an ephemeral port if `preferred` is actively bound by another listener.
fn bind_std_with_fallback(preferred: u16, label: &str) -> Result<(std::net::TcpListener, u16)> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    let _ = socket.set_reuse_address(true);
    let _ = socket.set_nonblocking(false);

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, preferred);
    match socket.bind(&socket2::SockAddr::from(addr)) {
        Ok(_) => {
            socket.listen(128)?;
            let listener: std::net::TcpListener = socket.into();
            let actual_port = listener.local_addr()?.port();
            info!("{label} bound to port {actual_port} (SO_REUSEADDR active)");
            Ok((listener, actual_port))
        }
        Err(e) => {
            warn!("{label} port {preferred} bind failed ({e}); falling back to ephemeral port");
            let fallback_sock = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?;
            let _ = fallback_sock.set_reuse_address(true);
            let _ = fallback_sock.set_nonblocking(false);
            let fallback_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
            fallback_sock.bind(&socket2::SockAddr::from(fallback_addr))?;
            fallback_sock.listen(128)?;
            let listener: std::net::TcpListener = fallback_sock.into();
            let port = listener.local_addr()?.port();
            warn!("{label} ephemeral port is {port}");
            Ok((listener, port))
        }
    }
}

/// Configures TCP_NODELAY and SO_KEEPALIVE on an accepted `std::net::TcpStream`.
fn configure_std_socket(stream: &std::net::TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        warn!("failed to set TCP_NODELAY: {e}");
    }
    let sock = socket2::SockRef::from(stream);
    #[allow(unused_mut)]
    let mut ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(3));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        ka = ka.with_retries(3);
    }
    let _ = sock.set_tcp_keepalive(&ka);
    let _ = sock.set_keepalive(true);
}

/// Spawns a dedicated blocking OS thread calling `accept()` directly on the kernel socket.
/// Completely eliminates EPOLLET accept-queue starvation on Android TV Wi-Fi stacks.
fn spawn_acceptor_thread(
    listener: std::net::TcpListener,
    name: &'static str,
    port: u16,
    running: Arc<AtomicBool>,
) -> (
    tokio::sync::mpsc::Receiver<(std::net::TcpStream, SocketAddr)>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<(std::net::TcpStream, SocketAddr)>(64);
    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            info!("{name} dedicated OS acceptor thread started on port {port}");
            loop {
                match listener.accept() {
                    Ok((std_stream, peer_addr)) => {
                        if !running.load(Ordering::SeqCst) {
                            debug!("{name}: server stopped, dropping wake socket");
                            break;
                        }
                        configure_std_socket(&std_stream);
                        if let Err(e) = std_stream.set_nonblocking(true) {
                            warn!("{name}: failed to set nonblocking for {peer_addr}: {e}");
                            continue;
                        }
                        if tx.blocking_send((std_stream, peer_addr)).is_err() {
                            debug!("{name}: channel closed, terminating acceptor thread");
                            break;
                        }
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        if !running.load(Ordering::SeqCst) {
                            break;
                        }
                        warn!("{name}: accept error: {e}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            info!("{name} dedicated OS acceptor thread terminated");
        })
        .expect("failed to spawn dedicated OS acceptor thread");
    (rx, handle)
}

/// Return the IPv4 address used to reach the local network.
pub fn detect_local_ip() -> Result<Ipv4Addr> {
    let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    if let Ok(()) = sock.connect((Ipv4Addr::new(192, 0, 2, 1), 9)) {
        if let Ok(addr) = sock.local_addr() {
            if let std::net::IpAddr::V4(ip) = addr.ip() {
                if !ip.is_unspecified() && !ip.is_loopback() {
                    return Ok(ip);
                }
            }
        }
    }
    if let Ok(()) = sock.connect((Ipv4Addr::new(8, 8, 8, 8), 53)) {
        if let Ok(addr) = sock.local_addr() {
            if let std::net::IpAddr::V4(ip) = addr.ip() {
                if !ip.is_unspecified() && !ip.is_loopback() {
                    return Ok(ip);
                }
            }
        }
    }
    Err(Error::Protocol(
        "unable to detect local IPv4 address".into(),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn companion_loop(
    mut rx: tokio::sync::mpsc::Receiver<(std::net::TcpStream, SocketAddr)>,
    identity: DeviceIdentity,
    pin: u32,
    delegate: Arc<dyn AtvDelegate>,
    inspector: Arc<InspectorHub>,
    media_hub: Arc<MediaHub>,
    keyboard_tx: broadcast::Sender<KeyboardFocusEvent>,
    current_keyboard: Arc<Mutex<Option<KeyboardFocusEvent>>>,
    trackpad_mode: Arc<AtomicU8>,
    mouse_mode: Arc<AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let registry = Arc::new(Mutex::new(DeviceRegistry::new(Duration::from_secs(
        24 * 3600,
    ))));
    let active_tracker = Arc::new(Mutex::new(ActiveConnectionTracker::new()));

    loop {
        let (std_stream, peer_addr) = tokio::select! {
            opt = rx.recv() => match opt {
                Some(pair) => pair,
                None => break,
            },
            _ = shutdown.changed() => break,
        };

        let stream = match TcpStream::from_std(std_stream) {
            Ok(s) => s,
            Err(e) => {
                warn!("companion from_std failed for {peer_addr}: {e}");
                continue;
            }
        };

        let identity = identity.clone();
        let delegate = delegate.clone();
        let inspector = inspector.clone();
        let media_hub = media_hub.clone();
        let media_rx = media_hub.subscribe();
        let keyboard_rx = keyboard_tx.subscribe();
        let current_keyboard = current_keyboard.clone();
        let trackpad_mode = trackpad_mode.clone();
        let mouse_mode = mouse_mode.clone();
        let registry = registry.clone();
        let active_tracker = active_tracker.clone();
        let mut client_shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = handle_companion(
                    stream,
                    identity,
                    pin,
                    delegate,
                    inspector,
                    media_hub,
                    media_rx,
                    keyboard_rx,
                    current_keyboard,
                    trackpad_mode,
                    mouse_mode,
                    registry,
                    active_tracker,
                ) => {},
                _ = client_shutdown.changed() => {},
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_companion(
    stream: TcpStream,
    identity: DeviceIdentity,
    pin: u32,
    delegate: Arc<dyn AtvDelegate>,
    inspector: Arc<InspectorHub>,
    media_hub: Arc<MediaHub>,
    mut media_rx: broadcast::Receiver<Arc<MediaInfo>>,
    mut keyboard_rx: broadcast::Receiver<KeyboardFocusEvent>,
    current_keyboard: Arc<Mutex<Option<KeyboardFocusEvent>>>,
    trackpad_mode: Arc<AtomicU8>,
    mouse_mode: Arc<AtomicBool>,
    registry: Arc<Mutex<DeviceRegistry>>,
    active_tracker: Arc<Mutex<ActiveConnectionTracker>>,
) {
    let peer_addr = stream.peer_addr().ok();
    let peer = peer_addr
        .as_ref()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "?".into());
    let peer_ip = peer_addr
        .map(|a| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

    info!("companion client connected: {peer}");
    inspector.add_client(&peer);
    delegate.on_event(EventKind::ClientConnected, &peer);
    inspector.emit("client_connected", &format!("{{\"peer\":\"{peer}\"}}"));

    let session_established = Arc::new(AtomicBool::new(false));
    let pairing_in_progress = Arc::new(AtomicBool::new(false));
    let (conn_id, mut abort_rx) = {
        let mut tracker = active_tracker.lock().await;
        tracker.register(
            peer_ip,
            session_established.clone(),
            pairing_in_progress.clone(),
        )
    };

    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut session = CompanionSession::new(
        writer.clone(),
        delegate.clone(),
        &identity,
        pin,
        mouse_mode,
        trackpad_mode,
        inspector.clone(),
        registry,
        active_tracker.clone(),
        media_hub,
        conn_id,
        peer_ip.to_string(),
        session_established,
        pairing_in_progress,
    );

    // Pre-auth handshake timeout: drop silent/stalled unauthenticated sockets after 8 seconds.
    // Cleared immediately once pair_setup_m1 or pair_verify_m3 completes.
    let timeout_deadline = session.timeout_deadline();
    *timeout_deadline.lock().await = Some(tokio::time::Instant::now() + Duration::from_secs(8));

    // Seed current keyboard focus state if active
    if let Some(evt) = current_keyboard.lock().await.clone() {
        if evt.focused {
            let _ = session.handle_keyboard_focus(evt).await;
        }
    }

    const MAX_FRAME_LENGTH: usize = 2 * 1024 * 1024;
    let mut disconnect_reason = "peer_closed".to_string();

    // Dedicated cancel-safe reader task so concurrent media/keyboard broadcasts
    // inside `tokio::select!` can never drop partial `read_exact` frame bytes.
    let (frame_tx, mut frame_rx) =
        tokio::sync::mpsc::channel::<Result<(FrameType, Vec<u8>, [u8; 4])>>(64);
    let peer_clone = peer.clone();
    let writer_for_probe = writer.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            let mut header = [0u8; 4];
            if let Err(e) = reader.read_exact(&mut header).await {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    let _ = frame_tx
                        .send(Err(Error::Protocol(format!("io:{e}"))))
                        .await;
                }
                break;
            }
            let Some(frame_type) = FrameType::from_u8(header[0]) else {
                // Gracefully reply to stray HTTP/RTSP probes without noisy errors
                if &header[..3] == b"GET"
                    || &header[..4] == b"POST"
                    || &header[..4] == b"OPTI"
                {
                    debug!("companion received stray HTTP probe from {peer_clone}");
                    use tokio::io::AsyncWriteExt;
                    let _ = writer_for_probe
                        .lock()
                        .await
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                } else {
                    let _ = frame_tx
                        .send(Err(Error::Protocol(format!(
                            "unknown frame type {}",
                            header[0]
                        ))))
                        .await;
                }
                break;
            };
            let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
            if length > MAX_FRAME_LENGTH {
                let _ = frame_tx
                    .send(Err(Error::Protocol(format!(
                        "frame too large ({length} bytes)"
                    ))))
                    .await;
                break;
            }
            let mut payload = vec![0u8; length];
            if let Err(e) = reader.read_exact(&mut payload).await {
                let _ = frame_tx
                    .send(Err(Error::Protocol(format!("incomplete_frame:{e}"))))
                    .await;
                break;
            }
            if frame_tx.send(Ok((frame_type, payload, header))).await.is_err() {
                break;
            }
        }
    });

    loop {
        let deadline = *timeout_deadline.lock().await;
        let timeout_fut = async {
            if let Some(dl) = deadline {
                tokio::time::sleep_until(dl).await;
                true
            } else {
                std::future::pending::<bool>().await
            }
        };

        tokio::select! {
            frame = frame_rx.recv() => {
                match frame {
                    Some(Ok((frame_type, payload, header))) => {
                        debug!("companion frame={:?} length={}", frame_type, payload.len());
                        if let Err(e) = session.handle_frame(frame_type, &payload, &header).await {
                            disconnect_reason = format!("error:{e}");
                            warn!("companion error: {e}");
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        disconnect_reason = format!("error:{e}");
                        warn!("companion read error from {peer}: {e}");
                        break;
                    }
                    None => {
                        break;
                    }
                }
            }
            kb = keyboard_rx.recv() => {
                if let Ok(evt) = kb {
                    let _ = session.handle_keyboard_focus(evt).await;
                }
            }
            media = media_rx.recv() => {
                if let Ok(info) = media {
                    let _ = session.handle_media_update(&info).await;
                }
            }
            _ = abort_rx.changed() => {
                disconnect_reason = "superseded".to_string();
                info!("companion connection from {peer} superseded by new remote session");
                break;
            }
            _ = timeout_fut => {
                disconnect_reason = "handshake_timeout".to_string();
                info!("companion connection from {peer} timed out before authentication; closing");
                break;
            }
        }
    }

    reader_task.abort();

    {
        let mut tracker = active_tracker.lock().await;
        tracker.unregister(conn_id);
    }

    info!("companion client disconnected: {peer} ({disconnect_reason})");
    inspector.remove_client(&peer);
    delegate.on_event(
        EventKind::ClientDisconnected,
        &format!("{peer} reason={disconnect_reason}"),
    );
    inspector.emit(
        "client_disconnected",
        &format!("{{\"peer\":\"{peer}\",\"reason\":\"{disconnect_reason}\"}}"),
    );
}

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[allow(clippy::too_many_arguments)]
async fn ui_loop(
    listener: TcpListener,
    inspector: Arc<InspectorHub>,
    delegate: Arc<dyn AtvDelegate>,
    media_hub: Arc<MediaHub>,
    trackpad_mode: Arc<AtomicU8>,
    mouse_mode: Arc<AtomicBool>,
    device_name: String,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    warn!("UI accept failed: {e}");
                    continue;
                }
            },
            _ = shutdown.changed() => break,
        };
        let inspector = inspector.clone();
        let delegate = delegate.clone();
        let media_hub = media_hub.clone();
        let trackpad_mode = trackpad_mode.clone();
        let mouse_mode = mouse_mode.clone();
        let device_name = device_name.clone();
        tokio::spawn(handle_ui_client(
            stream,
            inspector,
            delegate,
            media_hub,
            trackpad_mode,
            mouse_mode,
            device_name,
        ));
    }
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn handle_ui_client(
    mut stream: TcpStream,
    inspector: Arc<InspectorHub>,
    delegate: Arc<dyn AtvDelegate>,
    media_hub: Arc<MediaHub>,
    trackpad_mode: Arc<AtomicU8>,
    mouse_mode: Arc<AtomicBool>,
    device_name: String,
) {
    use tokio::io::AsyncWriteExt;

    let mut buf = vec![0u8; 4096];
    let n = match stream.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let raw_path = parts[1];
    let path = raw_path.split('?').next().unwrap_or(raw_path);

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            let body = DASHBOARD_HTML.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(body).await;
            let _ = stream.flush().await;
        }
        ("GET", "/debug") | ("GET", "/debug/") | ("GET", "/logs") => {
            let resp =
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:8766/\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
        }
        ("GET", "/events") => {
            let header = "HTTP/1.1 200 OK\r\n\
                          Content-Type: text/event-stream\r\n\
                          Cache-Control: no-cache\r\n\
                          Connection: keep-alive\r\n\
                          Access-Control-Allow-Origin: *\r\n\
                          \r\n";
            if stream.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            if stream.flush().await.is_err() {
                return;
            }

            let (history, mut rx) = inspector.subscribe();
            for item in history {
                let msg = format!("data: {item}\n\n");
                if stream.write_all(msg.as_bytes()).await.is_err() {
                    return;
                }
            }
            let _ = stream.flush().await;

            let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Ok(packet) => {
                                let sse = format!("data: {packet}\n\n");
                                if stream.write_all(sse.as_bytes()).await.is_err() {
                                    break;
                                }
                                if stream.flush().await.is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                    _ = ping_interval.tick() => {
                        if stream.write_all(b": ping\n\n").await.is_err() {
                            break;
                        }
                        if stream.flush().await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        ("POST", "/api/action") => {
            if let Some(pos) = req.find("\r\n\r\n") {
                let body = &req[pos + 4..];
                if let Some(button) = extract_button_from_json(body) {
                    if button == "toggle_mode" {
                        let cur_mode = TrackpadMode::from_u8(trackpad_mode.load(Ordering::SeqCst));
                        let new_mode = cur_mode.next();
                        trackpad_mode.store(new_mode as u8, Ordering::SeqCst);
                        mouse_mode.store(new_mode.is_mouse(), Ordering::SeqCst);
                        delegate.on_trackpad_mode_changed(new_mode);
                        inspector.emit(
                            "mode_changed",
                            &format!(
                                "{{\"mode\":\"{}\",\"mouse_mode\":{}}}",
                                new_mode.as_str(),
                                new_mode.is_mouse()
                            ),
                        );
                        crate::settings::UserSettings::update_trackpad_mode(new_mode.as_str());
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{{\"status\":\"ok\",\"mode\":\"{}\",\"mouse_mode\":{}}}",
                            new_mode.as_str(),
                            new_mode.is_mouse()
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                        return;
                    }
                    delegate.on_button(&button);
                    inspector.emit("button", &format!("{{\"name\":\"{button}\"}}"));
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    return;
                }
            }
            let resp = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
        }
        ("POST", "/api/mode") => {
            if let Some(pos) = req.find("\r\n\r\n") {
                let body = &req[pos + 4..];
                let cur_mode = TrackpadMode::from_u8(trackpad_mode.load(Ordering::SeqCst));
                let new_mode = if body.contains("\"idle\"") || body.contains("\"preview\"") {
                    TrackpadMode::Idle
                } else if body.contains("\"mouse\"") || body.contains("true") {
                    TrackpadMode::Mouse
                } else if body.contains("\"direction\"") || body.contains("false") {
                    TrackpadMode::Direction
                } else {
                    cur_mode.next()
                };
                trackpad_mode.store(new_mode as u8, Ordering::SeqCst);
                mouse_mode.store(new_mode.is_mouse(), Ordering::SeqCst);
                delegate.on_trackpad_mode_changed(new_mode);
                inspector.emit(
                    "mode_changed",
                    &format!(
                        "{{\"mode\":\"{}\",\"mouse_mode\":{}}}",
                        new_mode.as_str(),
                        new_mode.is_mouse()
                    ),
                );
                crate::settings::UserSettings::update_trackpad_mode(new_mode.as_str());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{{\"status\":\"ok\",\"mode\":\"{}\",\"mouse_mode\":{}}}",
                    new_mode.as_str(),
                    new_mode.is_mouse()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                return;
            }
            let resp = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
        }
        ("POST", "/api/toggle_mode") => {
            let cur_mode = TrackpadMode::from_u8(trackpad_mode.load(Ordering::SeqCst));
            let new_mode = cur_mode.next();
            trackpad_mode.store(new_mode as u8, Ordering::SeqCst);
            mouse_mode.store(new_mode.is_mouse(), Ordering::SeqCst);
            delegate.on_trackpad_mode_changed(new_mode);
            inspector.emit(
                "mode_changed",
                &format!(
                    "{{\"mode\":\"{}\",\"mouse_mode\":{}}}",
                    new_mode.as_str(),
                    new_mode.is_mouse()
                ),
            );
            crate::settings::UserSettings::update_trackpad_mode(new_mode.as_str());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{{\"status\":\"ok\",\"mode\":\"{}\",\"mouse_mode\":{}}}",
                new_mode.as_str(),
                new_mode.is_mouse()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            return;
        }
        ("POST", "/api/touchpad_settings") | ("POST", "/api/mouse_settings") | ("POST", "/api/speed") => {
            if let Some(pos) = req.find("\r\n\r\n") {
                let body = &req[pos + 4..];
                let (cur_speed, cur_accel, cur_verbose) = delegate.get_touchpad_settings();
                let new_speed = extract_float_from_json(body, "speed")
                    .unwrap_or(cur_speed)
                    .clamp(0.1, 10.0);
                let new_accel = extract_bool_from_json(body, "accel").unwrap_or(cur_accel);
                let new_verbose = extract_bool_from_json(body, "verbose_events")
                    .or_else(|| extract_bool_from_json(body, "verbose"))
                    .unwrap_or(cur_verbose);

                let mut cur_mode = TrackpadMode::from_u8(trackpad_mode.load(Ordering::SeqCst));
                if let Some(m_str) = extract_string_from_json(body, "mode")
                    .or_else(|| extract_string_from_json(body, "trackpad_mode"))
                {
                    if let Some(m) = TrackpadMode::from_str_opt(&m_str) {
                        cur_mode = m;
                        trackpad_mode.store(m as u8, Ordering::SeqCst);
                        mouse_mode.store(m.is_mouse(), Ordering::SeqCst);
                        delegate.on_trackpad_mode_changed(m);
                        inspector.emit(
                            "mode_changed",
                            &format!(
                                "{{\"mode\":\"{}\",\"mouse_mode\":{}}}",
                                m.as_str(),
                                m.is_mouse()
                            ),
                        );
                    }
                } else if let Some(new_mouse) = extract_bool_from_json(body, "mouse_mode") {
                    let m = if new_mouse {
                        TrackpadMode::Mouse
                    } else {
                        TrackpadMode::Direction
                    };
                    cur_mode = m;
                    trackpad_mode.store(m as u8, Ordering::SeqCst);
                    mouse_mode.store(m.is_mouse(), Ordering::SeqCst);
                    delegate.on_trackpad_mode_changed(m);
                    inspector.emit(
                        "mode_changed",
                        &format!(
                            "{{\"mode\":\"{}\",\"mouse_mode\":{}}}",
                            m.as_str(),
                            m.is_mouse()
                        ),
                    );
                }

                delegate.on_touchpad_settings_changed(new_speed, new_accel, new_verbose);
                crate::settings::UserSettings::update_all_with_mode(
                    new_speed,
                    new_accel,
                    new_verbose,
                    Some(cur_mode.as_str()),
                    Some(cur_mode.is_mouse()),
                );
                inspector.emit(
                    "touchpad_settings",
                    &format!(
                        "{{\"mode\":\"{}\",\"mouse_mode\":{},\"speed\":{new_speed:.2},\"accel\":{new_accel},\"verbose_events\":{new_verbose}}}",
                        cur_mode.as_str(),
                        cur_mode.is_mouse()
                    ),
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{{\"status\":\"ok\",\"mode\":\"{}\",\"mouse_mode\":{},\"speed\":{:.2},\"accel\":{},\"verbose_events\":{}}}",
                    cur_mode.as_str(),
                    cur_mode.is_mouse(),
                    new_speed,
                    new_accel,
                    new_verbose
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                return;
            }
            let resp = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
        }
        ("GET", "/api/status") | ("GET", "/api/mode") => {
            let cur_mode = TrackpadMode::from_u8(trackpad_mode.load(Ordering::SeqCst));
            let is_mouse = cur_mode.is_mouse();
            let (speed, accel, verbose_events) = delegate.get_touchpad_settings();
            let screen_json = match delegate.get_screen_size() {
                Some((w, h)) => format!("[{},{}]", w.round() as u32, h.round() as u32),
                None => "null".to_string(),
            };
            let (client_conn, client_peer, session_ready) = inspector.get_client_info();
            let peer_json = match client_peer {
                Some(p) => format!("\"{p}\""),
                None => "null".to_string(),
            };
            let audio_json = match delegate.get_audio_state() {
                Some((vol, muted)) => format!("{{\"volume\":{vol:.2},\"muted\":{muted}}}"),
                None => "null".to_string(),
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{{\"status\":\"ok\",\"server_online\":true,\"mode\":\"{}\",\"mouse_mode\":{},\"device_name\":\"{}\",\"mouse_speed\":{:.2},\"mouse_accel\":{},\"verbose_events\":{},\"screen_size\":{},\"client_connected\":{},\"client_peer\":{},\"session_ready\":{},\"audio\":{}}}",
                cur_mode.as_str(),
                is_mouse,
                device_name,
                speed,
                accel,
                verbose_events,
                screen_json,
                client_conn,
                peer_json,
                session_ready,
                audio_json
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            return;
        }
        ("GET", "/nowplaying") => {
            if let Some(m) = media_hub.get().await {
                let has_art = m.artwork.is_some();
                let title = escape_json(&m.title);
                let artist = escape_json(&m.artist);
                let album = escape_json(&m.album);
                let app_name = escape_json(&m.app_name);
                let app_id = escape_json(&m.app_bundle_id);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{{\"status\":\"ok\",\"has_media\":true,\"media\":{{\"title\":\"{}\",\"artist\":\"{}\",\"album\":\"{}\",\"app_name\":\"{}\",\"app_bundle_id\":\"{}\",\"duration\":{:.3},\"elapsed\":{:.3},\"rate\":{:.2},\"is_playing\":{},\"has_artwork\":{}}}}}",
                    title, artist, album, app_name, app_id, m.duration, m.elapsed, m.rate, m.is_playing, has_art
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            } else {
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{\"status\":\"ok\",\"has_media\":false,\"media\":null}";
                let _ = stream.write_all(resp.as_bytes()).await;
            }
            return;
        }
        ("GET", "/nowplaying/artwork") => {
            if let Some(m) = media_hub.get().await {
                if let Some(art) = m.artwork {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        art.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(&art).await;
                    return;
                }
            }
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
            return;
        }
        ("POST", "/nowplaying/command") => {
            if let Some(pos) = req.find("\r\n\r\n") {
                let body = &req[pos + 4..];
                if let Some(cmd) = extract_string_from_json(body, "command") {
                    delegate.on_media_command(&cmd, 0);
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    return;
                }
            }
            let resp = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
            return;
        }
        ("GET", "/favicon.ico") => {
            let resp = "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
        }
        _ => {
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
        }
    }
}

fn extract_string_from_json(json: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    let idx = json.find(&quoted)?;
    let rest = &json[idx + quoted.len()..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_button_from_json(json: &str) -> Option<String> {
    let key = "\"button\"";
    let idx = json.find(key)?;
    let rest = &json[idx + key.len()..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_float_from_json(json: &str, key: &str) -> Option<f64> {
    let quoted = format!("\"{key}\"");
    let idx = json.find(&quoted)?;
    let rest = &json[idx + quoted.len()..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    let num_str: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    num_str.parse().ok()
}

fn extract_bool_from_json(json: &str, key: &str) -> Option<bool> {
    let quoted = format!("\"{key}\"");
    let idx = json.find(&quoted)?;
    let rest = &json[idx + quoted.len()..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

async fn mrp_loop(
    mut rx: tokio::sync::mpsc::Receiver<(std::net::TcpStream, SocketAddr)>,
    identity: DeviceIdentity,
    delegate: Arc<dyn AtvDelegate>,
    media_hub: Arc<MediaHub>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let (std_stream, peer_addr) = tokio::select! {
            opt = rx.recv() => match opt {
                Some(pair) => pair,
                None => break,
            },
            _ = shutdown.changed() => break,
        };
        let stream = match TcpStream::from_std(std_stream) {
            Ok(s) => s,
            Err(e) => {
                warn!("MRP from_std failed for {peer_addr}: {e}");
                continue;
            }
        };
        let identity = identity.clone();
        let delegate = delegate.clone();
        let media_hub = media_hub.clone();
        let client_shutdown = shutdown.clone();
        tokio::spawn(crate::mrp::handle_mrp_client(
            stream,
            identity,
            delegate,
            media_hub,
            client_shutdown,
        ));
    }
}

async fn airplay_loop(
    mut rx: tokio::sync::mpsc::Receiver<(std::net::TcpStream, SocketAddr)>,
    identity: DeviceIdentity,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let (std_stream, peer_addr) = tokio::select! {
            opt = rx.recv() => match opt {
                Some(pair) => pair,
                None => break,
            },
            _ = shutdown.changed() => break,
        };
        let stream = match TcpStream::from_std(std_stream) {
            Ok(s) => s,
            Err(e) => {
                warn!("AirPlay from_std failed for {peer_addr}: {e}");
                continue;
            }
        };
        tokio::spawn(handle_airplay(stream, identity.clone()));
    }
}

async fn handle_airplay(mut stream: TcpStream, identity: DeviceIdentity) {
    let peer = peer_string(&stream);
    debug!("AirPlay client connected: {peer}");
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);

    let is_rtsp = req.starts_with("GET ") && req.contains("RTSP/1.0");
    let proto = if is_rtsp { "RTSP/1.0" } else { "HTTP/1.1" };

    let mut cseq_header = String::new();
    for line in req.lines() {
        if line.to_ascii_lowercase().starts_with("cseq:") {
            let val = line[5..].trim();
            cseq_header = format!("CSeq: {val}\r\n");
            break;
        }
    }

    let pk = {
        let mut h = sha2::Sha256::new();
        h.update(b"pk:");
        h.update(identity.server_identifier.as_bytes());
        hex::encode(&h.finalize()[..16])
    };

    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <plist version=\"1.0\">\n\
        <dict>\n\
            <key>deviceID</key>\n\
            <string>{}</string>\n\
            <key>features</key>\n\
            <integer>1518338039</integer>\n\
            <key>flags</key>\n\
            <integer>68</integer>\n\
            <key>model</key>\n\
            <string>{}</string>\n\
            <key>name</key>\n\
            <string>{}</string>\n\
            <key>pi</key>\n\
            <string>{}</string>\n\
            <key>pk</key>\n\
            <string>{}</string>\n\
            <key>protocolVersion</key>\n\
            <string>1.1</string>\n\
            <key>sourceVersion</key>\n\
            <string>{}</string>\n\
            <key>statusFlags</key>\n\
            <integer>68</integer>\n\
            <key>vv</key>\n\
            <integer>2</integer>\n\
        </dict>\n\
        </plist>",
        identity.device_id,
        DEVICE_MODEL,
        identity.name,
        identity.server_identifier,
        pk,
        SOURCE_VERSION,
    );

    let response = format!(
        "{proto} 200 OK\r\n\
        {cseq_header}\
        Content-Type: text/x-apple-plist+xml\r\n\
        Content-Length: {}\r\n\
        Server: AirTunes/715.2\r\n\
        \r\n\
        {plist}",
        plist.len()
    );

    use tokio::io::AsyncWriteExt;
    let _ = stream.write_all(response.as_bytes()).await;
}

fn peer_string(stream: &TcpStream) -> String {
    stream
        .peer_addr()
        .map(|a: SocketAddr| a.to_string())
        .unwrap_or_else(|_| "?".into())
}
