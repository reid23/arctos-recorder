//! The wired port the cameras are plugged into, on Linux. With no DHCP
//! server on the cameras' network, macOS and Windows give a port a
//! self-assigned (link-local) address, as the cameras do, but
//! NetworkManager keeps waiting for one; so that port gets a profile of its
//! own giving it a self-assigned address.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::hub::Hub;

/// The NetworkManager profile for the cameras' port.
const PROFILE: &str = "Arctos Recorder cameras";
/// Higher than the profiles NetworkManager makes by itself, so this one is
/// used whenever the port is connected.
const PROFILE_PRIORITY: &str = "100";
/// How often the ports are read, as adapters come and go.
const REFRESH: Duration = Duration::from_secs(5);
/// How long to wait for the port to take its new profile.
const ACTIVATION_TIMEOUT_SECONDS: &str = "20";

/// The wired ports and which the cameras are plugged into.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CameraPort {
    /// Wired ports NetworkManager manages, by device name.
    pub choices: Vec<String>,
    /// The one with the cameras' profile.
    pub chosen: Option<String>,
    /// Why the ports couldn't be read or the choice couldn't be made.
    pub error: Option<String>,
}

/// Keeps [`Hub::camera_port`] current.
pub async fn follow(hub: Arc<Hub>) {
    let mut ticker = tokio::time::interval(REFRESH);
    loop {
        ticker.tick().await;
        let read = read().await;
        hub.camera_port.send_if_modified(|port| {
            let before = port.clone();
            match read {
                Ok((choices, chosen)) => {
                    port.choices = choices;
                    port.chosen = chosen;
                }
                Err(e) => port.error = Some(format!("{e:#}")),
            }
            *port != before
        });
    }
}

/// Gives `port` the cameras' profile, taking it from any other port, or
/// removes the profile.
pub async fn choose(hub: &Hub, port: Option<String>) {
    let result = async {
        set_profile(port.as_deref()).await?;
        read().await
    }
    .await;
    hub.camera_port.send_modify(|camera_port| match result {
        Ok((choices, chosen)) => {
            *camera_port = CameraPort {
                choices,
                chosen,
                error: None,
            }
        }
        Err(e) => camera_port.error = Some(format!("{e:#}")),
    });
}

/// The wired ports, and which has the cameras' profile.
async fn read() -> Result<(Vec<String>, Option<String>)> {
    let devices = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE", "device", "status"]).await?;
    let chosen = if profile_exists().await? {
        let port = nmcli(&["-g", "connection.interface-name", "connection", "show", PROFILE]).await?;
        Some(port.trim().to_owned()).filter(|port| !port.is_empty())
    } else {
        None
    };
    Ok((wired_ports(&devices), chosen))
}

async fn set_profile(port: Option<&str>) -> Result<()> {
    let exists = profile_exists().await?;
    match port {
        Some(port) if exists => {
            nmcli(&["connection", "modify", PROFILE, "connection.interface-name", port]).await?;
        }
        Some(port) => {
            nmcli(&[
                "connection", "add", "type", "ethernet", "con-name", PROFILE, "ifname", port,
                "ipv4.method", "link-local", "ipv6.method", "link-local",
                "connection.autoconnect-priority", PROFILE_PRIORITY,
            ])
            .await?;
        }
        None if exists => {
            nmcli(&["connection", "delete", PROFILE]).await?;
            return Ok(());
        }
        None => return Ok(()),
    }
    // Without a cable plugged in, the profile is used once one is.
    if let Err(e) = nmcli(&["--wait", ACTIVATION_TIMEOUT_SECONDS, "connection", "up", PROFILE]).await {
        tracing::info!("the cameras' port isn't set up yet: {e:#}");
    }
    Ok(())
}

async fn profile_exists() -> Result<bool> {
    let profiles = nmcli(&["-t", "-f", "NAME", "connection", "show"]).await?;
    Ok(profiles.lines().any(|name| name == PROFILE))
}

async fn nmcli(args: &[&str]) -> Result<String> {
    let output = Command::new("nmcli")
        .args(args)
        .output()
        .await
        .context("running NetworkManager's nmcli")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// The managed Ethernet devices in `nmcli -t -f DEVICE,TYPE,STATE device
/// status` output.
fn wired_ports(devices: &str) -> Vec<String> {
    devices
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let (device, kind, state) = (fields.next()?, fields.next()?, fields.next()?);
            (kind == "ethernet" && state != "unmanaged").then(|| device.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_managed_ethernet_devices_are_ports() {
        let devices = "wlo1:wifi:connected\n\
                       enx00e04c680b82:ethernet:connected\n\
                       enp3s0:ethernet:unavailable\n\
                       docker0:bridge:connected (externally)\n\
                       veth1:ethernet:unmanaged\n\
                       lo:loopback:unmanaged\n";
        assert_eq!(wired_ports(devices), ["enx00e04c680b82", "enp3s0"]);
    }
}
