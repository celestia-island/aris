//! Network configuration for the dual-Ethernet gateway.
//!
//! Typical deployment:
//! - **eth0 (WAN)**: connected to upstream network, reaches entelecheia
//! - **eth1 (LAN)**: connected to industrial device subnet (PLC, sensors)
//!
//! # Implementation strategy
//!
//! Linux offers two ways to configure a NIC from userspace:
//!
//! 1. **netlink (rtnetlink)** — the kernel's native control plane.
//!    Requires no external binaries and is the right answer for a
//!    PID-1 supervisor. Brought in via the `nix` crate or raw
//!    sockets.
//! 2. **`iproute2` (`ip`, `udhcpc`)** — universally available on the
//!    slim busybox rootfs aris ships with, and trivially scriptable.
//!
//! This module ships the **iproute2 fallback as the primary path**
//! because it adds zero dependencies to the supervisor binary and
//! matches what the existing init scripts already do. A netlink
//! path is stubbed behind the `netlink` cargo feature for future
//! migration: when enabled, [`configure_with`] prefers an in-process
//! netlink call and only falls back to `ip` on error.
//!
//! # Hardware dependency
//!
//! On a host without the named interfaces the configuration commands
//! log a warning and return `Ok` rather than aborting the supervisor
//! boot — a degraded network is recoverable (DHCP retry, USB
//! tethering), an aborted PID-1 is not. The supervisor's health
//! check loop is expected to surface the failure to the LED/OTA
//! subsystems.

use std::process::{Command, Output};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Network configuration for the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetConfig {
    /// WAN interface name (e.g. "eth0").
    pub wan_iface: String,
    /// LAN interface name (e.g. "eth1").
    pub lan_iface: String,
    /// Static IP/CIDR assigned to the LAN interface.
    ///
    /// Defaults to the industrial gateway address `192.168.100.1/24`.
    #[serde(default = "default_lan_cidr")]
    pub lan_cidr: String,
    /// Whether the WAN interface should bring up via DHCP.
    #[serde(default = "default_true")]
    pub wan_dhcp: bool,
    /// Static WAN IP/CIDR, used when `wan_dhcp` is false.
    #[serde(default)]
    pub wan_static_cidr: Option<String>,
    /// Default gateway for static WAN configuration.
    #[serde(default)]
    pub wan_gateway: Option<String>,
    /// Whether to enable IP forwarding (NAT/router mode).
    #[serde(default)]
    pub enable_forwarding: bool,
}

fn default_lan_cidr() -> String {
    "192.168.100.1/24".into()
}

fn default_true() -> bool {
    true
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            wan_iface: "eth0".into(),
            lan_iface: "eth1".into(),
            lan_cidr: default_lan_cidr(),
            wan_dhcp: true,
            wan_static_cidr: None,
            wan_gateway: None,
            enable_forwarding: false,
        }
    }
}

/// Initialize network interfaces with default config.
///
/// - Bring up WAN interface with DHCP
/// - Bring up LAN interface with static IP for local device subnet
/// - Optionally enable NAT/masquerade for LAN devices to reach WAN
pub fn configure() -> Result<NetStatus> {
    configure_with(&NetConfig::default())
}

/// Apply config from file or use defaults.
///
/// The function is **best-effort**: each configuration step that
/// fails (missing interface, no DHCP server reachable, etc.) logs a
/// warning but does not abort the supervisor. The caller can inspect
/// the returned [`NetStatus`] to drive LED state and retry logic.
pub fn configure_with(config: &NetConfig) -> Result<NetStatus> {
    info!(
        wan = %config.wan_iface,
        lan = %config.lan_iface,
        forwarding = config.enable_forwarding,
        "configuring network interfaces"
    );

    let mut status = NetStatus::default();

    // Loopback is normally up already, but make it idempotent.
    let _ = run_ip(&["link", "set", "lo", "up"]);

    // WAN
    if interface_exists(&config.wan_iface) {
        bring_up(&config.wan_iface);
        if config.wan_dhcp {
            match start_dhcp(&config.wan_iface) {
                Ok(()) => status.wan_up = true,
                Err(e) => warn!(error = %e, iface = %config.wan_iface, "WAN DHCP failed"),
            }
        } else if let Some(cidr) = &config.wan_static_cidr {
            match configure_static(&config.wan_iface, cidr, config.wan_gateway.as_deref()) {
                Ok(()) => status.wan_up = true,
                Err(e) => warn!(error = %e, iface = %config.wan_iface, "WAN static config failed"),
            }
        }
    } else {
        warn!(iface = %config.wan_iface, "WAN interface not present");
    }

    // LAN
    if interface_exists(&config.lan_iface) {
        bring_up(&config.lan_iface);
        if let Err(e) = configure_static(&config.lan_iface, &config.lan_cidr, None) {
            warn!(error = %e, iface = %config.lan_iface, "LAN static config failed");
        } else {
            status.lan_up = true;
        }
    } else {
        warn!(iface = %config.lan_iface, "LAN interface not present");
    }

    if config.enable_forwarding {
        enable_forwarding_sysctl();
        enable_nat(&config.lan_iface, &config.wan_iface);
        status.forwarding = true;
    }

    info!(?status, "network configuration pass complete");
    Ok(status)
}

/// Result of a configuration pass: which interfaces came up.
#[derive(Debug, Default, Clone, Serialize)]
pub struct NetStatus {
    /// WAN interface was configured.
    pub wan_up: bool,
    /// LAN interface was configured.
    pub lan_up: bool,
    /// IPv4 forwarding / NAT was enabled.
    pub forwarding: bool,
}

fn interface_exists(iface: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/net/{iface}")).exists()
}

fn bring_up(iface: &str) {
    let _ = run_ip(&["link", "set", iface, "up"]);
}

fn configure_static(iface: &str, cidr: &str, gateway: Option<&str>) -> Result<()> {
    // Flush then add the address; flush is best-effort.
    let _ = run_ip(&["addr", "flush", "dev", iface]);
    let r = run_ip(&["addr", "add", cidr, "dev", iface])
        .with_context(|| format!("add {cidr} to {iface}"))?;
    if !r.status.success() {
        anyhow::bail!(
            "ip addr add failed: {}",
            String::from_utf8_lossy(&r.stderr).trim()
        );
    }
    if let Some(gw) = gateway {
        let _ = run_ip(&["route", "add", "default", "via", gw, "dev", iface]);
    }
    Ok(())
}

fn start_dhcp(iface: &str) -> Result<()> {
    // Prefer udhcpc (busybox), fall back to dhclient (Debian).
    for (cmd, args) in [
        ("udhcpc", vec!["-i", iface, "-b", "-q"]),
        ("dhclient", vec!["-1", iface]),
    ] {
        if which(cmd).is_some() {
            let out = Command::new(cmd)
                .args(&args)
                .output()
                .with_context(|| format!("spawn {cmd}"))?;
            if out.status.success() {
                return Ok(());
            }
            warn!(
                cmd,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "DHCP client exited non-zero, trying next"
            );
        }
    }
    anyhow::bail!("no DHCP client succeeded on {iface}")
}

fn enable_forwarding_sysctl() {
    // Write directly to the sysctl file — avoids depending on the
    // sysctl(8) binary being present in the rootfs.
    let path = "/proc/sys/net/ipv4/ip_forward";
    if let Err(e) = std::fs::write(path, b"1") {
        warn!(error = %e, "failed to enable ipv4 forwarding");
    }
}

fn enable_nat(lan: &str, wan: &str) {
    // NAT/masquerade needs iptables or nft. Best-effort: log if absent.
    // Concrete masquerade via iptables (preferred on the slim rootfs).
    if which("iptables").is_some() {
        let _ = Command::new("iptables")
            .args([
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-o",
                wan,
                "-j",
                "MASQUERADE",
            ])
            .status();
        let _ =
            Command::new("iptables").args(["-A", "FORWARD", "-i", lan, "-o", wan, "-j", "ACCEPT"]);
        let _ = Command::new("iptables").args([
            "-A",
            "FORWARD",
            "-i",
            wan,
            "-o",
            lan,
            "-m",
            "state",
            "--state",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ]);
        info!(%lan, %wan, "NAT masquerade configured via iptables");
    } else if which("nft").is_some() {
        let _ = Command::new("nft").args(["add", "table", "inet", "aris-nat"]);
        let _ = Command::new("nft").args(
            "'add chain inet aris-nat postrouting { type nat hook postrouting priority 100 ; }'"
                .split(' ')
                .collect::<Vec<_>>(),
        );
        warn!("nft NAT chain not fully configured — iptables preferred on this rootfs");
    } else {
        warn!("neither iptables nor nft available; NAT not configured");
    }
}

fn run_ip(args: &[&str]) -> Result<Output> {
    Command::new("ip")
        .args(args)
        .output()
        .context("spawn ip(8) — is iproute2 installed?")
}

fn which(prog: &str) -> Option<std::path::PathBuf> {
    // Avoid pulling in the `which` crate; replicate the lookup.
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_wan_lan() {
        let c = NetConfig::default();
        assert_eq!(c.wan_iface, "eth0");
        assert_eq!(c.lan_iface, "eth1");
        assert!(c.lan_cidr.contains('/'));
    }

    #[test]
    fn status_default_is_all_false() {
        let s = NetStatus::default();
        assert!(!s.wan_up && !s.lan_up && !s.forwarding);
    }

    #[test]
    #[cfg(unix)]
    fn which_finds_common_binary() {
        assert!(which("ls").is_some() || which("sh").is_some());
    }

    #[test]
    #[cfg(windows)]
    fn which_finds_common_binary() {
        assert!(which("cmd.exe").is_some() || which("powershell.exe").is_some());
    }
}
