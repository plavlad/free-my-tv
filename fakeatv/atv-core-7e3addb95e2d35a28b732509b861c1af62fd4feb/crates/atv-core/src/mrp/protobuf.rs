//! Minimal zero-dependency Protobuf wire encoder and decoder for Apple MediaRemote (MRP).

use std::time::{SystemTime, UNIX_EPOCH};
use crate::media::MediaInfo;

pub const TYPE_SEND_COMMAND_MESSAGE: u32 = 1;
pub const TYPE_SEND_COMMAND_RESULT_MESSAGE: u32 = 2;
pub const TYPE_GET_STATE_MESSAGE: u32 = 3;
pub const TYPE_SET_STATE_MESSAGE: u32 = 4;
pub const TYPE_SET_ARTWORK_MESSAGE: u32 = 5;
pub const TYPE_DEVICE_INFO_MESSAGE: u32 = 15;
pub const TYPE_CLIENT_UPDATES_CONFIG_MESSAGE: u32 = 16;
pub const TYPE_CRYPTO_PAIRING_MESSAGE: u32 = 34;
pub const TYPE_SET_CONNECTION_STATE_MESSAGE: u32 = 38;

pub const EXT_SEND_COMMAND: u32 = 6;
pub const EXT_SEND_COMMAND_RESULT: u32 = 7;
pub const EXT_SET_STATE: u32 = 9;
pub const EXT_SET_ARTWORK: u32 = 10;
pub const EXT_DEVICE_INFO: u32 = 20;
pub const EXT_CRYPTO_PAIRING: u32 = 39;
pub const EXT_SET_CONNECTION_STATE: u32 = 42;

// Command enum
pub const CMD_PLAY: u32 = 1;
pub const CMD_PAUSE: u32 = 2;
pub const CMD_TOGGLE_PLAY_PAUSE: u32 = 3;
pub const CMD_STOP: u32 = 4;
pub const CMD_NEXT_TRACK: u32 = 5;
pub const CMD_PREVIOUS_TRACK: u32 = 6;
pub const CMD_SEEK: u32 = 45;

fn current_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Default, Clone)]
pub struct ProtobufWriter {
    buf: Vec<u8>,
}

impl ProtobufWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn write_tag(&mut self, field_num: u32, wire_type: u8) {
        let tag = ((field_num as u64) << 3) | (wire_type as u64 & 0x07);
        self.write_varint(tag);
    }

    pub fn write_varint(&mut self, mut val: u64) {
        loop {
            if val < 0x80 {
                self.buf.push(val as u8);
                break;
            } else {
                self.buf.push(((val & 0x7F) | 0x80) as u8);
                val >>= 7;
            }
        }
    }

    pub fn write_varint_field(&mut self, field_num: u32, val: u64) {
        self.write_tag(field_num, 0);
        self.write_varint(val);
    }

    pub fn write_bool_field(&mut self, field_num: u32, val: bool) {
        self.write_varint_field(field_num, if val { 1 } else { 0 });
    }

    pub fn write_fixed64_field(&mut self, field_num: u32, val: u64) {
        self.write_tag(field_num, 1);
        self.buf.extend_from_slice(&val.to_le_bytes());
    }

    pub fn write_double_field(&mut self, field_num: u32, val: f64) {
        self.write_fixed64_field(field_num, val.to_bits());
    }

    pub fn write_fixed32_field(&mut self, field_num: u32, val: u32) {
        self.write_tag(field_num, 5);
        self.buf.extend_from_slice(&val.to_le_bytes());
    }

    pub fn write_float_field(&mut self, field_num: u32, val: f32) {
        self.write_fixed32_field(field_num, val.to_bits());
    }

    pub fn write_bytes_field(&mut self, field_num: u32, bytes: &[u8]) {
        self.write_tag(field_num, 2);
        self.write_varint(bytes.len() as u64);
        self.buf.extend_from_slice(bytes);
    }

    pub fn write_string_field(&mut self, field_num: u32, s: &str) {
        self.write_bytes_field(field_num, s.as_bytes());
    }

    pub fn write_message_field(&mut self, field_num: u32, msg: &[u8]) {
        self.write_bytes_field(field_num, msg);
    }
}

pub fn read_varint_slice(slice: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in slice.iter().enumerate() {
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

pub struct ProtobufField<'a> {
    pub tag: u32,
    pub wire_type: u8,
    pub data: &'a [u8],
    pub varint: u64,
}

pub struct ProtobufReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ProtobufReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn next_field(&mut self) -> Option<ProtobufField<'a>> {
        if self.pos >= self.data.len() {
            return None;
        }
        let (raw_tag, tag_len) = read_varint_slice(&self.data[self.pos..])?;
        self.pos += tag_len;
        let field_num = (raw_tag >> 3) as u32;
        let wire_type = (raw_tag & 0x07) as u8;

        match wire_type {
            0 => {
                let (val, len) = read_varint_slice(&self.data[self.pos..])?;
                self.pos += len;
                Some(ProtobufField {
                    tag: field_num,
                    wire_type,
                    data: &[],
                    varint: val,
                })
            }
            1 => {
                if self.pos + 8 > self.data.len() {
                    return None;
                }
                let slice = &self.data[self.pos..self.pos + 8];
                self.pos += 8;
                Some(ProtobufField {
                    tag: field_num,
                    wire_type,
                    data: slice,
                    varint: 0,
                })
            }
            2 => {
                let (len, len_bytes) = read_varint_slice(&self.data[self.pos..])?;
                self.pos += len_bytes;
                let usize_len = len as usize;
                if self.pos + usize_len > self.data.len() {
                    return None;
                }
                let slice = &self.data[self.pos..self.pos + usize_len];
                self.pos += usize_len;
                Some(ProtobufField {
                    tag: field_num,
                    wire_type,
                    data: slice,
                    varint: 0,
                })
            }
            5 => {
                if self.pos + 4 > self.data.len() {
                    return None;
                }
                let slice = &self.data[self.pos..self.pos + 4];
                self.pos += 4;
                Some(ProtobufField {
                    tag: field_num,
                    wire_type,
                    data: slice,
                    varint: 0,
                })
            }
            _ => None,
        }
    }
}

// -----------------------------------------------------------------------------
// Message Encoders
// -----------------------------------------------------------------------------

pub fn encode_protocol_message(
    msg_type: u32,
    identifier: &str,
    error_code: u32,
    inner_tag: u32,
    inner_payload: &[u8],
) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    w.write_varint_field(1, msg_type as u64);
    let id_str = if identifier.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        identifier.to_string()
    };
    w.write_string_field(2, &id_str);
    w.write_varint_field(4, error_code as u64);
    w.write_varint_field(5, current_timestamp_ms());
    if inner_tag > 0 && !inner_payload.is_empty() {
        w.write_message_field(inner_tag, inner_payload);
    }
    w.into_bytes()
}

pub fn encode_device_info(
    name: &str,
    uuid: &str,
    model: &str,
    os_build: &str,
) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    w.write_string_field(1, uuid); // uniqueIdentifier
    w.write_string_field(2, name); // name
    w.write_string_field(3, model); // localizedModelName
    w.write_string_field(4, os_build); // systemBuildVersion
    w.write_string_field(5, "com.apple.mediaremoted"); // applicationBundleIdentifier
    w.write_varint_field(7, 1); // protocolVersion
    w.write_varint_field(8, 120); // lastSupportedMessageType
    w.write_bool_field(9, true); // supportsSystemPairing
    w.write_bool_field(10, true); // allowsPairing
    w.write_bool_field(11, true); // connected
    w.write_bool_field(14, true); // supportsSharedQueue
    w.write_string_field(19, uuid); // deviceUID
    w.write_varint_field(21, 4); // deviceClass (AppleTV = 4)
    w.write_varint_field(22, 1); // logicalDeviceCount
    w.write_bool_field(24, false); // isProxyGroupPlayer
    w.into_bytes()
}

pub fn encode_crypto_pairing(pairing_data: &[u8], status: i32) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    w.write_bytes_field(1, pairing_data);
    w.write_varint_field(2, status as u64);
    w.write_bool_field(4, true); // isUsingSystemPairing
    w.write_varint_field(5, 2); // state
    w.into_bytes()
}

pub fn encode_set_connection_state(state: u32) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    w.write_varint_field(1, state as u64); // 2 = Connected
    w.into_bytes()
}

pub fn encode_command_info(command: u32, can_scrub: bool) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    w.write_varint_field(1, command as u64);
    w.write_bool_field(2, true); // enabled
    w.write_bool_field(3, true); // active
    if can_scrub {
        w.write_varint_field(16, 1);
    }
    w.into_bytes()
}

pub fn encode_supported_commands() -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    for &cmd in &[
        CMD_PLAY,
        CMD_PAUSE,
        CMD_TOGGLE_PLAY_PAUSE,
        CMD_STOP,
        CMD_NEXT_TRACK,
        CMD_PREVIOUS_TRACK,
    ] {
        w.write_message_field(1, &encode_command_info(cmd, false));
    }
    w.write_message_field(1, &encode_command_info(CMD_SEEK, true));
    w.into_bytes()
}

pub fn encode_now_playing_info(media: &MediaInfo) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    if !media.album.is_empty() {
        w.write_string_field(1, &media.album);
    }
    if !media.artist.is_empty() {
        w.write_string_field(2, &media.artist);
    }
    if media.duration > 0.0 {
        w.write_double_field(3, media.duration);
    }
    if media.elapsed >= 0.0 {
        w.write_double_field(4, media.elapsed);
    }
    w.write_float_field(5, if media.is_playing { media.rate.max(1.0) } else { 0.0 });
    w.write_double_field(8, if media.timestamp > 0.0 { media.timestamp } else { current_timestamp() });
    if !media.title.is_empty() {
        w.write_string_field(9, &media.title);
    }
    w.write_varint_field(10, 1001); // uniqueIdentifier
    w.into_bytes()
}

pub fn encode_set_state(media: &MediaInfo, display_name: &str) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    let npi = encode_now_playing_info(media);
    w.write_message_field(1, &npi);

    let sc = encode_supported_commands();
    w.write_message_field(2, &sc);

    let display_id = if !media.app_bundle_id.is_empty() {
        &media.app_bundle_id
    } else {
        "com.google.android.youtube.tv"
    };
    w.write_string_field(4, display_id);
    w.write_string_field(5, display_name);

    // playbackState: 1 = Playing, 2 = Paused, 3 = Stopped
    let state: u64 = if media.is_playing { 1 } else { 2 };
    w.write_varint_field(6, state);
    w.write_double_field(11, current_timestamp());
    w.into_bytes()
}

pub fn encode_set_artwork(jpeg_data: &[u8]) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    w.write_bytes_field(1, jpeg_data);
    w.into_bytes()
}

pub fn encode_send_command_result(status: u32) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    w.write_varint_field(1, 0); // sendError: NoError
    w.write_varint_field(2, status as u64); // handlerReturnStatus: 0 = Success
    w.into_bytes()
}

pub fn encode_send_command(cmd: u32, param: i64) -> Vec<u8> {
    let mut w = ProtobufWriter::new();
    w.write_varint_field(1, cmd as u64);
    if param > 0 {
        let mut opt = ProtobufWriter::new();
        opt.write_fixed64_field(1, (param as f64).to_bits());
        w.write_message_field(2, &opt.into_bytes());
    }
    w.into_bytes()
}

// -----------------------------------------------------------------------------
// Message Decoders
// -----------------------------------------------------------------------------

pub struct DecodedProtocolMessage<'a> {
    pub msg_type: u32,
    pub identifier: String,
    pub inner_tag: u32,
    pub inner_data: &'a [u8],
}

pub fn decode_protocol_message(data: &[u8]) -> Option<DecodedProtocolMessage<'_>> {
    let mut reader = ProtobufReader::new(data);
    let mut msg_type = 0u32;
    let mut identifier = String::new();
    let mut inner_tag = 0u32;
    let mut inner_data: &[u8] = &[];

    while let Some(field) = reader.next_field() {
        match field.tag {
            1 if field.wire_type == 0 => {
                msg_type = field.varint as u32;
            }
            2 if field.wire_type == 2 => {
                if let Ok(s) = std::str::from_utf8(field.data) {
                    identifier = s.to_string();
                }
            }
            tag if field.wire_type == 2 && tag >= 6 => {
                inner_tag = tag;
                inner_data = field.data;
            }
            _ => {}
        }
    }

    if msg_type > 0 {
        Some(DecodedProtocolMessage {
            msg_type,
            identifier,
            inner_tag,
            inner_data,
        })
    } else {
        None
    }
}

pub fn decode_crypto_pairing(data: &[u8]) -> Option<Vec<u8>> {
    let mut reader = ProtobufReader::new(data);
    while let Some(field) = reader.next_field() {
        if field.tag == 1 && field.wire_type == 2 {
            return Some(field.data.to_vec());
        }
    }
    None
}

pub fn decode_send_command(data: &[u8]) -> Option<(u32, i64)> {
    let mut reader = ProtobufReader::new(data);
    let mut cmd = 0u32;
    let mut param = 0i64;

    while let Some(field) = reader.next_field() {
        if field.tag == 1 && field.wire_type == 0 {
            cmd = field.varint as u32;
        } else if field.tag == 2 && field.wire_type == 2 {
            // CommandOptions message
            let mut opt_reader = ProtobufReader::new(field.data);
            while let Some(opt_field) = opt_reader.next_field() {
                // Seek position / playback position
                if opt_field.tag == 1 && opt_field.wire_type == 1 {
                    let pos_bits = u64::from_le_bytes(opt_field.data.try_into().unwrap_or([0; 8]));
                    param = f64::from_bits(pos_bits) as i64;
                }
            }
        }
    }

    if cmd > 0 {
        Some((cmd, param))
    } else {
        None
    }
}
