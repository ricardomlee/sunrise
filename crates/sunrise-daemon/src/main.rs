mod identity;
mod pairing;
mod tls;

use std::{
    collections::HashMap,
    net::{SocketAddr, UdpSocket},
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
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config_path = parse_config_path()?;
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

    let local_ip = detect_local_ip();
    let http_port = config.http_port;
    let https_port = config.https_port;
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
    };

    let http_addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    let https_addr = SocketAddr::from(([0, 0, 0, 0], https_port));

    let http = serve_http(http_addr, state.clone());
    let https = tls::serve_https(https_addr, identity, state);

    tokio::select! {
        result = http => result,
        result = https => result,
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

fn parse_config_path() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next(), args.next()) {
        (None, None, None) => Ok(default_config_path()),
        (Some("--config"), Some(path), None) => Ok(PathBuf::from(path)),
        _ => Err(anyhow!(
            "usage: cargo run -p sunrise-daemon -- [--config path/to/sunrise.toml]"
        )),
    }
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
    let info = ServerInfo::from_config(&config, state.local_ip.as_str(), paired);
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
