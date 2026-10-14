//! Answers the cameras' discovery beacons (see [`protocol::api`]).

use std::net::Ipv4Addr;

use anyhow::Result;
use protocol::api;
use tokio::net::UdpSocket;

pub async fn run() -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, api::DISCOVERY_PORT)).await?;
    let mut beacon = [0; 64];
    loop {
        let (len, from) = socket.recv_from(&mut beacon).await?;
        if &beacon[..len] == api::DISCOVERY_BEACON {
            socket.send_to(api::DISCOVERY_REPLY, from).await?;
        }
    }
}
