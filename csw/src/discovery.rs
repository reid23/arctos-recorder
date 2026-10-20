//! Finds the base station on the local network (see [`protocol::api`]).

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use protocol::api;
use tokio::net::UdpSocket;
use tokio::time::timeout;

const RETRY: Duration = Duration::from_secs(1);

/// Broadcasts beacons on every network until a base station answers;
/// returns its API URL.
pub async fn base_station_url() -> Result<String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    let mut reply = [0; 64];
    loop {
        // Each network's own broadcast address: a broadcast to all of them
        // goes out on only one, or nowhere without a default route, as on a
        // network of self-assigned addresses.
        for address in broadcast_addresses()? {
            if let Err(e) = socket
                .send_to(api::DISCOVERY_BEACON, (address, api::DISCOVERY_PORT))
                .await
            {
                tracing::debug!(%address, "sending a discovery beacon: {e}");
            }
        }
        while let Ok(received) = timeout(RETRY, socket.recv_from(&mut reply)).await {
            let (len, from): (usize, SocketAddr) = received?;
            if &reply[..len] == api::DISCOVERY_REPLY {
                return Ok(format!("http://{}:{}", from.ip(), api::PORT));
            }
        }
    }
}

/// The broadcast addresses of the IPv4 networks this computer is on now.
fn broadcast_addresses() -> Result<Vec<Ipv4Addr>> {
    let mut interfaces = std::ptr::null_mut();
    // SAFETY: getifaddrs fills in a list, freed below.
    if unsafe { libc::getifaddrs(&mut interfaces) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut addresses = Vec::new();
    let mut next = interfaces;
    // SAFETY: each entry, and the addresses it points to, stay valid until
    // the list is freed.
    while let Some(interface) = unsafe { next.as_ref() } {
        let flags = libc::IFF_UP as u32 | libc::IFF_BROADCAST as u32;
        let is_ipv4 = unsafe { interface.ifa_addr.as_ref() }
            .is_some_and(|address| address.sa_family == libc::AF_INET as libc::sa_family_t);
        if interface.ifa_flags & flags == flags
            && is_ipv4
            && let Some(broadcast) = unsafe { interface.ifa_ifu.cast::<libc::sockaddr_in>().as_ref() }
        {
            addresses.push(Ipv4Addr::from(u32::from_be(broadcast.sin_addr.s_addr)));
        }
        next = interface.ifa_next;
    }
    // SAFETY: the list came from getifaddrs and isn't used after this.
    unsafe { libc::freeifaddrs(interfaces) };
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_addresses_end_their_networks() {
        let addresses = broadcast_addresses().unwrap();
        assert!(addresses.iter().all(|address| !address.is_loopback() && address.octets()[3] != 0), "{addresses:?}");
    }
}
