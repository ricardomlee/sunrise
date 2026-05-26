use std::{net::IpAddr, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use sunrise_protocol::{ServerInfo, applist_xml, cancel_xml, launch_xml, pair_xml, serverinfo_xml};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::{AppState, identity::ServerIdentity};

pub async fn serve_https(
    addr: SocketAddr,
    identity: Arc<ServerIdentity>,
    state: AppState,
) -> Result<()> {
    let tls_config = Arc::new(identity.tls_config()?);
    let acceptor = TlsAcceptor::from(tls_config);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind HTTPS listener on {addr}"))?;

    info!(%addr, "HTTPS applist skeleton listening");
    info!("TODO: persist server certificate and paired Moonlight client certificates");
    info!("TODO: implement strict client certificate verification for HTTPS");
    info!("TODO: store paired Moonlight client certificates");
    info!("TODO: connect /launch to real app/session lifecycle");
    info!("TODO: replace RTSP skeleton with negotiated stream setup");
    info!("TODO: implement RTP video");
    info!("TODO: implement RTP audio");
    info!("TODO: implement ENet control");

    loop {
        let (stream, remote) = listener.accept().await.context("HTTPS accept failed")?;
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_tls_connection(acceptor, stream, remote, state).await {
                debug!(%remote, error = %err, "HTTPS connection ended");
            }
        });
    }
}

async fn handle_tls_connection(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    remote: SocketAddr,
    state: AppState,
) -> Result<()> {
    let mut stream = acceptor
        .accept(stream)
        .await
        .context("TLS handshake failed")?;

    let mut buffer = vec![0_u8; 8192];
    let bytes_read = timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .context("timed out reading HTTPS request")?
        .context("failed to read HTTPS request")?;
    if bytes_read == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let (path, query) = split_path_query(target);

    info!(%remote, path = %path, query = %query, "incoming HTTPS request");

    let (status, body) = if method == "GET" && path == "/serverinfo" {
        let unique_id = query_value(query, "uniqueid");
        let paired = match unique_id {
            Some(unique_id) => state.pairing.is_paired(unique_id).await,
            None => false,
        };
        let config = state.config.lock().await;
        let mut info = ServerInfo::from_config(&config, state.local_ip.as_str(), paired);
        info.current_game = *state.current_game.lock().await;
        if info.current_game != 0 {
            info.state = "SUNSHINE_SERVER_BUSY".to_string();
        }
        info!(paired = paired, "HTTPS serverinfo requested");
        ("200 OK", serverinfo_xml(&info))
    } else if method == "GET" && path == "/applist" {
        info!("applist requested");
        ("200 OK", applist_xml())
    } else if method == "GET" && (path == "/launch" || path == "/resume") {
        let app_id = query_value(query, "appid")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(1);
        *state.current_game.lock().await = app_id;
        let rtsp_url = rtsp_session_url(remote, &state).await;
        info!(app_id, %rtsp_url, route = %path, "launch/resume requested");
        ("200 OK", launch_xml(&rtsp_url))
    } else if method == "GET" && path == "/cancel" {
        *state.current_game.lock().await = 0;
        info!("cancel requested");
        ("200 OK", cancel_xml())
    } else if method == "GET" && path == "/pair" && query.contains("phrase=pairchallenge") {
        info!("HTTPS pairchallenge requested");
        ("200 OK", pair_xml([("paired", "1")]))
    } else {
        warn!(%remote, %method, %path, "HTTPS route not implemented");
        (
            "404 Not Found",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root status_code=\"404\" />\n"
                .to_string(),
        )
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write HTTPS response")?;
    stream.shutdown().await.ok();
    Ok(())
}

fn split_path_query(target: &str) -> (&str, &str) {
    target.split_once('?').unwrap_or((target, ""))
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
}

async fn rtsp_session_url(remote: SocketAddr, state: &AppState) -> String {
    let config = state.config.lock().await;
    let host = if remote.ip().is_loopback() {
        remote.ip().to_string()
    } else {
        state.local_ip.to_string()
    };
    format!("rtsp://{}:{}", format_rtsp_host(&host), config.rtsp_port)
}

fn format_rtsp_host(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.to_string(),
    }
}
