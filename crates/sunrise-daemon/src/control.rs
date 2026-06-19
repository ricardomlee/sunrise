use std::{
    net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket},
    sync::{Arc, RwLock},
    time::Duration,
};

use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result, anyhow};
use hex::FromHex;
use rusty_enet::{Event, Host, HostSettings};
use tokio::{task::JoinHandle, time::interval};
use tracing::{debug, info, warn};

const CONTROL_CHANNELS: usize = 2;
const CONTROL_PEERS: usize = 8;
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ENCRYPTED_PACKET_TYPE: u16 = 0x0001;
const GCM_TAG_LEN: usize = 16;

type ControlHost = Host<StdUdpSocket>;

#[derive(Clone, Default)]
pub(crate) struct ControlCryptoState {
    current_key: Arc<RwLock<Option<ControlSessionKey>>>,
}

impl ControlCryptoState {
    pub(crate) fn set_key(&self, key: ControlSessionKey) {
        let key_id = key.key_id;
        *self
            .current_key
            .write()
            .expect("control crypto lock poisoned") = Some(key);
        info!(
            key_id,
            "stored GameStream control RI key for launched session"
        );
    }

    pub(crate) fn clear_key(&self) {
        *self
            .current_key
            .write()
            .expect("control crypto lock poisoned") = None;
        info!("cleared GameStream control RI key");
    }

    fn key(&self) -> Option<ControlSessionKey> {
        self.current_key
            .read()
            .expect("control crypto lock poisoned")
            .clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ControlSessionKey {
    pub(crate) key_id: u32,
    key: [u8; 16],
}

impl ControlSessionKey {
    pub(crate) fn from_launch_query(rikeyid: &str, rikey: &str) -> Result<Self> {
        let key_id = rikeyid
            .parse::<u32>()
            .with_context(|| format!("invalid rikeyid {rikeyid:?}"))?;
        let key = <[u8; 16]>::from_hex(rikey).context("rikey must be a 16-byte hex AES-128 key")?;
        Ok(Self { key_id, key })
    }
}

pub(crate) fn spawn_control_server(
    bind_ip: IpAddr,
    port: u16,
    crypto: ControlCryptoState,
) -> Result<JoinHandle<()>> {
    let addr = SocketAddr::new(bind_ip, port);
    let host = control_host(addr)?;

    Ok(tokio::spawn(async move {
        if let Err(err) = run_control_server(host, crypto).await {
            warn!(%err, "ENet control server stopped with error");
        }
    }))
}

fn control_host(addr: SocketAddr) -> Result<ControlHost> {
    let socket = StdUdpSocket::bind(addr)
        .with_context(|| format!("failed to bind ENet control UDP socket on {addr}"))?;
    Host::new(
        socket,
        HostSettings {
            peer_limit: CONTROL_PEERS,
            channel_limit: CONTROL_CHANNELS,
            ..Default::default()
        },
    )
    .map_err(|err| anyhow!("failed to initialize ENet control host on {addr}: {err:?}"))
}

async fn run_control_server(mut host: ControlHost, crypto: ControlCryptoState) -> Result<()> {
    let addr = host
        .socket()
        .local_addr()
        .context("failed to read ENet control address")?;
    info!(%addr, "ENet control server listening");
    let mut ticker = interval(CONTROL_POLL_INTERVAL);

    loop {
        ticker.tick().await;
        while let Some(event) = host.service()? {
            handle_control_event(event, &crypto)?;
        }
        host.flush();
    }
}

fn handle_control_event(event: Event<StdUdpSocket>, crypto: &ControlCryptoState) -> Result<()> {
    match event {
        Event::Connect { peer, data } => {
            let peer_id = peer.id();
            info!(?peer_id, data, "ENet control client connected");
        }
        Event::Disconnect { peer, data } => {
            let peer_id = peer.id();
            info!(?peer_id, data, "ENet control client disconnected");
        }
        Event::Receive {
            peer,
            channel_id,
            packet,
        } => {
            let peer_id = peer.id();
            inspect_control_packet(packet.data(), crypto);
            debug!(
                ?peer_id,
                channel_id,
                len = packet.data().len(),
                "received ENet control packet"
            );
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct EncryptedControlPacket<'a> {
    sequence: u32,
    tag: &'a [u8],
    payload: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
struct ControlPayload<'a> {
    message_type: ControlMessageType,
    declared_len: usize,
    body: &'a [u8],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ControlMessageType {
    StartA,
    StartB,
    InvalidateRefFrames,
    LossStats,
    FrameStats,
    InputData,
    Termination,
    PeriodicPing,
    IdrFrame,
    HdrMode,
    RumbleData,
    RumbleTriggers,
    MotionEvent,
    RgbLed,
    Unknown(u16),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum InputDataType {
    MouseMoveRel,
    MouseMoveAbs,
    MouseButtonDown,
    MouseButtonUp,
    KeyDown,
    KeyUp,
    MouseScroll,
    MouseHScroll,
    Touch,
    Pen,
    ControllerMulti,
    ControllerArrival,
    ControllerTouch,
    ControllerMotion,
    ControllerBattery,
    Haptics,
    Utf8Text,
    Unknown(u32),
}

#[derive(Debug, PartialEq, Eq)]
struct InputDataHeader {
    message_size: usize,
    input_type: InputDataType,
}

fn inspect_control_packet(data: &[u8], crypto: &ControlCryptoState) {
    if let Ok(packet) = parse_encrypted_control_packet(data) {
        let maybe_key = crypto.key();
        if let Some(key) = maybe_key.as_ref() {
            match decrypt_control_packet(key, &packet).and_then(|plain| {
                let payload = parse_control_payload(&plain)?;
                log_control_payload(&payload);
                Ok(())
            }) {
                Ok(()) => {
                    debug!(
                        key_id = key.key_id,
                        sequence = packet.sequence,
                        "decrypted GameStream control packet"
                    );
                    return;
                }
                Err(err) => {
                    debug!(
                        key_id = key.key_id,
                        sequence = packet.sequence,
                        error = %err,
                        "failed to decrypt GameStream control packet"
                    );
                }
            }
        }
        debug!(
            sequence = packet.sequence,
            payload_len = packet.payload.len(),
            tag_len = packet.tag.len(),
            has_key = maybe_key.is_some(),
            "received encrypted GameStream control packet"
        );
        return;
    }

    if let Ok(payload) = parse_control_payload(data) {
        log_control_payload(&payload);
        return;
    }

    debug!(
        len = data.len(),
        "received unrecognized GameStream control packet"
    );
}

fn log_control_payload(payload: &ControlPayload<'_>) {
    if payload.message_type == ControlMessageType::InputData {
        match parse_input_data_header(payload.body) {
            Ok(input) => {
                debug!(
                    message_type = ?payload.message_type,
                    input_type = ?input.input_type,
                    message_size = input.message_size,
                    "received decrypted GameStream input payload"
                );
            }
            Err(err) => {
                debug!(
                    message_type = ?payload.message_type,
                    declared_len = payload.declared_len,
                    error = %err,
                    "received malformed GameStream input payload"
                );
            }
        }
    } else {
        debug!(
            message_type = ?payload.message_type,
            declared_len = payload.declared_len,
            body_len = payload.body.len(),
            "received decrypted GameStream control payload"
        );
    }
}

fn decrypt_control_packet(
    key: &ControlSessionKey,
    packet: &EncryptedControlPacket<'_>,
) -> Result<Vec<u8>> {
    let cipher = Aes128Gcm::new_from_slice(&key.key).context("invalid AES-128 control key")?;
    let nonce = control_nonce(packet.sequence);
    let mut ciphertext = Vec::with_capacity(packet.payload.len() + packet.tag.len());
    ciphertext.extend_from_slice(packet.payload);
    ciphertext.extend_from_slice(packet.tag);
    cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|err| anyhow!("AES-GCM control decrypt failed: {err}"))
}

fn control_nonce(sequence: u32) -> [u8; 12] {
    let mut nonce = [0_u8; 12];
    nonce[..4].copy_from_slice(&sequence.to_le_bytes());
    nonce
}

fn parse_encrypted_control_packet(data: &[u8]) -> Result<EncryptedControlPacket<'_>> {
    if data.len() < 4 + 4 + GCM_TAG_LEN {
        anyhow::bail!("encrypted control packet is too short");
    }

    let packet_type = u16::from_le_bytes([data[0], data[1]]);
    if packet_type != ENCRYPTED_PACKET_TYPE {
        anyhow::bail!("unsupported encrypted control packet type {packet_type:#06x}");
    }

    let declared_len = usize::from(u16::from_le_bytes([data[2], data[3]]));
    let actual_len = data.len() - 4;
    if declared_len != actual_len {
        anyhow::bail!(
            "encrypted control packet length mismatch: declared {declared_len}, got {actual_len}"
        );
    }

    let sequence = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let tag_start = 8;
    let payload_start = tag_start + GCM_TAG_LEN;
    Ok(EncryptedControlPacket {
        sequence,
        tag: &data[tag_start..payload_start],
        payload: &data[payload_start..],
    })
}

fn parse_control_payload(data: &[u8]) -> Result<ControlPayload<'_>> {
    if data.len() < 4 {
        anyhow::bail!("control payload is too short");
    }

    let message_type = ControlMessageType::from_wire(u16::from_le_bytes([data[0], data[1]]));
    let declared_len = usize::from(u16::from_le_bytes([data[2], data[3]]));
    let body = &data[4..];
    if declared_len != body.len() {
        anyhow::bail!(
            "control payload length mismatch: declared {declared_len}, got {}",
            body.len()
        );
    }

    Ok(ControlPayload {
        message_type,
        declared_len,
        body,
    })
}

fn parse_input_data_header(data: &[u8]) -> Result<InputDataHeader> {
    if data.len() < 8 {
        anyhow::bail!("input payload is too short");
    }

    let message_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if message_size > data.len() {
        anyhow::bail!(
            "input payload size exceeds packet body: declared {message_size}, got {}",
            data.len()
        );
    }

    let input_type =
        InputDataType::from_wire(u32::from_le_bytes([data[4], data[5], data[6], data[7]]));
    Ok(InputDataHeader {
        message_size,
        input_type,
    })
}

impl ControlMessageType {
    fn from_wire(value: u16) -> Self {
        match value {
            0x0305 => Self::StartA,
            0x0307 => Self::StartB,
            0x0301 => Self::InvalidateRefFrames,
            0x0201 => Self::LossStats,
            0x0204 => Self::FrameStats,
            0x0206 => Self::InputData,
            0x0100 => Self::Termination,
            0x0200 => Self::PeriodicPing,
            0x0302 => Self::IdrFrame,
            0x010e => Self::HdrMode,
            0x010b => Self::RumbleData,
            0x5500 => Self::RumbleTriggers,
            0x5501 => Self::MotionEvent,
            0x5502 => Self::RgbLed,
            value => Self::Unknown(value),
        }
    }
}

impl InputDataType {
    fn from_wire(value: u32) -> Self {
        match value {
            0x0000_0007 => Self::MouseMoveRel,
            0x0000_0005 => Self::MouseMoveAbs,
            0x0000_0008 => Self::MouseButtonDown,
            0x0000_0009 => Self::MouseButtonUp,
            0x0000_0003 => Self::KeyDown,
            0x0000_0004 => Self::KeyUp,
            0x0000_000a => Self::MouseScroll,
            0x5500_0001 => Self::MouseHScroll,
            0x5500_0002 => Self::Touch,
            0x5500_0003 => Self::Pen,
            0x0000_000c => Self::ControllerMulti,
            0x5500_0004 => Self::ControllerArrival,
            0x5500_0005 => Self::ControllerTouch,
            0x5500_0006 => Self::ControllerMotion,
            0x5500_0007 => Self::ControllerBattery,
            0x0000_000d => Self::Haptics,
            0x0000_0017 => Self::Utf8Text,
            value => Self::Unknown(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const TEST_DEADLINE: Duration = Duration::from_secs(3);

    #[tokio::test]
    async fn control_host_accepts_enet_connection() {
        let mut server = control_host("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server.socket().local_addr().unwrap();
        let client_socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client = Host::new(
            client_socket,
            HostSettings {
                peer_limit: 1,
                channel_limit: CONTROL_CHANNELS,
                ..Default::default()
            },
        )
        .unwrap();

        let client_peer = client
            .connect(server_addr, CONTROL_CHANNELS, 0)
            .unwrap()
            .id();
        client.flush();

        let mut client_connected = false;
        let mut server_connected = false;
        let deadline = Instant::now() + TEST_DEADLINE;

        while Instant::now() < deadline {
            while let Some(event) = server.service().unwrap() {
                handle_control_event(event, &ControlCryptoState::default()).unwrap();
                server_connected = true;
            }
            server.flush();

            while let Some(event) = client.service().unwrap() {
                match event {
                    Event::Connect { peer, .. } if peer.id() == client_peer => {
                        client_connected = true;
                    }
                    _ => {}
                }
            }
            client.flush();

            if client_connected && server_connected {
                return;
            }

            tokio::time::sleep(CONTROL_POLL_INTERVAL).await;
        }

        panic!("timed out waiting for ENet control connection");
    }

    #[test]
    fn parses_encrypted_control_packet_envelope() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&0x0001_u16.to_le_bytes());
        packet.extend_from_slice(&24_u16.to_le_bytes());
        packet.extend_from_slice(&42_u32.to_le_bytes());
        packet.extend_from_slice(&[0xab; GCM_TAG_LEN]);
        packet.extend_from_slice(&[0xcd; 4]);

        let parsed = parse_encrypted_control_packet(&packet).unwrap();

        assert_eq!(parsed.sequence, 42);
        assert_eq!(parsed.tag, &[0xab; GCM_TAG_LEN]);
        assert_eq!(parsed.payload, &[0xcd; 4]);
    }

    #[test]
    fn rejects_encrypted_control_packet_length_mismatch() {
        let packet = [
            0x01, 0x00, // type
            0x08, 0x00, // wrong len
            0x01, 0x00, 0x00, 0x00, // seq
            0x00, 0x00, 0x00, 0x00, // partial tag
        ];

        assert!(parse_encrypted_control_packet(&packet).is_err());
    }

    #[test]
    fn parses_decrypted_control_payload_header() {
        let payload = [
            0x06, 0x02, // INPUT_DATA
            0x08, 0x00, // body len
            0x00, 0x00, 0x00, 0x08, // input message size, big endian
            0x03, 0x00, 0x00, 0x00, // KEY_DOWN, little endian
        ];

        let parsed = parse_control_payload(&payload).unwrap();
        let input = parse_input_data_header(parsed.body).unwrap();

        assert_eq!(parsed.message_type, ControlMessageType::InputData);
        assert_eq!(parsed.declared_len, 8);
        assert_eq!(input.message_size, 8);
        assert_eq!(input.input_type, InputDataType::KeyDown);
    }

    #[test]
    fn maps_common_input_data_types() {
        assert_eq!(
            InputDataType::from_wire(0x0000_0007),
            InputDataType::MouseMoveRel
        );
        assert_eq!(InputDataType::from_wire(0x0000_0004), InputDataType::KeyUp);
        assert_eq!(
            InputDataType::from_wire(0x5500_0004),
            InputDataType::ControllerArrival
        );
        assert_eq!(
            InputDataType::from_wire(0xdead_beef),
            InputDataType::Unknown(0xdead_beef)
        );
    }

    #[test]
    fn parses_control_session_key_from_launch_query() {
        let key =
            ControlSessionKey::from_launch_query("7", "00112233445566778899AABBCCDDEEFF").unwrap();

        assert_eq!(key.key_id, 7);
        assert_eq!(
            key.key,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );
    }

    #[test]
    fn decrypts_encrypted_control_packet_with_stored_key() {
        let key =
            ControlSessionKey::from_launch_query("1", "00112233445566778899AABBCCDDEEFF").unwrap();
        let plaintext = [
            0x00, 0x02, // PERIODIC_PING
            0x00, 0x00, // empty body
        ];
        let cipher = Aes128Gcm::new_from_slice(&key.key).unwrap();
        let nonce = control_nonce(9);
        let encrypted = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
            .unwrap();
        let tag_start = encrypted.len() - GCM_TAG_LEN;

        let mut packet = Vec::new();
        packet.extend_from_slice(&ENCRYPTED_PACKET_TYPE.to_le_bytes());
        let declared_len = u16::try_from(4 + GCM_TAG_LEN + tag_start).unwrap();
        packet.extend_from_slice(&declared_len.to_le_bytes());
        packet.extend_from_slice(&9_u32.to_le_bytes());
        packet.extend_from_slice(&encrypted[tag_start..]);
        packet.extend_from_slice(&encrypted[..tag_start]);

        let parsed = parse_encrypted_control_packet(&packet).unwrap();
        let decrypted = decrypt_control_packet(&key, &parsed).unwrap();
        let payload = parse_control_payload(&decrypted).unwrap();

        assert_eq!(payload.message_type, ControlMessageType::PeriodicPing);
        assert!(payload.body.is_empty());
    }
}
