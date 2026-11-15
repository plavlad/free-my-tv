//! MRP (MediaRemote Protocol) client session handler.

use std::sync::Arc;
use ed25519_dalek::{Signer, SigningKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{watch, Mutex};
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::crypto::{hkdf_expand, CompanionCipher};
use crate::delegate::AtvDelegate;
use crate::error::Result;
use crate::identity::DeviceIdentity;
use crate::media::MediaHub;
use crate::mrp::protobuf::*;
use crate::tlv::{self, tags};



struct MrpWriter {
    writer: OwnedWriteHalf,
    cipher: Option<CompanionCipher>,
}

impl MrpWriter {
    fn new(writer: OwnedWriteHalf) -> Self {
        Self {
            writer,
            cipher: None,
        }
    }

    async fn write_varint(&mut self, mut val: usize) -> std::io::Result<()> {
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
        self.writer.write_all(&buf[..pos]).await
    }

    async fn send_message(&mut self, payload: &[u8]) -> Result<()> {
        let data = if let Some(cipher) = &mut self.cipher {
            cipher.encrypt(payload, None, None)?
        } else {
            payload.to_vec()
        };

        self.write_varint(data.len()).await?;
        self.writer.write_all(&data).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

pub async fn handle_mrp_client(
    stream: TcpStream,
    identity: DeviceIdentity,
    delegate: Arc<dyn AtvDelegate>,
    media_hub: Arc<MediaHub>,
    mut shutdown: watch::Receiver<bool>,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    info!("MRP client connected: {peer}");

    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(MrpWriter::new(writer)));

    let mut media_rx = media_hub.subscribe();

    // Ephemeral Curve25519 key pair for pair-verify
    let verify_private = StaticSecret::random_from_rng(rand::thread_rng());
    let verify_pub = X25519PublicKey::from(&verify_private);
    let signing_key = SigningKey::from_bytes(&identity.private_key);
    let mut shared_secret_bytes: Option<[u8; 32]> = None;

    // Background task: stream media updates to this client
    let writer_for_updates = writer.clone();
    let display_name = identity.name.clone();
    let mut update_shutdown = shutdown.clone();
    let (client_cancel_tx, mut client_cancel_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                res = media_rx.recv() => {
                    match res {
                        Ok(media) => {
                            let mut w = writer_for_updates.lock().await;
                            // Do not stream media updates until Pair-Verify is complete and encryption is enabled
                            if w.cipher.is_none() {
                                continue;
                            }
                            let state_payload = encode_set_state(&media, &display_name);
                            let msg = encode_protocol_message(
                                TYPE_SET_STATE_MESSAGE,
                                "",
                                0,
                                EXT_SET_STATE,
                                &state_payload,
                            );
                            if w.send_message(&msg).await.is_err() {
                                break;
                            }

                            if let Some(art) = &media.artwork {
                                let art_payload = encode_set_artwork(art);
                                let art_msg = encode_protocol_message(
                                    TYPE_SET_ARTWORK_MESSAGE,
                                    "",
                                    0,
                                    EXT_SET_ARTWORK,
                                    &art_payload,
                                );
                                if w.send_message(&art_msg).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = update_shutdown.changed() => break,
                _ = client_cancel_rx.changed() => break,
            }
        }
    });

    let mut decrypt_cipher: Option<CompanionCipher> = None;

    loop {
        let size = tokio::select! {
            res = read_varint_from_reader(&mut reader) => match res {
                Some(s) if s > 0 && s < 10_000_000 => s,
                _ => break,
            },
            _ = shutdown.changed() => break,
        };

        let mut body = vec![0u8; size];
        if reader.read_exact(&mut body).await.is_err() {
            break;
        }

        if let Some(ref mut cipher) = decrypt_cipher {
            match cipher.decrypt(&body, None, None) {
                Ok(decrypted) => body = decrypted,
                Err(e) => {
                    warn!("MRP decrypt error from {peer}: {e}");
                    break;
                }
            }
        }

        let Some(msg) = decode_protocol_message(&body) else {
            debug!("MRP unknown packet from {peer} size={size}");
            continue;
        };

        debug!("MRP << type={} id={}", msg.msg_type, msg.identifier);

        match msg.msg_type {
            TYPE_DEVICE_INFO_MESSAGE => {
                info!("MRP << DeviceInfoMessage from {peer}");
                let dev_info = encode_device_info(
                    &identity.name,
                    &identity.server_identifier,
                    "Apple TV",
                    "22K160",
                );
                let resp = encode_protocol_message(
                    TYPE_DEVICE_INFO_MESSAGE,
                    &msg.identifier,
                    0,
                    EXT_DEVICE_INFO,
                    &dev_info,
                );
                let mut w = writer.lock().await;
                if let Err(e) = w.send_message(&resp).await {
                    warn!("MRP send DeviceInfo error: {e}");
                    break;
                }

                // Send Connected state
                let conn_state = encode_set_connection_state(2);
                let conn_msg = encode_protocol_message(
                    TYPE_SET_CONNECTION_STATE_MESSAGE,
                    "",
                    0,
                    EXT_SET_CONNECTION_STATE,
                    &conn_state,
                );
                let _ = w.send_message(&conn_msg).await;

                // Send active media state immediately
                let media = media_hub.get().await.unwrap_or_default();
                let state_payload = encode_set_state(&media, &identity.name);
                let state_msg = encode_protocol_message(
                    TYPE_SET_STATE_MESSAGE,
                    "",
                    0,
                    EXT_SET_STATE,
                    &state_payload,
                );
                let _ = w.send_message(&state_msg).await;

                if let Some(art) = &media.artwork {
                    let art_payload = encode_set_artwork(art);
                    let art_msg = encode_protocol_message(
                        TYPE_SET_ARTWORK_MESSAGE,
                        "",
                        0,
                        EXT_SET_ARTWORK,
                        &art_payload,
                    );
                    let _ = w.send_message(&art_msg).await;
                }
            }

            TYPE_CRYPTO_PAIRING_MESSAGE => {
                if let Some(pd_bytes) = decode_crypto_pairing(msg.inner_data) {
                    let pairing_tlv = tlv::read(&pd_bytes);
                    let seq = pairing_tlv
                        .get(&tags::SEQ_NO)
                        .map(|v| {
                            v.iter()
                                .enumerate()
                                .fold(0u64, |acc, (i, b)| acc | ((*b as u64) << (8 * i)))
                        })
                        .unwrap_or(0);

                    debug!("MRP << CryptoPairing seq={seq}");

                    if seq == 1 {
                        // Pair-Verify M1
                        if let Some(client_pub_bytes) = pairing_tlv.get(&tags::PUBLIC_KEY) {
                            if let Ok(client_pub_arr) = <[u8; 32]>::try_from(client_pub_bytes.as_slice()) {
                                let client_pub = X25519PublicKey::from(client_pub_arr);
                                let shared = verify_private.diffie_hellman(&client_pub);
                                shared_secret_bytes = Some(*shared.as_bytes());

                                let session_key = hkdf_expand(
                                    "Pair-Verify-Encrypt-Salt",
                                    "Pair-Verify-Encrypt-Info",
                                    shared.as_bytes(),
                                );

                                let server_pub = verify_pub.to_bytes();
                                let unique_id = identity.server_identifier.as_bytes();

                                let mut info_to_sign = Vec::with_capacity(32 + unique_id.len() + 32);
                                info_to_sign.extend_from_slice(&server_pub);
                                info_to_sign.extend_from_slice(unique_id);
                                info_to_sign.extend_from_slice(client_pub_bytes);
                                let signature = signing_key.sign(&info_to_sign);

                                if let Ok(mut chacha) = CompanionCipher::new(&session_key, &session_key, 8) {
                                    let inner_tlv = tlv::write(&[
                                        (tags::IDENTIFIER, unique_id),
                                        (tags::SIGNATURE, &signature.to_bytes()),
                                    ]);
                                    if let Ok(encrypted) = chacha.encrypt(&inner_tlv, None, Some(b"PV-Msg02")) {
                                        let out_tlv = tlv::write(&[
                                            (tags::SEQ_NO, &[0x02]),
                                            (tags::PUBLIC_KEY, &server_pub),
                                            (tags::ENCRYPTED_DATA, &encrypted),
                                        ]);
                                        let crypto_payload = encode_crypto_pairing(&out_tlv, 0);
                                        let resp = encode_protocol_message(
                                            TYPE_CRYPTO_PAIRING_MESSAGE,
                                            &msg.identifier,
                                            0,
                                            EXT_CRYPTO_PAIRING,
                                            &crypto_payload,
                                        );
                                        let mut w = writer.lock().await;
                                        let _ = w.send_message(&resp).await;
                                    }
                                }
                            }
                        }
                    } else if seq == 3 {
                        // Pair-Verify M3 -> M4
                        let out_tlv = tlv::write(&[(tags::SEQ_NO, &[0x04])]);
                        let crypto_payload = encode_crypto_pairing(&out_tlv, 0);
                        let resp = encode_protocol_message(
                            TYPE_CRYPTO_PAIRING_MESSAGE,
                            &msg.identifier,
                            0,
                            EXT_CRYPTO_PAIRING,
                            &crypto_payload,
                        );

                        let mut w = writer.lock().await;
                        let _ = w.send_message(&resp).await;

                        // Enable session encryption
                        if let Some(shared) = shared_secret_bytes {
                            let write_key = hkdf_expand("", "ServerEncrypt-main", &shared);
                            let read_key = hkdf_expand("", "ClientEncrypt-main", &shared);

                            if let (Ok(enc_cipher), Ok(dec_cipher)) = (
                                CompanionCipher::new(&write_key, &read_key, 12),
                                CompanionCipher::new(&write_key, &read_key, 12),
                            ) {
                                w.cipher = Some(enc_cipher);
                                decrypt_cipher = Some(dec_cipher);
                                info!("MRP encryption successfully enabled for {peer}");
                            }
                        }

                        // Send Connected state and current playback state
                        let conn_state = encode_set_connection_state(2);
                        let conn_msg = encode_protocol_message(
                            TYPE_SET_CONNECTION_STATE_MESSAGE,
                            "",
                            0,
                            EXT_SET_CONNECTION_STATE,
                            &conn_state,
                        );
                        let _ = w.send_message(&conn_msg).await;

                        let media = media_hub.get().await.unwrap_or_default();
                        let state_payload = encode_set_state(&media, &identity.name);
                        let state_msg = encode_protocol_message(
                            TYPE_SET_STATE_MESSAGE,
                            "",
                            0,
                            EXT_SET_STATE,
                            &state_payload,
                        );
                        let _ = w.send_message(&state_msg).await;
                    }
                }
            }

            TYPE_GET_STATE_MESSAGE | TYPE_CLIENT_UPDATES_CONFIG_MESSAGE => {
                let media = media_hub.get().await.unwrap_or_default();
                let state_payload = encode_set_state(&media, &identity.name);
                let state_msg = encode_protocol_message(
                    TYPE_SET_STATE_MESSAGE,
                    &msg.identifier,
                    0,
                    EXT_SET_STATE,
                    &state_payload,
                );
                let mut w = writer.lock().await;
                let _ = w.send_message(&state_msg).await;
            }

            TYPE_SEND_COMMAND_MESSAGE => {
                if let Some((cmd, param)) = decode_send_command(msg.inner_data) {
                    let cmd_name = match cmd {
                        CMD_PLAY => "play",
                        CMD_PAUSE => "pause",
                        CMD_TOGGLE_PLAY_PAUSE => "play_pause",
                        CMD_STOP => "stop",
                        CMD_NEXT_TRACK => "next",
                        CMD_PREVIOUS_TRACK => "prev",
                        CMD_SEEK => "seek",
                        other => {
                            debug!("unknown MRP command {other}");
                            "unknown"
                        }
                    };

                    info!("MRP << Command: {cmd_name} (param={param})");

                    let res_payload = encode_send_command_result(0); // 0 = Success
                    let resp = encode_protocol_message(
                        TYPE_SEND_COMMAND_RESULT_MESSAGE,
                        &msg.identifier,
                        0,
                        EXT_SEND_COMMAND_RESULT,
                        &res_payload,
                    );
                    {
                        let mut w = writer.lock().await;
                        let _ = w.send_message(&resp).await;
                    }

                    delegate.on_media_command(cmd_name, param);
                }
            }

            _ => {
                debug!("MRP unhandled message type={}", msg.msg_type);
            }
        }
    }

    let _ = client_cancel_tx.send(true);
    info!("MRP client disconnected: {peer}");
}

async fn read_varint_from_reader(reader: &mut OwnedReadHalf) -> Option<usize> {
    let mut shift = 0u32;
    let mut result: usize = 0;
    loop {
        let mut byte = [0u8; 1];
        if reader.read_exact(&mut byte).await.is_err() {
            return None;
        }
        result |= ((byte[0] & 0x7F) as usize) << shift;
        if byte[0] & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}
