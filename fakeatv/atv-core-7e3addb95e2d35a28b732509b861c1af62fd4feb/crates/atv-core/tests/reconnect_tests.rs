//! Integration tests for FakeATV connection stability, reconnection preemption,
//! MRP handshake isolation under media floods, handshake timeouts, and command responsiveness.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atv_core::crypto::{hkdf_expand, CompanionCipher};
use atv_core::delegate::{AtvDelegate, EventKind, TouchPhase};
use atv_core::media::MediaInfo;
use atv_core::mrp::protobuf::*;
use atv_core::opack::{self, Value};
use atv_core::server::{AtvConfig, AtvServer};
use atv_core::tlv::{self, tags};
use atv_core::FrameType;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

/// Test delegate collecting received buttons and events for assertions.
#[derive(Default)]
struct TestDelegate {
    buttons: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<(EventKind, String)>>>,
    media_commands: Arc<Mutex<Vec<(String, i64)>>>,
}

impl AtvDelegate for TestDelegate {
    fn on_button(&self, name: &str) {
        if let Ok(mut b) = self.buttons.lock() {
            b.push(name.to_string());
        }
    }
    fn on_touch(&self, _dx: f64, _dy: f64, _phase: TouchPhase) {}
    fn on_audio(&self, _volume: f64, _muted: bool) {}
    fn on_event(&self, kind: EventKind, detail: &str) {
        if let Ok(mut e) = self.events.lock() {
            e.push((kind, detail.to_string()));
        }
    }
    fn on_text_input(&self, _text: &str) {}
    fn on_media_command(&self, command: &str, param: i64) {
        if let Ok(mut c) = self.media_commands.lock() {
            c.push((command.to_string(), param));
        }
    }
}

// ---------------------------------------------------------------------------
// MRP Client Helper Functions
// ---------------------------------------------------------------------------

async fn write_varint(stream: &mut TcpStream, mut val: usize) {
    let mut buf = [0u8; 10];
    let mut pos = 0;
    loop {
        if val < 0x80 {
            buf[pos] = val as u8;
            pos += 1;
            break;
        } else {
            buf[pos] = ((val & 0x7F) | 0x80) as u8;
            pos += 1;
            val >>= 7;
        }
    }
    stream.write_all(&buf[..pos]).await.unwrap();
}

async fn read_varint(stream: &mut TcpStream) -> usize {
    let mut shift = 0u32;
    let mut result: usize = 0;
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await.unwrap();
        result |= ((byte[0] & 0x7F) as usize) << shift;
        if byte[0] & 0x80 == 0 {
            return result;
        }
        shift += 7;
    }
}

async fn send_mrp_msg(
    stream: &mut TcpStream,
    msg: &[u8],
    cipher: Option<&mut CompanionCipher>,
) {
    let data = if let Some(c) = cipher {
        c.encrypt(msg, None, None).unwrap()
    } else {
        msg.to_vec()
    };
    write_varint(stream, data.len()).await;
    stream.write_all(&data).await.unwrap();
    stream.flush().await.unwrap();
}

#[allow(dead_code)]
struct OwnedMrpMessage {
    msg_type: u32,
    identifier: String,
    inner_tag: u32,
    inner_data: Vec<u8>,
}

async fn recv_mrp_msg(
    stream: &mut TcpStream,
    cipher: Option<&mut CompanionCipher>,
) -> OwnedMrpMessage {
    let size = read_varint(stream).await;
    let mut body = vec![0u8; size];
    stream.read_exact(&mut body).await.unwrap();
    if let Some(c) = cipher {
        body = c.decrypt(&body, None, None).unwrap();
    }
    let decoded = decode_protocol_message(&body).expect("failed to decode protocol message");
    OwnedMrpMessage {
        msg_type: decoded.msg_type,
        identifier: decoded.identifier,
        inner_tag: decoded.inner_tag,
        inner_data: decoded.inner_data.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Companion Client Helper Functions
// ---------------------------------------------------------------------------

async fn send_companion_frame(stream: &mut TcpStream, frame_type: FrameType, payload: &[u8]) {
    let mut header = [0u8; 4];
    header[0] = frame_type as u8;
    let len = payload.len() as u32;
    header[1] = (len >> 16) as u8;
    header[2] = (len >> 8) as u8;
    header[3] = (len & 0xFF) as u8;
    stream.write_all(&header).await.unwrap();
    if !payload.is_empty() {
        stream.write_all(payload).await.unwrap();
    }
    stream.flush().await.unwrap();
}

async fn recv_companion_frame(stream: &mut TcpStream) -> (FrameType, Vec<u8>) {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await.unwrap();
    let frame_type = FrameType::from_u8(header[0]).expect("valid frame type");
    let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).await.unwrap();
    }
    (frame_type, payload)
}

/// Performs client-side Pair-Verify against the Companion listener and returns
/// the negotiated session cipher.
async fn perform_companion_pair_verify(
    stream: &mut TcpStream,
) -> (CompanionCipher, X25519PublicKey) {
    let client_secret = StaticSecret::random_from_rng(rand::thread_rng());
    let client_pub = X25519PublicKey::from(&client_secret);

    // M1
    let m1_tlv = tlv::write(&[
        (tags::SEQ_NO, &[0x01]),
        (tags::PUBLIC_KEY, client_pub.as_bytes()),
    ]);
    let m1_opack = opack::pack(&Value::dict(vec![Value::kv("_pd", Value::Bytes(m1_tlv))]));
    send_companion_frame(stream, FrameType::PvStart, &m1_opack).await;

    // M2
    let (ft, m2_payload) = recv_companion_frame(stream).await;
    assert_eq!(ft, FrameType::PvNext);
    let (m2_val, _) = opack::unpack(&m2_payload).unwrap();
    let m2_pd = match m2_val.get("_pd") {
        Some(Value::Bytes(b)) => b,
        _ => panic!("missing _pd in M2"),
    };
    let m2_tlv = tlv::read(m2_pd);
    let server_pub_bytes = m2_tlv.get(&tags::PUBLIC_KEY).unwrap();
    let server_pub =
        X25519PublicKey::from(<[u8; 32]>::try_from(server_pub_bytes.as_slice()).unwrap());
    let shared = client_secret.diffie_hellman(&server_pub);

    // M3
    let m3_tlv = tlv::write(&[(tags::SEQ_NO, &[0x03])]);
    let m3_opack = opack::pack(&Value::dict(vec![Value::kv("_pd", Value::Bytes(m3_tlv))]));
    send_companion_frame(stream, FrameType::PvNext, &m3_opack).await;

    // M4
    let (ft, m4_payload) = recv_companion_frame(stream).await;
    assert_eq!(ft, FrameType::PvNext);
    let (m4_val, _) = opack::unpack(&m4_payload).unwrap();
    let m4_pd = match m4_val.get("_pd") {
        Some(Value::Bytes(b)) => b,
        _ => panic!("missing _pd in M4"),
    };
    let m4_tlv = tlv::read(m4_pd);
    assert_eq!(m4_tlv.get(&tags::SEQ_NO), Some(&vec![0x04]));

    // Derive session cipher (client write = ClientEncrypt-main, client read = ServerEncrypt-main)
    let write_key = hkdf_expand("", "ClientEncrypt-main", shared.as_bytes());
    let read_key = hkdf_expand("", "ServerEncrypt-main", shared.as_bytes());
    let cipher = CompanionCipher::new(&write_key, &read_key, 12).unwrap();

    (cipher, server_pub)
}

async fn send_encrypted_companion_opack(
    stream: &mut TcpStream,
    cipher: &mut CompanionCipher,
    val: &Value,
) {
    let plain = opack::pack(val);
    let mut header = [0u8; 4];
    header[0] = FrameType::EOpack as u8;
    let len = (plain.len() + 16) as u32;
    header[1] = (len >> 16) as u8;
    header[2] = (len >> 8) as u8;
    header[3] = (len & 0xFF) as u8;
    let enc = cipher.encrypt(&plain, Some(&header), None).unwrap();
    stream.write_all(&header).await.unwrap();
    stream.write_all(&enc).await.unwrap();
    stream.flush().await.unwrap();
}

async fn recv_encrypted_companion_opack(
    stream: &mut TcpStream,
    cipher: &mut CompanionCipher,
) -> Value {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0], FrameType::EOpack as u8);
    let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
    let mut enc_payload = vec![0u8; len];
    stream.read_exact(&mut enc_payload).await.unwrap();
    let decrypted = cipher.decrypt(&enc_payload, Some(&header), None).unwrap();
    let (val, _) = opack::unpack(&decrypted).unwrap();
    val
}

// ---------------------------------------------------------------------------
// TEST 1: Rapid Reconnect Cycle & Preemption
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rapid_reconnect_preempts_old_session() {
    let delegate = Arc::new(TestDelegate::default());
    let buttons = delegate.buttons.clone();

    let mut config = AtvConfig::default();
    config.name = "Test ATV Preemption".to_string();
    config.ui_port = None; // Disable web UI in test
    config.ip = Some(Ipv4Addr::LOCALHOST);

    let server = AtvServer::start(config, delegate).await.unwrap();
    let companion_port = server.companion_port();

    // 1. Client 1 connects and establishes session
    let mut client1 = TcpStream::connect((Ipv4Addr::LOCALHOST, companion_port))
        .await
        .unwrap();
    let (mut cipher1, _) = perform_companion_pair_verify(&mut client1).await;

    // Send _sessionStart
    let session_start_1 = Value::dict(vec![
        Value::kv("_i", Value::str("_sessionStart")),
        Value::kv("_x", Value::Int(1)),
        Value::kv("_c", Value::dict(vec![Value::kv("_sid", Value::Int(11111))])),
    ]);
    send_encrypted_companion_opack(&mut client1, &mut cipher1, &session_start_1).await;
    let resp1 = recv_encrypted_companion_opack(&mut client1, &mut cipher1).await;
    let sid1 = resp1
        .get("_c")
        .and_then(|c| c.get("_sid"))
        .and_then(Value::as_int)
        .expect("sid1 in response");

    // 2. Client 1 stops reading / drops abruptly without FIN or _sessionStop
    // In TCP, dropping the stream drops the handle.
    drop(client1);

    // 3. Client 2 reconnects immediately from the SAME IP
    let mut client2 = TcpStream::connect((Ipv4Addr::LOCALHOST, companion_port))
        .await
        .unwrap();
    let (mut cipher2, _) = perform_companion_pair_verify(&mut client2).await;

    // Send _sessionStart on connection 2
    let session_start_2 = Value::dict(vec![
        Value::kv("_i", Value::str("_sessionStart")),
        Value::kv("_x", Value::Int(2)),
        Value::kv("_c", Value::dict(vec![Value::kv("_sid", Value::Int(22222))])),
    ]);
    send_encrypted_companion_opack(&mut client2, &mut cipher2, &session_start_2).await;
    let resp2 = recv_encrypted_companion_opack(&mut client2, &mut cipher2).await;
    let sid2 = resp2
        .get("_c")
        .and_then(|c| c.get("_sid"))
        .and_then(Value::as_int)
        .expect("sid2 in response");

    // Session ID continuity: when connecting from same peer, stable session ID is preserved
    assert_eq!(sid1, sid2, "Session ID must be preserved across reconnects");

    // 4. Send a control command from client 2 (_hidC select button release)
    let select_cmd = Value::dict(vec![
        Value::kv("_i", Value::str("_hidC")),
        Value::kv("_x", Value::Int(3)),
        Value::kv(
            "_c",
            Value::dict(vec![
                Value::kv("_hidC", Value::Int(6)), // 6 = select
                Value::kv("_hBtS", Value::Int(2)), // 2 = release
            ]),
        ),
    ]);
    send_encrypted_companion_opack(&mut client2, &mut cipher2, &select_cmd).await;
    let cmd_resp = recv_encrypted_companion_opack(&mut client2, &mut cipher2).await;
    assert_eq!(
        cmd_resp.get("_rT").and_then(Value::as_int),
        Some(0),
        "Command must succeed with result 0"
    );

    // Allow event loop to process delegate call
    tokio::time::sleep(Duration::from_millis(50)).await;
    let captured = buttons.lock().unwrap().clone();
    assert!(
        captured.contains(&"select".to_string()),
        "Delegate must have received 'select' button event from reconnected client"
    );

    drop(client2);
    server.stop().await;
}

// ---------------------------------------------------------------------------
// TEST 2: MRP Handshake Isolation under Media Flooding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mrp_handshake_isolated_from_media_flood() {
    let delegate = Arc::new(TestDelegate::default());
    let mut config = AtvConfig::default();
    config.name = "Test ATV MRP Flood".to_string();
    config.ui_port = None;
    config.ip = Some(Ipv4Addr::LOCALHOST);

    let server = AtvServer::start(config, delegate).await.unwrap();
    let mrp_port = server.mrp_port();
    let media_hub = server.media_hub();

    // Spawn aggressive background media flooding task
    let hub_flood = media_hub.clone();
    let flood_stop = Arc::new(AtomicUsize::new(0));
    let flood_stop_clone = flood_stop.clone();
    let flood_task = tokio::spawn(async move {
        let artwork_dummy = vec![0xFF; 2048]; // 2KB artwork flood
        let mut count = 0;
        while flood_stop_clone.load(Ordering::Relaxed) == 0 {
            count += 1;
            hub_flood
                .update(MediaInfo {
                    title: format!("Flood Track {count}"),
                    artist: "Flood Artist".to_string(),
                    album: "Flood Album".to_string(),
                    app_name: "FloodApp".to_string(),
                    app_bundle_id: "com.apple.flood".to_string(),
                    duration: 300.0,
                    elapsed: count as f64,
                    rate: 1.0,
                    is_playing: true,
                    artwork: Some(artwork_dummy.clone()),
                    timestamp: 1000.0,
                })
                .await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    // Client connects to MRP
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, mrp_port))
        .await
        .unwrap();

    let client_secret = StaticSecret::random_from_rng(rand::thread_rng());
    let client_pub = X25519PublicKey::from(&client_secret);

    // M1: Client sends CryptoPairingMessage (seq=1)
    let m1_tlv = tlv::write(&[
        (tags::SEQ_NO, &[0x01]),
        (tags::PUBLIC_KEY, client_pub.as_bytes()),
    ]);
    let m1_payload = encode_crypto_pairing(&m1_tlv, 0);
    let m1_msg = encode_protocol_message(
        TYPE_CRYPTO_PAIRING_MESSAGE,
        "mrp-pv-1",
        0,
        EXT_CRYPTO_PAIRING,
        &m1_payload,
    );
    send_mrp_msg(&mut stream, &m1_msg, None).await;

    // Receive M2: Must be TYPE_CRYPTO_PAIRING_MESSAGE (not corrupted by SET_STATE / SET_ARTWORK)
    let m2 = recv_mrp_msg(&mut stream, None).await;
    assert_eq!(
        m2.msg_type, TYPE_CRYPTO_PAIRING_MESSAGE,
        "During Pair-Verify, media updates must NOT leak into the unencrypted handshake"
    );
    let m2_pd = decode_crypto_pairing(&m2.inner_data).expect("crypto pairing in M2");
    let m2_tlv = tlv::read(&m2_pd);
    let server_pub_bytes = m2_tlv.get(&tags::PUBLIC_KEY).unwrap();
    let server_pub =
        X25519PublicKey::from(<[u8; 32]>::try_from(server_pub_bytes.as_slice()).unwrap());
    let shared = client_secret.diffie_hellman(&server_pub);

    // M3: Client sends seq=3
    let m3_tlv = tlv::write(&[(tags::SEQ_NO, &[0x03])]);
    let m3_payload = encode_crypto_pairing(&m3_tlv, 0);
    let m3_msg = encode_protocol_message(
        TYPE_CRYPTO_PAIRING_MESSAGE,
        "mrp-pv-2",
        0,
        EXT_CRYPTO_PAIRING,
        &m3_payload,
    );
    send_mrp_msg(&mut stream, &m3_msg, None).await;

    // Receive M4: Must be TYPE_CRYPTO_PAIRING_MESSAGE
    let m4 = recv_mrp_msg(&mut stream, None).await;
    assert_eq!(
        m4.msg_type, TYPE_CRYPTO_PAIRING_MESSAGE,
        "M4 response must be CryptoPairingMessage"
    );

    // Handshake complete! Now initialize encryption on client
    let write_key = hkdf_expand("", "ClientEncrypt-main", shared.as_bytes());
    let read_key = hkdf_expand("", "ServerEncrypt-main", shared.as_bytes());
    let mut client_cipher = CompanionCipher::new(&write_key, &read_key, 12).unwrap();

    // Now media updates and connection state messages arrive encrypted without nonce errors
    let next_msg = recv_mrp_msg(&mut stream, Some(&mut client_cipher)).await;
    assert!(
        next_msg.msg_type == TYPE_SET_CONNECTION_STATE_MESSAGE
            || next_msg.msg_type == TYPE_SET_STATE_MESSAGE,
        "Encrypted state messages must decode cleanly without AEAD verification failure"
    );

    // Stop flood and cleanup
    flood_stop.store(1, Ordering::Relaxed);
    let _ = flood_task.await;
    drop(stream);
    server.stop().await;
}

// ---------------------------------------------------------------------------
// TEST 3: Handshake Timeout on Stalled Connection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_handshake_timeout_drops_stalled_connection() {
    let delegate = Arc::new(TestDelegate::default());
    let mut config = AtvConfig::default();
    config.name = "Test ATV Timeout".to_string();
    config.ui_port = None;
    config.ip = Some(Ipv4Addr::LOCALHOST);

    let server = AtvServer::start(config, delegate).await.unwrap();
    let companion_port = server.companion_port();

    // Connect raw TCP socket and send NOTHING
    let start = Instant::now();
    let mut stalled_socket = TcpStream::connect((Ipv4Addr::LOCALHOST, companion_port))
        .await
        .unwrap();

    // Read should wait until timeout expires and then return EOF (0 bytes)
    let mut buf = [0u8; 10];
    let n = stalled_socket.read(&mut buf).await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(
        n, 0,
        "Stalled connection must receive EOF when server closes socket after handshake timeout"
    );
    assert!(
        elapsed >= Duration::from_millis(7000),
        "Handshake timeout must be at least 7-8s to accommodate client setup (elapsed: {elapsed:?})"
    );
    assert!(
        elapsed <= Duration::from_millis(11000),
        "Handshake timeout must terminate within reasonable bounds (elapsed: {elapsed:?})"
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// TEST 4: Command Processing Responsiveness Under Artwork Load
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_command_processing_responsiveness() {
    let delegate = Arc::new(TestDelegate::default());
    let media_commands = delegate.media_commands.clone();

    let mut config = AtvConfig::default();
    config.name = "Test ATV Command Resp".to_string();
    config.ui_port = None;
    config.ip = Some(Ipv4Addr::LOCALHOST);

    let server = AtvServer::start(config, delegate).await.unwrap();
    let mrp_port = server.mrp_port();
    let media_hub = server.media_hub();

    // Populate media hub with heavy artwork payload (128 KB)
    let large_artwork = vec![0xAB; 128 * 1024];
    media_hub
        .update(MediaInfo {
            title: "Responsive Track".to_string(),
            artist: "Responsive Artist".to_string(),
            album: "Responsive Album".to_string(),
            app_name: "Music".to_string(),
            app_bundle_id: "com.apple.Music".to_string(),
            duration: 240.0,
            elapsed: 30.0,
            rate: 1.0,
            is_playing: true,
            artwork: Some(large_artwork),
            timestamp: 2000.0,
        })
        .await;

    // Connect client and perform MRP handshake
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, mrp_port))
        .await
        .unwrap();

    let client_secret = StaticSecret::random_from_rng(rand::thread_rng());
    let client_pub = X25519PublicKey::from(&client_secret);

    // M1
    let m1_tlv = tlv::write(&[
        (tags::SEQ_NO, &[0x01]),
        (tags::PUBLIC_KEY, client_pub.as_bytes()),
    ]);
    let m1_payload = encode_crypto_pairing(&m1_tlv, 0);
    let m1_msg = encode_protocol_message(
        TYPE_CRYPTO_PAIRING_MESSAGE,
        "cmd-pv-1",
        0,
        EXT_CRYPTO_PAIRING,
        &m1_payload,
    );
    send_mrp_msg(&mut stream, &m1_msg, None).await;

    // M2
    let m2 = recv_mrp_msg(&mut stream, None).await;
    let m2_pd = decode_crypto_pairing(&m2.inner_data).unwrap();
    let m2_tlv = tlv::read(&m2_pd);
    let server_pub_bytes = m2_tlv.get(&tags::PUBLIC_KEY).unwrap();
    let server_pub =
        X25519PublicKey::from(<[u8; 32]>::try_from(server_pub_bytes.as_slice()).unwrap());
    let shared = client_secret.diffie_hellman(&server_pub);

    // M3
    let m3_tlv = tlv::write(&[(tags::SEQ_NO, &[0x03])]);
    let m3_payload = encode_crypto_pairing(&m3_tlv, 0);
    let m3_msg = encode_protocol_message(
        TYPE_CRYPTO_PAIRING_MESSAGE,
        "cmd-pv-2",
        0,
        EXT_CRYPTO_PAIRING,
        &m3_payload,
    );
    send_mrp_msg(&mut stream, &m3_msg, None).await;

    // M4
    let _ = recv_mrp_msg(&mut stream, None).await;

    let write_key = hkdf_expand("", "ClientEncrypt-main", shared.as_bytes());
    let read_key = hkdf_expand("", "ServerEncrypt-main", shared.as_bytes());
    let mut client_cipher = CompanionCipher::new(&write_key, &read_key, 12).unwrap();

    // Drain initial state updates (connection state + initial state)
    let _ = recv_mrp_msg(&mut stream, Some(&mut client_cipher)).await;

    // Send Play command and measure response time
    let cmd_payload = encode_send_command(CMD_PLAY, 0);
    let cmd_id = "test-play-cmd";
    let cmd_msg = encode_protocol_message(
        TYPE_SEND_COMMAND_MESSAGE,
        cmd_id,
        0,
        EXT_SEND_COMMAND,
        &cmd_payload,
    );

    let start_time = Instant::now();
    send_mrp_msg(&mut stream, &cmd_msg, Some(&mut client_cipher)).await;

    // Read result
    let mut got_command_result = false;
    // We allow reading up to 5 messages in case state messages were queued
    for _ in 0..5 {
        let msg = recv_mrp_msg(&mut stream, Some(&mut client_cipher)).await;
        if msg.msg_type == TYPE_SEND_COMMAND_RESULT_MESSAGE {
            assert_eq!(msg.identifier, cmd_id);
            got_command_result = true;
            break;
        }
    }
    let response_duration = start_time.elapsed();

    assert!(
        got_command_result,
        "Server must send TYPE_SEND_COMMAND_RESULT_MESSAGE in response to command"
    );
    assert!(
        response_duration < Duration::from_millis(150),
        "Command result must arrive rapidly (<150ms), not delayed by artwork writes (took {response_duration:?})"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    let cmds = media_commands.lock().unwrap().clone();
    assert!(
        cmds.iter().any(|(name, _)| name == "play"),
        "Delegate must receive media command 'play'"
    );

    drop(stream);
    server.stop().await;
}
