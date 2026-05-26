use std::{
    net::{SocketAddr, UdpSocket as StdUdpSocket},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use rusty_enet::{Event, Host, HostSettings, Packet, Peer};
use tokio::{task::JoinHandle, time::interval};
use tracing::{debug, info, warn};

const CONTROL_CHANNELS: usize = 2;
const CONTROL_PEERS: usize = 8;
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(10);

type ControlHost = Host<StdUdpSocket>;
type ControlPeer = Peer<StdUdpSocket>;

pub(crate) fn spawn_control_server(port: u16) -> Result<JoinHandle<()>> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let host = control_host(addr)?;

    Ok(tokio::spawn(async move {
        if let Err(err) = run_control_server(host).await {
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

async fn run_control_server(mut host: ControlHost) -> Result<()> {
    let addr = host
        .socket()
        .local_addr()
        .context("failed to read ENet control address")?;
    info!(%addr, "ENet control server listening");
    let mut ticker = interval(CONTROL_POLL_INTERVAL);

    loop {
        ticker.tick().await;
        while let Some(event) = host.service()? {
            handle_control_event(event)?;
        }
        host.flush();
    }
}

fn handle_control_event(event: Event<StdUdpSocket>) -> Result<()> {
    match event {
        Event::Connect { peer, data } => {
            let peer_id = peer.id();
            info!(?peer_id, data, "ENet control client connected");
            send_control_marker(peer, b"SUNRISE-CONTROL-READY")?;
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
            debug!(
                ?peer_id,
                channel_id,
                len = packet.data().len(),
                "received ENet control packet"
            );
            send_control_marker(peer, b"SUNRISE-CONTROL-ACK")?;
        }
    }
    Ok(())
}

fn send_control_marker(peer: &mut ControlPeer, payload: &[u8]) -> Result<()> {
    let packet = Packet::reliable(payload);
    peer.send(0, &packet)
        .map_err(|err| anyhow!("failed to queue ENet control marker: {err:?}"))
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
                handle_control_event(event).unwrap();
                server_connected = true;
            }
            server.flush();

            while let Some(event) = client.service().unwrap() {
                match event {
                    Event::Connect { peer, .. } if peer.id() == client_peer => {
                        client_connected = true;
                    }
                    Event::Receive { packet, .. } => {
                        assert!(packet.data().starts_with(b"SUNRISE-CONTROL-"));
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
}
