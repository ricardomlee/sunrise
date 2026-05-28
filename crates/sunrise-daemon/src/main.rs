mod capture;
mod control;
mod encoder;
mod identity;
mod media;
mod pairing;
mod rtsp;
mod tls;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Query, State},
    http::{Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use identity::ServerIdentity;
use pairing::PairingState;
use sunrise_config::{SunriseConfig, default_config_path, load_or_generate};
use sunrise_protocol::{ServerInfo, serverinfo_xml};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) config: Arc<Mutex<SunriseConfig>>,
    pub(crate) local_ip: Arc<String>,
    pub(crate) pairing: PairingState,
    pub(crate) current_game: Arc<Mutex<u32>>,
    pub(crate) rtsp: rtsp::RtspState,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let command = parse_command()?;
    if let Command::CaptureSmoke(options) = command {
        let report = capture::run_capture_smoke(options)?;
        info!(
            output = %report.output_path.display(),
            monitor_index = report.monitor_index,
            monitor_name = report.monitor_name.as_deref().unwrap_or("unknown"),
            width = report.width,
            height = report.height,
            row_pitch = report.row_pitch,
            depth_pitch = report.depth_pitch,
            source_format = %report.source_format,
            bytes_written = report.bytes_written,
            "Windows capture smoke completed"
        );
        return Ok(());
    }
    if let Command::WgcSmoke(options) = command {
        let report = capture::run_wgc_smoke(options)?;
        info!(
            output = %report.output_path.display(),
            monitor_index = report.monitor_index,
            monitor_name = report.monitor_name.as_deref().unwrap_or("unknown"),
            width = report.width,
            height = report.height,
            row_pitch = report.row_pitch,
            depth_pitch = report.depth_pitch,
            source_format = %report.source_format,
            bytes_written = report.bytes_written,
            "Windows Graphics Capture smoke completed"
        );
        return Ok(());
    }
    if let Command::CaptureLoop(options) = command {
        let report = capture::run_capture_loop(options)?;
        let fps = f64::from(report.frames) / report.elapsed.as_secs_f64().max(0.001);
        info!(
            monitor_index = report.monitor_index,
            monitor_name = report.monitor_name.as_deref().unwrap_or("unknown"),
            width = report.width,
            height = report.height,
            frames = report.frames,
            elapsed_ms = report.elapsed.as_millis(),
            fps,
            source_format = %report.source_format,
            "Windows capture loop completed"
        );
        return Ok(());
    }
    if let Command::CaptureList = command {
        capture::run_capture_list()?;
        return Ok(());
    }
    if let Command::EncodeSmoke(options) = command {
        let report = encoder::run_encode_smoke(options)?;
        let fps = f64::from(report.frames) / report.elapsed.as_secs_f64().max(0.001);
        info!(
            output = %report.output_path.display(),
            encoder = %report.encoder,
            width = report.width,
            height = report.height,
            frames = report.frames,
            elapsed_ms = report.elapsed.as_millis(),
            fps,
            bytes_written = report.bytes_written,
            nal_units = report.nal_units,
            source_format = %report.source_format,
            "H.264 encode smoke completed"
        );
        return Ok(());
    }
    if let Command::QsvSmoke(options) = command {
        let report = encoder::run_encode_smoke(options)?;
        let fps = f64::from(report.frames) / report.elapsed.as_secs_f64().max(0.001);
        info!(
            output = %report.output_path.display(),
            encoder = %report.encoder,
            width = report.width,
            height = report.height,
            frames = report.frames,
            elapsed_ms = report.elapsed.as_millis(),
            fps,
            bytes_written = report.bytes_written,
            nal_units = report.nal_units,
            source_format = %report.source_format,
            "QSV H.264 encode smoke completed"
        );
        return Ok(());
    }
    if let Command::NativeNvencSmoke(options) = command {
        let report = encoder::run_native_nvenc_smoke(options)?;
        let fps = f64::from(report.frames) / report.elapsed.as_secs_f64().max(0.001);
        info!(
            output = %report.output_path.display(),
            width = report.width,
            height = report.height,
            frames = report.frames,
            elapsed_ms = report.elapsed.as_millis(),
            fps,
            bytes_written = report.bytes_written,
            nal_units = report.nal_units,
            source_format = %report.source_format,
            "native D3D11 zero-copy NVENC smoke completed"
        );
        return Ok(());
    }
    let Command::Serve { config_path } = command else {
        unreachable!("capture smoke command returns before daemon startup");
    };
    let (mut config, created) = load_or_generate(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    if created {
        info!(path = %config_path.display(), "generated default config");
    } else {
        info!(path = %config_path.display(), "loaded config");
    }
    info!(
        unique_id = %config.unique_id,
        uuid = %config.uuid,
        mac = %config.mac_address,
        "sunrise host identity"
    );

    let (identity, identity_created) = ServerIdentity::load_or_generate(&mut config)?;
    if identity_created {
        config.write(&config_path).with_context(|| {
            format!(
                "failed to persist server certificate {}",
                config_path.display()
            )
        })?;
        info!("generated and persisted RSA-2048 certificate for pairing and HTTPS");
    } else {
        info!("loaded persisted RSA-2048 certificate for pairing and HTTPS");
    }

    let bind_ip = bind_ip_from_env()?;
    let local_ip = advertised_local_ip(bind_ip);
    let http_port = config.http_port;
    let https_port = config.https_port;
    let rtsp_port = config.rtsp_port;
    info!(%bind_ip, local_ip = %local_ip, "using listener bind address");
    let paired_clients = config
        .paired_clients
        .iter()
        .map(|client| client.unique_id.clone())
        .collect();
    let shared_config = Arc::new(Mutex::new(config));
    let identity = Arc::new(identity);
    let pairing = PairingState::new(
        Arc::clone(&identity),
        config_path.clone(),
        Arc::clone(&shared_config),
        paired_clients,
    );

    let state = AppState {
        config: Arc::clone(&shared_config),
        local_ip: Arc::new(local_ip),
        pairing,
        current_game: Arc::new(Mutex::new(0)),
        rtsp: rtsp::RtspState::new(bind_ip),
    };

    let http_addr = SocketAddr::new(bind_ip, http_port);
    let https_addr = SocketAddr::new(bind_ip, https_port);
    let rtsp_addr = SocketAddr::new(bind_ip, rtsp_port);

    let http = serve_http(http_addr, state.clone());
    let https = tls::serve_https(https_addr, identity, state.clone());
    let rtsp = rtsp::serve_rtsp(rtsp_addr, state);

    tokio::select! {
        result = http => result,
        result = https => result,
        result = rtsp => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for ctrl-c")?;
            info!("shutdown requested");
            Ok(())
        }
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn bind_ip_from_env() -> Result<IpAddr> {
    match std::env::var("SUNRISE_BIND_IP") {
        Ok(value) => parse_bind_ip(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_bind_ip(None),
        Err(err) => Err(anyhow!("failed to read SUNRISE_BIND_IP: {err}")),
    }
}

fn parse_bind_ip(value: Option<&str>) -> Result<IpAddr> {
    let Some(value) = value else {
        return Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!("SUNRISE_BIND_IP was set but empty"));
    }
    value
        .parse::<IpAddr>()
        .with_context(|| format!("failed to parse SUNRISE_BIND_IP={value:?} as an IP address"))
}

enum Command {
    Serve { config_path: PathBuf },
    CaptureSmoke(capture::CaptureSmokeOptions),
    WgcSmoke(capture::CaptureSmokeOptions),
    CaptureLoop(capture::CaptureLoopOptions),
    CaptureList,
    EncodeSmoke(encoder::EncodeSmokeOptions),
    QsvSmoke(encoder::EncodeSmokeOptions),
    NativeNvencSmoke(encoder::NativeNvencSmokeOptions),
}

fn parse_command() -> Result<Command> {
    let mut args = std::env::args().skip(1).peekable();
    match args.peek().map(String::as_str) {
        None => Ok(Command::Serve {
            config_path: default_config_path(),
        }),
        Some("--config") => {
            args.next();
            let Some(path) = args.next() else {
                return Err(anyhow!("missing path after --config"));
            };
            if args.next().is_some() {
                return Err(anyhow!(usage()));
            }
            Ok(Command::Serve {
                config_path: PathBuf::from(path),
            })
        }
        Some("capture-smoke") => {
            args.next();
            parse_capture_smoke(args).map(Command::CaptureSmoke)
        }
        Some("wgc-smoke") => {
            args.next();
            parse_capture_smoke(args).map(Command::WgcSmoke)
        }
        Some("capture-loop") => {
            args.next();
            parse_capture_loop(args).map(Command::CaptureLoop)
        }
        Some("capture-list") => {
            args.next();
            if args.next().is_some() {
                return Err(anyhow!(usage()));
            }
            Ok(Command::CaptureList)
        }
        Some("encode-smoke") => {
            args.next();
            parse_encode_smoke(args).map(Command::EncodeSmoke)
        }
        Some("qsv-smoke") => {
            args.next();
            parse_qsv_smoke(args).map(Command::QsvSmoke)
        }
        Some("native-nvenc-smoke") => {
            args.next();
            parse_native_nvenc_smoke(args).map(Command::NativeNvencSmoke)
        }
        _ => Err(anyhow!(usage())),
    }
}

fn parse_capture_smoke<I>(args: I) -> Result<capture::CaptureSmokeOptions>
where
    I: IntoIterator<Item = String>,
{
    let mut output_path = PathBuf::from("target/capture-smoke/frame.bmp");
    let mut monitor_index = None;
    let mut timeout_ms = 33_u32;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --output"));
                };
                output_path = PathBuf::from(value);
            }
            "--monitor" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --monitor"));
                };
                monitor_index = Some(
                    value
                        .parse::<usize>()
                        .context("failed to parse --monitor as a one-based monitor index")?,
                );
            }
            "--timeout-ms" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --timeout-ms"));
                };
                timeout_ms = value
                    .parse::<u32>()
                    .context("failed to parse --timeout-ms as milliseconds")?;
            }
            _ => return Err(anyhow!(usage())),
        }
    }

    Ok(capture::CaptureSmokeOptions {
        output_path,
        source: capture::CaptureSourceOptions {
            monitor_index,
            timeout_ms,
        },
    })
}

fn parse_capture_loop<I>(args: I) -> Result<capture::CaptureLoopOptions>
where
    I: IntoIterator<Item = String>,
{
    let mut frame_count = 120_u32;
    let mut monitor_index = None;
    let mut timeout_ms = 33_u32;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frames" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --frames"));
                };
                frame_count = value
                    .parse::<u32>()
                    .context("failed to parse --frames as a frame count")?;
            }
            "--monitor" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --monitor"));
                };
                monitor_index = Some(
                    value
                        .parse::<usize>()
                        .context("failed to parse --monitor as a one-based monitor index")?,
                );
            }
            "--timeout-ms" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --timeout-ms"));
                };
                timeout_ms = value
                    .parse::<u32>()
                    .context("failed to parse --timeout-ms as milliseconds")?;
            }
            _ => return Err(anyhow!(usage())),
        }
    }

    Ok(capture::CaptureLoopOptions {
        source: capture::CaptureSourceOptions {
            monitor_index,
            timeout_ms,
        },
        frame_count,
    })
}

fn parse_encode_smoke<I>(args: I) -> Result<encoder::EncodeSmokeOptions>
where
    I: IntoIterator<Item = String>,
{
    let mut output_path = PathBuf::from("target/capture-smoke/capture.h264");
    let mut ffmpeg_path = PathBuf::from("ffmpeg.exe");
    let mut encoder = "auto".to_string();
    let mut frame_count = 120_u32;
    let mut fps = 30_u32;
    let mut monitor_index = None;
    let mut timeout_ms = 33_u32;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --output"));
                };
                output_path = PathBuf::from(value);
            }
            "--ffmpeg" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --ffmpeg"));
                };
                ffmpeg_path = PathBuf::from(value);
            }
            "--encoder" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --encoder"));
                };
                encoder = value;
            }
            "--frames" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --frames"));
                };
                frame_count = value
                    .parse::<u32>()
                    .context("failed to parse --frames as a frame count")?;
            }
            "--fps" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --fps"));
                };
                fps = value.parse::<u32>().context("failed to parse --fps")?;
            }
            "--monitor" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --monitor"));
                };
                monitor_index = Some(
                    value
                        .parse::<usize>()
                        .context("failed to parse --monitor as a one-based monitor index")?,
                );
            }
            "--timeout-ms" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --timeout-ms"));
                };
                timeout_ms = value
                    .parse::<u32>()
                    .context("failed to parse --timeout-ms as milliseconds")?;
            }
            _ => return Err(anyhow!(usage())),
        }
    }

    Ok(encoder::EncodeSmokeOptions {
        source: capture::CaptureSourceOptions {
            monitor_index,
            timeout_ms,
        },
        output_path,
        ffmpeg_path,
        encoder,
        frame_count,
        fps,
    })
}

fn parse_qsv_smoke<I>(args: I) -> Result<encoder::EncodeSmokeOptions>
where
    I: IntoIterator<Item = String>,
{
    let mut options = parse_encode_smoke(args)?;
    options.encoder = "h264_qsv".to_string();
    if options.output_path == PathBuf::from("target/capture-smoke/capture.h264") {
        options.output_path = PathBuf::from("target/capture-smoke/qsv.h264");
    }
    Ok(options)
}

fn parse_native_nvenc_smoke<I>(args: I) -> Result<encoder::NativeNvencSmokeOptions>
where
    I: IntoIterator<Item = String>,
{
    let mut output_path = PathBuf::from("target/capture-smoke/native-nvenc.h264");
    let mut frame_count = 120_u32;
    let mut fps = 30_u32;
    let mut monitor_index = None;
    let mut timeout_ms = 33_u32;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --output"));
                };
                output_path = PathBuf::from(value);
            }
            "--frames" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --frames"));
                };
                frame_count = value
                    .parse::<u32>()
                    .context("failed to parse --frames as a frame count")?;
            }
            "--fps" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --fps"));
                };
                fps = value.parse::<u32>().context("failed to parse --fps")?;
            }
            "--monitor" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --monitor"));
                };
                monitor_index = Some(
                    value
                        .parse::<usize>()
                        .context("failed to parse --monitor as a one-based monitor index")?,
                );
            }
            "--timeout-ms" => {
                let Some(value) = args.next() else {
                    return Err(anyhow!("missing value after --timeout-ms"));
                };
                timeout_ms = value
                    .parse::<u32>()
                    .context("failed to parse --timeout-ms as milliseconds")?;
            }
            _ => return Err(anyhow!(usage())),
        }
    }

    Ok(encoder::NativeNvencSmokeOptions {
        source: capture::CaptureSourceOptions {
            monitor_index,
            timeout_ms,
        },
        output_path,
        frame_count,
        fps,
    })
}

fn usage() -> &'static str {
    "usage: cargo run -p sunrise-daemon -- [--config path/to/sunrise.toml]\n       cargo run -p sunrise-daemon --features capture-windows -- capture-list\n       cargo run -p sunrise-daemon --features capture-windows -- capture-smoke [--monitor 1] [--output target/capture-smoke/frame.bmp] [--timeout-ms 33]\n       cargo run -p sunrise-daemon --features capture-windows -- wgc-smoke [--monitor 1] [--output target/capture-smoke/frame.bmp] [--timeout-ms 1000]\n       cargo run -p sunrise-daemon --features capture-windows -- capture-loop [--monitor 1] [--frames 120] [--timeout-ms 33]\n       cargo run -p sunrise-daemon --features capture-windows -- encode-smoke [--monitor 1] [--frames 120] [--fps 30] [--encoder auto|h264_nvenc|h264_qsv|libx264] [--ffmpeg ffmpeg.exe] [--output target/capture-smoke/capture.h264]\n       cargo run -p sunrise-daemon --features capture-windows -- qsv-smoke [--monitor 1] [--frames 120] [--fps 30] [--ffmpeg ffmpeg.exe] [--output target/capture-smoke/qsv.h264]\n       cargo run -p sunrise-daemon --features native-nvenc -- native-nvenc-smoke [--monitor 1] [--frames 120] [--fps 30] [--output target/capture-smoke/native-nvenc.h264]"
}

async fn serve_http(addr: SocketAddr, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/", get(root))
        .route("/serverinfo", get(serverinfo))
        .route("/pair", get(pairing::pair))
        .route("/unpair", get(pairing::unpair))
        .layer(middleware::from_fn(log_http_request))
        .with_state(state);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {addr}"))?;
    info!(%addr, "HTTP server listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("HTTP server failed")
}

async fn log_http_request(request: Request<Body>, next: Next) -> Response {
    let remote = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect_info| connect_info.0.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();

    info!(remote = %remote, path = %path, query = %query, "incoming HTTP request");
    next.run(request).await
}

async fn root(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    render_serverinfo(&state, query.get("uniqueid").map(String::as_str)).await
}

async fn serverinfo(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    info!("serverinfo requested");
    render_serverinfo(&state, query.get("uniqueid").map(String::as_str)).await
}

async fn render_serverinfo(state: &AppState, client_unique_id: Option<&str>) -> Response {
    let paired = match client_unique_id {
        Some(unique_id) => state.pairing.is_paired(unique_id).await,
        None => false,
    };
    let config = state.config.lock().await;
    let mut info = ServerInfo::from_config(&config, state.local_ip.as_str(), paired);
    info.current_game = *state.current_game.lock().await;
    if info.current_game != 0 {
        info.state = "SUNSHINE_SERVER_BUSY".to_string();
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        serverinfo_xml(&info),
    )
        .into_response()
}

fn detect_local_ip() -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("8.8.8.8:80")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|err| {
            warn!(%err, "failed to detect local IP, falling back to 127.0.0.1");
            "127.0.0.1".to_string()
        })
}

fn advertised_local_ip(bind_ip: IpAddr) -> String {
    if bind_ip.is_unspecified() {
        detect_local_ip()
    } else {
        bind_ip.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::parse_bind_ip;

    #[test]
    fn default_bind_ip_is_all_interfaces() {
        assert_eq!(
            parse_bind_ip(None).unwrap(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn parses_explicit_bind_ip() {
        assert_eq!(
            parse_bind_ip(Some("192.168.2.10")).unwrap(),
            IpAddr::from([192, 168, 2, 10])
        );
    }

    #[test]
    fn rejects_empty_bind_ip() {
        assert!(parse_bind_ip(Some(" ")).is_err());
    }
}
