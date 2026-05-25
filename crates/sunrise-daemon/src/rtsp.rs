use std::{collections::HashMap, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tracing::{debug, info, warn};

use crate::AppState;

const SESSION_ID: &str = "DEADBEEFCAFE";
const MAX_RTSP_REQUEST_BYTES: usize = 1024 * 1024;

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
    let request = read_rtsp_request(&mut stream).await?;
    info!(
        %remote,
        method = %request.method,
        target = %request.target,
        cseq = %request.cseq(),
        payload_len = request.payload.len(),
        "incoming RTSP request"
    );

    let source_ip = if remote.ip().is_loopback() {
        remote.ip().to_string()
    } else {
        state.local_ip.to_string()
    };
    let response = rtsp_response(&request, &source_ip);
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write RTSP response")?;
    stream.shutdown().await.ok();
    Ok(())
}

async fn read_rtsp_request(stream: &mut TcpStream) -> Result<RtspRequest> {
    let mut buffer = Vec::new();
    let mut scratch = [0_u8; 4096];

    loop {
        let bytes_read = timeout(Duration::from_secs(5), stream.read(&mut scratch))
            .await
            .context("timed out reading RTSP request")?
            .context("failed to read RTSP request")?;
        if bytes_read == 0 {
            break;
        }

        buffer.extend_from_slice(&scratch[..bytes_read]);
        if buffer.len() > MAX_RTSP_REQUEST_BYTES {
            bail!("RTSP request exceeded maximum size");
        }

        if request_complete(&buffer)? {
            break;
        }
    }

    parse_rtsp_request(&buffer)
}

fn request_complete(buffer: &[u8]) -> Result<bool> {
    let Some(header_end) = find_header_end(buffer) else {
        return Ok(false);
    };
    let headers = std::str::from_utf8(&buffer[..header_end])
        .context("RTSP request headers were not UTF-8")?;
    let content_length = content_length(headers)?;
    Ok(buffer.len() >= header_end + 4 + content_length)
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

fn rtsp_response(request: &RtspRequest, source_ip: &str) -> String {
    match request.method.as_str() {
        "OPTIONS" => response(
            request.cseq(),
            &[
                ("Public", "OPTIONS, DESCRIBE, SETUP, ANNOUNCE, PLAY"),
                ("Session", SESSION_ID),
            ],
            "",
        ),
        "DESCRIBE" => {
            let payload = describe_sdp(source_ip);
            response(
                request.cseq(),
                &[("Content-Type", "application/sdp"), ("Session", SESSION_ID)],
                &payload,
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
            )
        }
        "ANNOUNCE" | "PLAY" => response(
            request.cseq(),
            &[("Session", &format!("{SESSION_ID};timeout=90"))],
            "",
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

fn response(cseq: &str, headers: &[(&str, &str)], payload: &str) -> String {
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
    response.push_str("Connection: close\r\n\r\n");
    response.push_str(payload);
    response
}

fn describe_sdp(source_ip: &str) -> String {
    // This advertises placeholder H.264 and Opus streams only. The RTP producers are not
    // implemented yet, so Moonlight should progress through RTSP then fail later.
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

fn setup_port_for_target(target: &str) -> u16 {
    let target = target.to_ascii_lowercase();
    if target.contains("audio") {
        48000
    } else if target.contains("control") {
        47999
    } else {
        47998
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

        let response = rtsp_response(&request, "127.0.0.1");

        assert!(response.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(response.contains("CSeq: 2\r\n"));
        assert!(response.contains("Content-Type: application/sdp\r\n"));
        assert!(response.contains("a=rtpmap:96 H264/90000"));
    }

    #[test]
    fn setup_response_advertises_expected_ports() {
        let request =
            parse_rtsp_request(b"SETUP streamid=audio RTSP/1.0\r\nCSeq: 3\r\n\r\n").unwrap();

        let response = rtsp_response(&request, "192.0.2.10");

        assert!(response.contains("Session: DEADBEEFCAFE;timeout=90\r\n"));
        assert!(
            response.contains("Transport: unicast;server_port=48000-48001;source=192.0.2.10\r\n")
        );
    }
}
