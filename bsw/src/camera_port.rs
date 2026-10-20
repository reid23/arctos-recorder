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
/// What NetworkManager must allow a user (see `nmcli general permissions`)
/// to set up the port without an administrator's password.
const PERMISSIONS: [&str; 2] = [
    "org.freedesktop.NetworkManager.settings.modify.system",
    "org.freedesktop.NetworkManager.network-control",
];
/// pkexec's exit statuses when its password prompt was dismissed, and when
/// it couldn't get permission otherwise.
const PKEXEC_DISMISSED: i32 = 126;
const PKEXEC_NOT_AUTHORIZED: i32 = 127;

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
    let change = match port {
        Some(port) if exists => {
            shell_line(&["nmcli", "connection", "modify", PROFILE, "connection.interface-name", port])
        }
        Some(port) => shell_line(&[
            "nmcli", "connection", "add", "type", "ethernet", "con-name", PROFILE, "ifname", port,
            "ipv4.method", "link-local", "ipv6.method", "link-local",
            "connection.autoconnect-priority", PROFILE_PRIORITY,
        ]),
        None if exists => shell_line(&["nmcli", "connection", "delete", PROFILE]),
        None => return Ok(()),
    };
    let script = match port {
        // Without a cable plugged in, the profile is used once one is.
        Some(_) => format!(
            "{change} && {{ {} || true; }}",
            shell_line(&["nmcli", "--wait", ACTIVATION_TIMEOUT_SECONDS, "connection", "up", PROFILE])
        ),
        None => change,
    };
    let errors = change_settings(&script).await?;
    if !errors.is_empty() {
        tracing::info!("setting up the cameras' port: {errors}");
    }
    Ok(())
}

/// Runs `script` with sh, as root if NetworkManager doesn't let this user
/// set up the port: pkexec then asks for an administrator's password, once
/// for the whole script. Returns what it wrote to stderr.
async fn change_settings(script: &str) -> Result<String> {
    let permissions = nmcli(&["-t", "-f", "PERMISSION,VALUE", "general", "permissions"]).await?;
    let as_administrator = !may_set_up_port(&permissions);
    let mut command = Command::new(if as_administrator { "pkexec" } else { "sh" });
    if as_administrator {
        command.arg("sh");
    }
    let output = command
        .args(["-c", script])
        .output()
        .await
        .context("running NetworkManager's nmcli")?;
    let errors = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    match output.status.code() {
        Some(0) => Ok(errors),
        Some(PKEXEC_DISMISSED) if as_administrator => {
            bail!("Changing the cameras' port needs an administrator's password")
        }
        Some(PKEXEC_NOT_AUTHORIZED) if as_administrator => {
            tracing::warn!("pkexec: {errors}");
            bail!("Not allowed to change the network settings")
        }
        _ => bail!("{errors}"),
    }
}

/// Whether `nmcli -t -f PERMISSION,VALUE general permissions` output lets
/// this user set up the port by themselves.
fn may_set_up_port(permissions: &str) -> bool {
    PERMISSIONS.iter().all(|permission| {
        permissions
            .lines()
            .any(|line| line.strip_prefix(permission) == Some(":yes"))
    })
}

/// `words` as a line sh splits back into them.
fn shell_line(words: &[&str]) -> String {
    words
        .iter()
        .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
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

    #[test]
    fn only_users_allowed_both_changes_set_up_the_port_by_themselves() {
        let permissions = |modify, control| {
            format!(
                "org.freedesktop.NetworkManager.enable-disable-network:no\n\
                 org.freedesktop.NetworkManager.network-control:{control}\n\
                 org.freedesktop.NetworkManager.settings.modify.own:auth\n\
                 org.freedesktop.NetworkManager.settings.modify.system:{modify}\n"
            )
        };
        assert!(may_set_up_port(&permissions("yes", "yes")));
        assert!(!may_set_up_port(&permissions("no", "auth")));
        assert!(!may_set_up_port(&permissions("auth", "yes")));
        assert!(!may_set_up_port(&permissions("yes", "no")));
        assert!(!may_set_up_port(""));
    }

    #[test]
    fn shell_lines_quote_every_word() {
        assert_eq!(
            shell_line(&["nmcli", "Arctos Recorder cameras", "it's", "$HOME"]),
            r#"'nmcli' 'Arctos Recorder cameras' 'it'\''s' '$HOME'"#
        );
    }
}
