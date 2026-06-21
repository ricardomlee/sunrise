use std::{
    collections::HashMap,
    env,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{Mutex, broadcast};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinHandle,
    time::{interval, timeout},
};
use tracing::{debug, info, warn};

use crate::{
    AppState, control, encoder,
    media::{
        AnnexBVideoSource, AudioEncryptionKey, AudioPacketizer, OpusSilenceSource, VideoPacketizer,
        VideoSource,
    },
};

const SESSION_ID: &str = "DEADBEEFCAFE";
const MAX_RTSP_REQUEST_BYTES: usize = 1024 * 1024;
const VIDEO_PORT: u16 = 47998;
const AUDIO_PORT: u16 = 48000;
const CONTROL_PORT: u16 = 47999;

#[derive(Clone)]
pub struct RtspState {
    inner: Arc<Mutex<RtspSessionState>>,
    bind_ip: IpAddr,
    control_crypto: control::ControlCryptoState,
}

impl RtspState {
    pub fn new(bind_ip: IpAddr) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RtspSessionState::default())),
            bind_ip,
            control_crypto: control::ControlCryptoState::default(),
        }
    }

    pub(crate) fn set_control_key(&self, key: control::ControlSessionKey) {
        self.control_crypto.set_key(key);
    }

    pub(crate) fn clear_control_key(&self) {
        self.control_crypto.clear_key();
    }
}

impl Default for RtspState {
    fn default() -> Self {
        Self::new(IpAddr::from([0, 0, 0, 0]))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RtspRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    payload: String,
}

impl RtspRequest {
    fn cseq(&self) -> &str {
        self.headers.get("cseq").map(String::as_str).unwrap_or("1")
    }

    fn requested_keep_alive(&self) -> bool {
        self.headers
            .get("connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("keep-alive"))
    }
}

pub async fn serve_rtsp(addr: SocketAddr, state: AppState) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind RTSP listener on {addr}"))?;

    info!(%addr, "RTSP skeleton listening");
    info!("TODO: replace RTSP skeleton with negotiated audio/video/control stream setup");

    loop {
        let (stream, remote) = listener.accept().await.context("RTSP accept failed")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_rtsp_connection(stream, remote, state).await {
                debug!(%remote, error = %err, "RTSP connection ended");
            }
        });
    }
}

async fn handle_rtsp_connection(
    mut stream: TcpStream,
    remote: SocketAddr,
    state: AppState,
) -> Result<()> {
    let source_ip = if remote.ip().is_loopback() {
        remote.ip().to_string()
    } else {
        state.local_ip.to_string()
    };
    let mut buffer = Vec::new();

    while let Some(request) = read_rtsp_request(&mut stream, &mut buffer).await? {
        info!(
            %remote,
            method = %request.method,
            target = %request.target,
            cseq = %request.cseq(),
            payload_len = request.payload.len(),
            "incoming RTSP request"
        );

        let should_close = !request.requested_keep_alive() || request.method == "TEARDOWN";
        if request.method == "SETUP" {
            if let Some(media) = media_kind_for_target(&request.target) {
                state.rtsp.setup_udp(media).await?;
            }
        } else if request.method == "PLAY" {
            state.rtsp.start_streams().await;
        } else if request.method == "TEARDOWN" {
            state.rtsp.stop_streams().await;
        }

        let response = rtsp_response(&request, &source_ip, should_close);
        stream
            .write_all(response.as_bytes())
            .await
            .context("failed to write RTSP response")?;

        if should_close {
            break;
        }
    }

    stream.shutdown().await.ok();
    Ok(())
}

#[derive(Default)]
struct RtspSessionState {
    video_socket: Option<Arc<UdpSocket>>,
    audio_socket: Option<Arc<UdpSocket>>,
    control_task: Option<JoinHandle<()>>,
    stream_tasks: Vec<JoinHandle<()>>,
    streams_started: bool,
}

impl RtspState {
    async fn setup_udp(&self, media: MediaKind) -> Result<()> {
        let mut session = self.inner.lock().await;
        match media {
            MediaKind::Video if session.video_socket.is_none() => {
                session.video_socket =
                    Some(Arc::new(bind_udp_port(self.bind_ip, VIDEO_PORT).await?));
                info!(port = VIDEO_PORT, "video RTP UDP port ready");
            }
            MediaKind::Audio if session.audio_socket.is_none() => {
                session.audio_socket =
                    Some(Arc::new(bind_udp_port(self.bind_ip, AUDIO_PORT).await?));
                info!(port = AUDIO_PORT, "audio RTP UDP port ready");
            }
            MediaKind::Control if session.control_task.is_none() => {
                session.control_task = Some(control::spawn_control_server(
                    self.bind_ip,
                    CONTROL_PORT,
                    self.control_crypto.clone(),
                )?);
                info!(port = CONTROL_PORT, "ENet control UDP port ready");
            }
            _ => {}
        }
        Ok(())
    }

    async fn start_streams(&self) {
        let mut session = self.inner.lock().await;
        if session.streams_started {
            return;
        }
        session.streams_started = true;

        if let Some(socket) = session.video_socket.clone() {
            let control_events = self.control_crypto.subscribe();
            session.stream_tasks.push(tokio::spawn(async move {
                if let Err(err) = stream_video_rtp(socket, control_events).await {
                    warn!(error = %err, "video RTP sender stopped");
                }
            }));
        }

        if let Some(socket) = session.audio_socket.clone() {
            let audio_key = self
                .control_crypto
                .current_key()
                .map(|key| AudioEncryptionKey::new(key.key_id_u32(), key.key_bytes()));
            session.stream_tasks.push(tokio::spawn(async move {
                if let Err(err) = stream_audio_rtp(socket, audio_key).await {
                    warn!(error = %err, "audio RTP sender stopped");
                }
            }));
        }
    }

    async fn stop_streams(&self) {
        let mut session = self.inner.lock().await;
        for task in session.stream_tasks.drain(..) {
            task.abort();
        }
        session.streams_started = false;
        session.video_socket = None;
        session.audio_socket = None;
        if let Some(task) = session.control_task.take() {
            task.abort();
        }
        self.control_crypto.clear_key();
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum MediaKind {
    Video,
    Audio,
    Control,
}

async fn bind_udp_port(bind_ip: IpAddr, port: u16) -> Result<UdpSocket> {
    let addr = SocketAddr::new(bind_ip, port);
    UdpSocket::bind(addr)
        .await
        .with_context(|| format!("failed to bind UDP port {addr}"))
}

async fn read_rtsp_request(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<Option<RtspRequest>> {
    let mut scratch = [0_u8; 4096];

    loop {
        if let Some(request_len) = complete_request_len(buffer)? {
            let raw = buffer.drain(..request_len).collect::<Vec<_>>();
            return Ok(Some(parse_rtsp_request(&raw)?));
        }

        let bytes_read = timeout(Duration::from_secs(5), stream.read(&mut scratch))
            .await
            .context("timed out reading RTSP request")?
            .context("failed to read RTSP request")?;
        if bytes_read == 0 {
            if buffer.is_empty() {
                return Ok(None);
            }
            bail!("RTSP connection closed with incomplete request");
        }

        buffer.extend_from_slice(&scratch[..bytes_read]);
        if buffer.len() > MAX_RTSP_REQUEST_BYTES {
            bail!("RTSP request exceeded maximum size");
        }
    }
}

fn complete_request_len(buffer: &[u8]) -> Result<Option<usize>> {
    let Some(header_end) = find_header_end(buffer) else {
        return Ok(None);
    };
    let headers = std::str::from_utf8(&buffer[..header_end])
        .context("RTSP request headers were not UTF-8")?;
    let content_length = content_length(headers)?;
    let request_len = header_end + 4 + content_length;
    Ok((buffer.len() >= request_len).then_some(request_len))
}

fn parse_rtsp_request(raw: &[u8]) -> Result<RtspRequest> {
    let header_end = find_header_end(raw).ok_or_else(|| anyhow!("RTSP headers not complete"))?;
    let headers_text =
        std::str::from_utf8(&raw[..header_end]).context("RTSP request was not UTF-8")?;
    let payload_start = header_end + 4;
    let body_length = content_length(headers_text)?;
    let payload_end = payload_start + body_length;
    if raw.len() < payload_end {
        bail!("RTSP payload shorter than Content-Length");
    }

    let mut lines = headers_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing RTSP request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing RTSP method"))?
        .to_string();
    let target = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing RTSP target"))?
        .to_string();
    let protocol = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing RTSP protocol"))?;
    if protocol != "RTSP/1.0" {
        bail!("unsupported RTSP protocol {protocol}");
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            warn!(line, "ignoring malformed RTSP header");
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    let payload = String::from_utf8_lossy(&raw[payload_start..payload_end]).to_string();
    Ok(RtspRequest {
        method,
        target,
        headers,
        payload,
    })
}

fn rtsp_response(request: &RtspRequest, source_ip: &str, close_connection: bool) -> String {
    match request.method.as_str() {
        "OPTIONS" => response(
            request.cseq(),
            &[
                (
                    "Public",
                    "OPTIONS, DESCRIBE, SETUP, ANNOUNCE, PLAY, GET_PARAMETER, TEARDOWN",
                ),
                ("Session", SESSION_ID),
            ],
            "",
            close_connection,
        ),
        "DESCRIBE" => {
            let payload = describe_sdp(source_ip);
            response(
                request.cseq(),
                &[("Content-Type", "application/sdp"), ("Session", SESSION_ID)],
                &payload,
                close_connection,
            )
        }
        "SETUP" => {
            let port = setup_port_for_target(&request.target);
            let transport = format!("unicast;server_port={port}-{};source={source_ip}", port + 1);
            response(
                request.cseq(),
                &[
                    ("Session", &format!("{SESSION_ID};timeout=90")),
                    ("Transport", transport.as_str()),
                ],
                "",
                close_connection,
            )
        }
        "ANNOUNCE" | "PLAY" | "GET_PARAMETER" | "TEARDOWN" => response(
            request.cseq(),
            &[("Session", &format!("{SESSION_ID};timeout=90"))],
            "",
            close_connection,
        ),
        method => {
            warn!(method, "unsupported RTSP method");
            format!(
                "RTSP/1.0 405 Method Not Allowed\r\nCSeq: {}\r\n\r\n",
                request.cseq()
            )
        }
    }
}

fn response(cseq: &str, headers: &[(&str, &str)], payload: &str, close_connection: bool) -> String {
    let mut response = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n");
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    if !payload.is_empty() {
        response.push_str("Content-Length: ");
        response.push_str(&payload.len().to_string());
        response.push_str("\r\n");
    }
    response.push_str(if close_connection {
        "Connection: close\r\n\r\n"
    } else {
        "Connection: keep-alive\r\n\r\n"
    });
    response.push_str(payload);
    response
}

fn describe_sdp(source_ip: &str) -> String {
    // This advertises synthetic H.264 and Opus streams only. Real capture, encoder negotiation,
    // encrypted audio, and full control-channel handling are intentionally still future work.
    [
        "v=0",
        &format!("o=sunrise 0 0 IN IP4 {source_ip}"),
        "s=Sunrise Desktop",
        &format!("c=IN IP4 {source_ip}"),
        "t=0 0",
        "a=control:*",
        "m=video 47998 RTP/AVP 96",
        "a=rtpmap:96 H264/90000",
        "a=fmtp:96 packetization-mode=1",
        "a=control:streamid=video",
        "m=audio 48000 RTP/AVP 97",
        "a=rtpmap:97 opus/48000/2",
        "a=fmtp:97 minptime=10;useinbandfec=1",
        "a=control:streamid=audio",
        "",
    ]
    .join("\r\n")
}

async fn stream_video_rtp(
    socket: Arc<UdpSocket>,
    mut control_events: broadcast::Receiver<control::ControlEvent>,
) -> Result<()> {
    let client = wait_for_udp_ping(&socket, "video").await?;
    let mut source = video_source_from_env()?;
    let mut packetizer = VideoPacketizer::new();
    let mut ticker = interval(source.frame_interval());

    info!(
        %client,
        source = source.description(),
        frames = source.frame_count_hint(),
        "starting video RTP sender"
    );
    loop {
        ticker.tick().await;
        if drain_video_control_events(&mut control_events) {
            debug!("received IDR request for current video stream");
            source
                .request_idr()
                .context("failed to request an IDR frame from video source")?;
        }
        let frame = source.next_frame()?;
        for packet in packetizer.packetize(&frame) {
            socket
                .send_to(&packet, client)
                .await
                .context("failed to send video RTP packet")?;
        }
    }
}

fn drain_video_control_events(rx: &mut broadcast::Receiver<control::ControlEvent>) -> bool {
    let mut idr_requested = false;
    loop {
        match rx.try_recv() {
            Ok(control::ControlEvent::IdrFrameRequested) => idr_requested = true,
            Ok(event) => trace_video_control_event(event),
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Closed) => break,
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                warn!(skipped, "video control event receiver lagged");
                idr_requested = true;
            }
        }
    }
    idr_requested
}

fn trace_video_control_event(event: control::ControlEvent) {
    debug!(?event, "received non-video-control GameStream event");
}

fn video_source_from_env() -> Result<Box<dyn VideoSource>> {
    let video_source = env::var("SUNRISE_VIDEO_SOURCE").ok();
    let live_nvenc = env::var("SUNRISE_LIVE_NVENC").ok();
    let h264_path = env::var("SUNRISE_H264_PATH").ok();

    match select_video_source(
        video_source.as_deref(),
        live_nvenc.as_deref(),
        h264_path.as_deref(),
    )? {
        VideoSourceChoice::NativeNvenc => {
            info!("selected RTSP video source: native-nvenc live capture");
            match encoder::native_nvenc_video_source_from_env() {
                Ok(source) => Ok(source),
                Err(err) => {
                    warn!(
                        error = %err,
                        "native D3D11 NVENC source failed; falling back to FFmpeg h264_nvenc capture source"
                    );
                    encoder::ffmpeg_nvenc_video_source_from_env()
                        .context("failed to create FFmpeg h264_nvenc fallback source")
                }
            }
        }
        VideoSourceChoice::Qsv => {
            info!("selected RTSP video source: qsv live capture");
            encoder::qsv_video_source_from_env()
        }
        VideoSourceChoice::AnnexB => {
            info!("selected RTSP video source: annex-b file");
            Ok(Box::new(AnnexBVideoSource::from_env()))
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum VideoSourceChoice {
    NativeNvenc,
    Qsv,
    AnnexB,
}

fn select_video_source(
    video_source: Option<&str>,
    live_nvenc: Option<&str>,
    h264_path: Option<&str>,
) -> Result<VideoSourceChoice> {
    if live_nvenc.is_some_and(|value| value == "1") {
        return Ok(VideoSourceChoice::NativeNvenc);
    }

    match video_source
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value)
            if value.eq_ignore_ascii_case("native-nvenc")
                || value.eq_ignore_ascii_case("capture") =>
        {
            Ok(VideoSourceChoice::NativeNvenc)
        }
        Some(value)
            if value.eq_ignore_ascii_case("qsv") || value.eq_ignore_ascii_case("h264_qsv") =>
        {
            Ok(VideoSourceChoice::Qsv)
        }
        Some(value)
            if value.eq_ignore_ascii_case("annex-b")
                || value.eq_ignore_ascii_case("file")
                || value.eq_ignore_ascii_case("h264") =>
        {
            Ok(VideoSourceChoice::AnnexB)
        }
        Some(value) => bail!(
            "unsupported SUNRISE_VIDEO_SOURCE={value:?}; expected native-nvenc, qsv, or annex-b"
        ),
        None if native_nvenc_video_source_available() => Ok(VideoSourceChoice::NativeNvenc),
        None if h264_path.is_some_and(|value| !value.trim().is_empty()) => {
            Ok(VideoSourceChoice::AnnexB)
        }
        None => Ok(VideoSourceChoice::AnnexB),
    }
}

fn native_nvenc_video_source_available() -> bool {
    cfg!(all(target_os = "windows", feature = "native-nvenc"))
}

async fn stream_audio_rtp(
    socket: Arc<UdpSocket>,
    audio_key: Option<AudioEncryptionKey>,
) -> Result<()> {
    let client = wait_for_udp_ping(&socket, "audio").await?;
    let mut source = OpusSilenceSource::new();
    let mut packetizer = match audio_key {
        Some(key) => AudioPacketizer::with_encryption(Some(key)),
        None => AudioPacketizer::new(),
    };
    let mut ticker = interval(source.packet_interval());

    info!(%client, encrypted = audio_key.is_some(), "starting synthetic audio RTP sender");
    loop {
        ticker.tick().await;
        let packet = packetizer.packetize(&source.next_packet());
        socket
            .send_to(&packet, client)
            .await
            .context("failed to send audio RTP packet")?;
    }
}

async fn wait_for_udp_ping(socket: &UdpSocket, media: &str) -> Result<SocketAddr> {
    let mut buffer = [0_u8; 1500];
    let (_, client) = timeout(Duration::from_secs(15), socket.recv_from(&mut buffer))
        .await
        .with_context(|| format!("timed out waiting for {media} UDP ping"))?
        .with_context(|| format!("failed to receive {media} UDP ping"))?;
    info!(%client, media, "received UDP stream ping");
    Ok(client)
}

fn setup_port_for_target(target: &str) -> u16 {
    match media_kind_for_target(target).unwrap_or(MediaKind::Video) {
        MediaKind::Video => VIDEO_PORT,
        MediaKind::Audio => AUDIO_PORT,
        MediaKind::Control => CONTROL_PORT,
    }
}

fn media_kind_for_target(target: &str) -> Option<MediaKind> {
    let target = target.to_ascii_lowercase();
    if target.contains("audio") {
        Some(MediaKind::Audio)
    } else if target.contains("control") {
        Some(MediaKind::Control)
    } else if target.contains("video")
        || target.contains("streamid")
        || target.starts_with("rtsp://")
    {
        Some(MediaKind::Video)
    } else {
        None
    }
}

fn content_length(headers: &str) -> Result<usize> {
    for line in headers.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            return value
                .trim()
                .parse::<usize>()
                .context("invalid RTSP Content-Length");
        }
    }
    Ok(0)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rtsp_request_with_headers_and_payload() {
        let request = parse_rtsp_request(
            b"ANNOUNCE streamid=control/13/0 RTSP/1.0\r\nCSeq: 4\r\nContent-Length: 5\r\n\r\nhello",
        )
        .unwrap();

        assert_eq!(request.method, "ANNOUNCE");
        assert_eq!(request.target, "streamid=control/13/0");
        assert_eq!(request.cseq(), "4");
        assert_eq!(request.payload, "hello");
    }

    #[test]
    fn describe_response_includes_sdp_payload() {
        let request =
            parse_rtsp_request(b"DESCRIBE rtsp://127.0.0.1:48010 RTSP/1.0\r\nCSeq: 2\r\n\r\n")
                .unwrap();

        let response = rtsp_response(&request, "127.0.0.1", false);

        assert!(response.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(response.contains("CSeq: 2\r\n"));
        assert!(response.contains("Content-Type: application/sdp\r\n"));
        assert!(response.contains("a=rtpmap:96 H264/90000"));
    }

    #[test]
    fn setup_response_advertises_expected_ports() {
        let request =
            parse_rtsp_request(b"SETUP streamid=audio RTSP/1.0\r\nCSeq: 3\r\n\r\n").unwrap();

        let response = rtsp_response(&request, "192.0.2.10", false);

        assert!(response.contains("Session: DEADBEEFCAFE;timeout=90\r\n"));
        assert!(
            response.contains("Transport: unicast;server_port=48000-48001;source=192.0.2.10\r\n")
        );
    }

    #[test]
    fn detects_complete_pipelined_request_without_consuming_followup() {
        let buffer = b"OPTIONS rtsp://127.0.0.1:48010 RTSP/1.0\r\nCSeq: 1\r\n\r\nDESCRIBE rtsp://127.0.0.1:48010 RTSP/1.0\r\nCSeq: 2\r\n\r\n";

        let request_len = complete_request_len(buffer).unwrap().unwrap();

        assert_eq!(
            std::str::from_utf8(&buffer[..request_len]).unwrap(),
            "OPTIONS rtsp://127.0.0.1:48010 RTSP/1.0\r\nCSeq: 1\r\n\r\n"
        );
        assert!(
            std::str::from_utf8(&buffer[request_len..])
                .unwrap()
                .starts_with("DESCRIBE")
        );
    }

    #[test]
    fn close_is_default_for_moonlight_tcp_rtsp_responses() {
        let request =
            parse_rtsp_request(b"OPTIONS rtsp://127.0.0.1:48010 RTSP/1.0\r\nCSeq: 1\r\n\r\n")
                .unwrap();

        let response = rtsp_response(&request, "127.0.0.1", true);

        assert!(response.contains("Connection: close\r\n"));
    }

    #[test]
    fn native_nvenc_video_source_is_selected_strictly() {
        assert_eq!(
            select_video_source(Some("native-nvenc"), None, Some("sample.h264")).unwrap(),
            VideoSourceChoice::NativeNvenc
        );
        assert_eq!(
            select_video_source(Some("NATIVE-NVENC"), None, None).unwrap(),
            VideoSourceChoice::NativeNvenc
        );
        assert_eq!(
            select_video_source(None, Some("1"), Some("sample.h264")).unwrap(),
            VideoSourceChoice::NativeNvenc
        );
        assert_eq!(
            select_video_source(Some("annex-b"), Some("0"), None).unwrap(),
            VideoSourceChoice::AnnexB
        );
        assert_eq!(
            select_video_source(Some("qsv"), None, Some("sample.h264")).unwrap(),
            VideoSourceChoice::Qsv
        );
    }

    #[test]
    fn default_video_source_prefers_live_capture_when_compiled_in() {
        let expected = if native_nvenc_video_source_available() {
            VideoSourceChoice::NativeNvenc
        } else {
            VideoSourceChoice::AnnexB
        };
        assert_eq!(
            select_video_source(None, None, Some("sample.h264")).unwrap(),
            expected
        );
    }

    #[test]
    fn invalid_video_source_is_rejected() {
        assert!(select_video_source(Some("surprise"), None, None).is_err());
    }

    #[test]
    fn drains_video_control_events_and_reports_idr_requests() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        tx.send(control::ControlEvent::StartA).unwrap();
        tx.send(control::ControlEvent::IdrFrameRequested).unwrap();

        assert!(drain_video_control_events(&mut rx));
        assert!(!drain_video_control_events(&mut rx));
    }
}
