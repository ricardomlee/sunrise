use std::{
    fs,
    net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use aes::Aes128;
use aes_gcm::{
    Aes128Gcm, AesGcm, Nonce,
    aead::{Aead, KeyInit, Payload, consts::U16, generic_array::GenericArray},
};
use anyhow::{Context, Result, anyhow};
use hex::FromHex;
use rusty_enet::{Event, Host, HostSettings};
use tokio::{sync::broadcast, task::JoinHandle, time::interval};
use tracing::{debug, info, trace, warn};

const CONTROL_CHANNELS: usize = 2;
const CONTROL_PEERS: usize = 8;
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ENCRYPTED_PACKET_TYPE: u16 = 0x0001;
const GCM_TAG_LEN: usize = 16;
const CONTROL_DUMP_DIR_ENV: &str = "SUNRISE_CONTROL_DUMP_DIR";
const CONTROL_EVENT_CAPACITY: usize = 64;

type LegacyAes128Gcm = AesGcm<Aes128, U16>;

type ControlHost = Host<StdUdpSocket>;

#[derive(Clone)]
pub(crate) struct ControlCryptoState {
    current_key: Arc<RwLock<Option<ControlSessionKey>>>,
    dump: Option<ControlDump>,
    event_tx: broadcast::Sender<ControlEvent>,
}

impl Default for ControlCryptoState {
    fn default() -> Self {
        Self {
            current_key: Arc::new(RwLock::new(None)),
            dump: ControlDump::from_env(),
            event_tx: broadcast::channel(CONTROL_EVENT_CAPACITY).0,
        }
    }
}

impl ControlCryptoState {
    pub(crate) fn set_key(&self, key: ControlSessionKey) {
        let key_id = key.key_id;
        if let Some(dump) = &self.dump {
            if let Err(err) = dump.write_session(&key) {
                warn!(error = %err, "failed to write GameStream control dump session metadata");
            }
        }
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

    fn dump_packet(&self, channel_id: u8, data: &[u8]) {
        let Some(dump) = &self.dump else {
            return;
        };
        if let Err(err) = dump.write_packet(channel_id, data) {
            warn!(error = %err, "failed to write GameStream control packet dump");
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ControlEvent> {
        self.event_tx.subscribe()
    }

    fn emit_event(&self, event: ControlEvent) {
        if self.event_tx.send(event).is_err() {
            trace!("dropping GameStream control event because no receivers are active");
        }
    }
}

#[derive(Clone)]
struct ControlDump {
    dir: PathBuf,
    next_index: Arc<std::sync::atomic::AtomicU32>,
}

impl ControlDump {
    fn from_env() -> Option<Self> {
        let value = std::env::var(CONTROL_DUMP_DIR_ENV).ok()?;
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let dir = PathBuf::from(value);
        if let Err(err) = fs::create_dir_all(&dir) {
            warn!(
                path = %dir.display(),
                error = %err,
                "failed to create GameStream control dump directory"
            );
            return None;
        }
        info!(path = %dir.display(), "GameStream control packet dumps enabled");
        Some(Self {
            dir,
            next_index: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        })
    }

    fn write_session(&self, key: &ControlSessionKey) -> Result<()> {
        let path = self.dir.join("session.txt");
        let body = format!(
            "rikeyid={}\nrikey={}\n",
            key.key_id,
            hex::encode_upper(key.key)
        );
        fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))
    }

    fn write_packet(&self, channel_id: u8, data: &[u8]) -> Result<()> {
        let index = self
            .next_index
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self
            .dir
            .join(format!("control-{index:06}-ch{channel_id}.bin"));
        fs::write(&path, data).with_context(|| format!("failed to write {}", path.display()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ControlSessionKey {
    pub(crate) key_id: i32,
    key: [u8; 16],
}

impl ControlSessionKey {
    pub(crate) fn from_launch_query(rikeyid: &str, rikey: &str) -> Result<Self> {
        let key_id = rikeyid
            .parse::<i32>()
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
            crypto.dump_packet(channel_id, packet.data());
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
    header: &'a [u8],
    encrypted_body: &'a [u8],
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
pub(crate) enum InputDataType {
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ControlEvent {
    IdrFrameRequested,
    StartA,
    StartB,
    PeriodicPing,
    Termination,
    InputData(InputDataType),
}

fn inspect_control_packet(data: &[u8], crypto: &ControlCryptoState) {
    if let Ok(packet) = parse_encrypted_control_packet(data) {
        let maybe_key = crypto.key();
        if let Some(key) = maybe_key.as_ref() {
            match decrypt_control_packet(key, &packet).and_then(|decrypted| {
                debug!(
                    key_id = key.key_id,
                    sequence = packet.sequence,
                    nonce = decrypted.nonce,
                    tag_layout = decrypted.tag_layout,
                    "decrypted GameStream control packet"
                );
                let payload = parse_control_payload(&decrypted.plaintext)?;
                publish_control_event(&payload, crypto);
                log_control_payload(&payload);
                Ok(())
            }) {
                Ok(()) => return,
                Err(err) => {
                    if should_log_control_packet(packet.sequence) {
                        debug!(
                            key_id = key.key_id,
                            sequence = packet.sequence,
                            error = %err,
                            "failed to decrypt GameStream control packet"
                        );
                    }
                }
            }
        }
        if should_log_control_packet(packet.sequence) {
            debug!(
                sequence = packet.sequence,
                payload_len = packet.payload.len(),
                tag_len = packet.tag.len(),
                has_key = maybe_key.is_some(),
                "received encrypted GameStream control packet"
            );
        }
        return;
    }

    if let Ok(payload) = parse_control_payload(data) {
        publish_control_event(&payload, crypto);
        log_control_payload(&payload);
        return;
    }

    trace!(
        len = data.len(),
        "received unrecognized GameStream control packet"
    );
}

fn publish_control_event(payload: &ControlPayload<'_>, crypto: &ControlCryptoState) {
    if let Some(event) = control_event_from_payload(payload) {
        crypto.emit_event(event);
    }
}

fn control_event_from_payload(payload: &ControlPayload<'_>) -> Option<ControlEvent> {
    match payload.message_type {
        ControlMessageType::IdrFrame => Some(ControlEvent::IdrFrameRequested),
        ControlMessageType::StartA => Some(ControlEvent::StartA),
        ControlMessageType::StartB => Some(ControlEvent::StartB),
        ControlMessageType::PeriodicPing => Some(ControlEvent::PeriodicPing),
        ControlMessageType::Termination => Some(ControlEvent::Termination),
        ControlMessageType::InputData => parse_input_data_header(payload.body)
            .ok()
            .map(|input| ControlEvent::InputData(input.input_type)),
        _ => None,
    }
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

struct DecryptedControlPacket {
    plaintext: Vec<u8>,
    nonce: &'static str,
    tag_layout: &'static str,
}

fn decrypt_control_packet(
    key: &ControlSessionKey,
    packet: &EncryptedControlPacket<'_>,
) -> Result<DecryptedControlPacket> {
    let cipher = Aes128Gcm::new_from_slice(&key.key).context("invalid AES-128 control key")?;
    let legacy_cipher =
        LegacyAes128Gcm::new_from_slice(&key.key).context("invalid AES-128 control key")?;
    let layouts = encrypted_control_layouts(packet);
    let nonces = control_nonce_candidates(packet.sequence);
    let legacy_nonces = legacy_control_nonce_candidates(packet.sequence);

    for layout in &layouts {
        for nonce in nonces {
            let payload = Payload {
                msg: layout.ciphertext_and_tag.as_slice(),
                aad: &[],
            };
            if let Ok(plaintext) = cipher.decrypt(Nonce::from_slice(&nonce.bytes), payload) {
                return Ok(DecryptedControlPacket {
                    plaintext,
                    nonce: nonce.name,
                    tag_layout: layout.name,
                });
            }

            let payload = Payload {
                msg: layout.ciphertext_and_tag.as_slice(),
                aad: layout.aad,
            };
            if let Ok(plaintext) = cipher.decrypt(Nonce::from_slice(&nonce.bytes), payload) {
                return Ok(DecryptedControlPacket {
                    plaintext,
                    nonce: nonce.name,
                    tag_layout: layout.name_with_aad,
                });
            }
        }
    }

    for layout in &layouts {
        for nonce in legacy_nonces {
            let payload = Payload {
                msg: layout.ciphertext_and_tag.as_slice(),
                aad: &[],
            };
            if let Ok(plaintext) =
                legacy_cipher.decrypt(GenericArray::from_slice(&nonce.bytes), payload)
            {
                return Ok(DecryptedControlPacket {
                    plaintext,
                    nonce: nonce.name,
                    tag_layout: layout.name,
                });
            }

            let payload = Payload {
                msg: layout.ciphertext_and_tag.as_slice(),
                aad: layout.aad,
            };
            if let Ok(plaintext) =
                legacy_cipher.decrypt(GenericArray::from_slice(&nonce.bytes), payload)
            {
                return Ok(DecryptedControlPacket {
                    plaintext,
                    nonce: nonce.name,
                    tag_layout: layout.name_with_aad,
                });
            }
        }
    }

    Err(anyhow!(
        "AES-GCM control decrypt failed for supported nonce/tag layouts"
    ))
}

struct EncryptedLayout<'a> {
    name: &'static str,
    name_with_aad: &'static str,
    ciphertext_and_tag: Vec<u8>,
    aad: &'a [u8],
}

fn encrypted_control_layouts<'a>(
    packet: &'a EncryptedControlPacket<'a>,
) -> Vec<EncryptedLayout<'a>> {
    let mut tag_first = Vec::with_capacity(packet.payload.len() + packet.tag.len());
    tag_first.extend_from_slice(packet.payload);
    tag_first.extend_from_slice(packet.tag);

    vec![
        EncryptedLayout {
            name: "tag-first",
            name_with_aad: "tag-first+header-aad",
            ciphertext_and_tag: tag_first,
            aad: packet.header,
        },
        EncryptedLayout {
            name: "tag-last",
            name_with_aad: "tag-last+header-aad",
            ciphertext_and_tag: packet.encrypted_body.to_vec(),
            aad: packet.header,
        },
    ]
}

#[derive(Copy, Clone)]
struct NonceCandidate {
    name: &'static str,
    bytes: [u8; 12],
}

fn control_nonce_candidates(sequence: u32) -> [NonceCandidate; 6] {
    let mut client_control = [0_u8; 12];
    client_control[..4].copy_from_slice(&sequence.to_le_bytes());
    client_control[10] = b'C';
    client_control[11] = b'C';

    let mut host_control = [0_u8; 12];
    host_control[..4].copy_from_slice(&sequence.to_le_bytes());
    host_control[10] = b'H';
    host_control[11] = b'C';

    let mut le_prefix = [0_u8; 12];
    le_prefix[..4].copy_from_slice(&sequence.to_le_bytes());

    let mut be_prefix = [0_u8; 12];
    be_prefix[..4].copy_from_slice(&sequence.to_be_bytes());

    let mut le_suffix = [0_u8; 12];
    le_suffix[8..].copy_from_slice(&sequence.to_le_bytes());

    let mut be_suffix = [0_u8; 12];
    be_suffix[8..].copy_from_slice(&sequence.to_be_bytes());

    [
        NonceCandidate {
            name: "client-control-v2",
            bytes: client_control,
        },
        NonceCandidate {
            name: "host-control-v2",
            bytes: host_control,
        },
        NonceCandidate {
            name: "seq-le-prefix",
            bytes: le_prefix,
        },
        NonceCandidate {
            name: "seq-be-prefix",
            bytes: be_prefix,
        },
        NonceCandidate {
            name: "seq-le-suffix",
            bytes: le_suffix,
        },
        NonceCandidate {
            name: "seq-be-suffix",
            bytes: be_suffix,
        },
    ]
}

#[derive(Copy, Clone)]
struct LegacyNonceCandidate {
    name: &'static str,
    bytes: [u8; 16],
}

fn legacy_control_nonce_candidates(sequence: u32) -> [LegacyNonceCandidate; 2] {
    // Moonlight's legacy control encryption uses a 16-byte AES-GCM IV with only the low
    // sequence byte populated. Newer control-v2 uses a 12-byte nonce with stream markers.
    let mut byte0 = [0_u8; 16];
    byte0[0] = sequence as u8;

    let mut le_prefix = [0_u8; 16];
    le_prefix[..4].copy_from_slice(&sequence.to_le_bytes());

    [
        LegacyNonceCandidate {
            name: "legacy-byte0-iv16",
            bytes: byte0,
        },
        LegacyNonceCandidate {
            name: "legacy-le-prefix-iv16",
            bytes: le_prefix,
        },
    ]
}

fn should_log_control_packet(sequence: u32) -> bool {
    sequence < 3 || sequence.is_multiple_of(300)
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
        header: &data[..tag_start],
        encrypted_body: &data[tag_start..],
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

    enum LegacyMode {
        ControlV2,
        Iv16,
    }

    fn encrypted_test_packet(
        key: &ControlSessionKey,
        sequence: u32,
        plaintext: &[u8],
        mode: LegacyMode,
    ) -> Vec<u8> {
        let encrypted = match mode {
            LegacyMode::ControlV2 => {
                let cipher = Aes128Gcm::new_from_slice(&key.key).unwrap();
                let nonce = control_nonce_candidates(sequence)[0].bytes;
                cipher
                    .encrypt(Nonce::from_slice(&nonce), plaintext)
                    .unwrap()
            }
            LegacyMode::Iv16 => {
                let cipher = LegacyAes128Gcm::new_from_slice(&key.key).unwrap();
                let nonce = legacy_control_nonce_candidates(sequence)[0].bytes;
                cipher
                    .encrypt(GenericArray::from_slice(&nonce), plaintext)
                    .unwrap()
            }
        };
        let tag_start = encrypted.len() - GCM_TAG_LEN;

        let mut packet = Vec::new();
        packet.extend_from_slice(&ENCRYPTED_PACKET_TYPE.to_le_bytes());
        let declared_len = u16::try_from(4 + GCM_TAG_LEN + tag_start).unwrap();
        packet.extend_from_slice(&declared_len.to_le_bytes());
        packet.extend_from_slice(&sequence.to_le_bytes());
        packet.extend_from_slice(&encrypted[tag_start..]);
        packet.extend_from_slice(&encrypted[..tag_start]);
        packet
    }

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
    fn parses_signed_control_session_key_id_from_launch_query() {
        let key =
            ControlSessionKey::from_launch_query("-1653680226", "00112233445566778899AABBCCDDEEFF")
                .unwrap();

        assert_eq!(key.key_id, -1653680226);
    }

    #[test]
    fn decrypts_encrypted_control_packet_with_stored_key() {
        let key =
            ControlSessionKey::from_launch_query("1", "00112233445566778899AABBCCDDEEFF").unwrap();
        let plaintext = [
            0x00, 0x02, // PERIODIC_PING
            0x00, 0x00, // empty body
        ];
        let packet = encrypted_test_packet(&key, 9, &plaintext, LegacyMode::ControlV2);

        let parsed = parse_encrypted_control_packet(&packet).unwrap();
        let decrypted = decrypt_control_packet(&key, &parsed).unwrap();
        let payload = parse_control_payload(&decrypted.plaintext).unwrap();

        assert_eq!(payload.message_type, ControlMessageType::PeriodicPing);
        assert!(payload.body.is_empty());
        assert_eq!(decrypted.nonce, "client-control-v2");
        assert_eq!(decrypted.tag_layout, "tag-first");
    }

    #[test]
    fn decrypts_legacy_iv16_control_packet_with_stored_key() {
        let key =
            ControlSessionKey::from_launch_query("1", "00112233445566778899AABBCCDDEEFF").unwrap();
        let plaintext = [
            0x00, 0x02, // PERIODIC_PING
            0x00, 0x00, // empty body
        ];
        let packet = encrypted_test_packet(&key, 9, &plaintext, LegacyMode::Iv16);

        let parsed = parse_encrypted_control_packet(&packet).unwrap();
        let decrypted = decrypt_control_packet(&key, &parsed).unwrap();
        let payload = parse_control_payload(&decrypted.plaintext).unwrap();

        assert_eq!(payload.message_type, ControlMessageType::PeriodicPing);
        assert!(payload.body.is_empty());
        assert_eq!(decrypted.nonce, "legacy-byte0-iv16");
        assert_eq!(decrypted.tag_layout, "tag-first");
    }

    #[test]
    fn emits_idr_event_from_decrypted_control_packet() {
        let crypto = ControlCryptoState::default();
        let mut events = crypto.subscribe();
        let key =
            ControlSessionKey::from_launch_query("1", "00112233445566778899AABBCCDDEEFF").unwrap();
        let packet = encrypted_test_packet(
            &key,
            0,
            &[0x02, 0x03, 0x02, 0x00, 0x00, 0x00],
            LegacyMode::Iv16,
        );
        crypto.set_key(key);

        inspect_control_packet(&packet, &crypto);

        assert_eq!(events.try_recv().unwrap(), ControlEvent::IdrFrameRequested);
    }

    #[test]
    #[ignore]
    fn decrypts_captured_control_dump() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|path| path.parent())
            .expect("crate should live below the workspace root")
            .to_path_buf();
        let dump_dir = workspace_root.join("target/control-dumps/latest");
        let session = std::fs::read_to_string(dump_dir.join("session.txt")).unwrap();
        let rikeyid = session
            .lines()
            .find_map(|line| line.strip_prefix("rikeyid="))
            .unwrap();
        let rikey = session
            .lines()
            .find_map(|line| line.strip_prefix("rikey="))
            .unwrap();
        let key = ControlSessionKey::from_launch_query(rikeyid, rikey).unwrap();
        let packet = std::fs::read(dump_dir.join("control-000000-ch0.bin")).unwrap();
        let parsed = parse_encrypted_control_packet(&packet).unwrap();
        let decrypted = decrypt_control_packet(&key, &parsed).unwrap();
        let payload = parse_control_payload(&decrypted.plaintext).unwrap();

        assert_eq!(decrypted.nonce, "legacy-byte0-iv16");
        assert_eq!(decrypted.tag_layout, "tag-first");
        assert!(matches!(payload.message_type, ControlMessageType::IdrFrame));
    }
}
