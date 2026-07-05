//! aris-core — Device firmware supervisor.
//!
//! Runs on the gateway box as PID 1 (or systemd service). Manages:
//! - Hardware initialization (GPIO, LEDs, watchdog)
//! - Network configuration (dual Ethernet WAN/LAN)
//! - USB composite gadget (mass-storage + NCM virtual Ethernet)
//! - evernight daemon lifecycle (spawn, health-check, restart)
//! - OTA firmware updates (download, verify, apply, fallback)
//! - Device identity and provisioning

#![warn(missing_docs)]
#![deny(unsafe_code)]
#![allow(dead_code)] // Stub modules — implementation pending

use tracing::info;

mod led;
mod net;
mod ota;
mod usb;
mod watchdog;

/// Main entry point for the aris-core supervisor.
///
/// Startup sequence:
/// 1. Initialize hardware watchdog
/// 2. Configure dual Ethernet (WAN + LAN)
/// 3. Initialize USB composite gadget (mass-storage + NCM)
/// 4. Verify device identity (read from secure storage)
/// 5. Start evernight daemon as child process
/// 6. Enter supervision loop (health-check + restart on failure)
fn main() -> anyhow::Result<()> {
    #[cfg(feature = "cli")]
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!("aris-core starting up...");

    let _watchdog = watchdog::init()?;
    info!("watchdog initialized");

    net::configure()?;
    info!("networking configured");

    usb::init(&usb::UsbGadgetConfig::default())?;
    info!("USB gadget configured");

    Ok(())
}
