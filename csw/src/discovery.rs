//! Finds the base station on the local network (see [`protocol::api`]).

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use protocol::api;
use tokio::net::UdpSocket;
use tokio::time::timeout;

const RETRY: Duration = Duration::from_secs(1);

/// Broadcasts beacons until a base station answers; returns its API URL.
pub async fn base_station_url() -> Result<String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    let mut reply = [0; 64];
    loop {
        socket
            .send_to(
                api::DISCOVERY_BEACON,
                (Ipv4Addr::BROADCAST, api::DISCOVERY_PORT),
            )
            .await?;
        while let Ok(received) = timeout(RETRY, socket.recv_from(&mut reply)).await {
            let (len, from): (usize, SocketAddr) = received?;
            if &reply[..len] == api::DISCOVERY_REPLY {
                return Ok(format!("http://{}:{}", from.ip(), api::PORT));
            }
        }
    }
}
