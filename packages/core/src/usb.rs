//! USB gadget management for the gateway.
//!
//! When the board is connected to a host (PC, phone, tablet) via the USB-C
//! port, it presents itself as a composite USB device:
//!
//! 1. **Mass Storage** — a FAT image containing per-OS auto-installer scripts
//!    for evernight.  The host sees a USB drive; opening the appropriate
//!    installer installs the evernight client and configures the connection.
//!
//! 2. **CDC-NCM** — a virtual Ethernet adapter.  This gives the host a direct
//!    IP link to the gateway without needing an external network.  The gateway
//!    runs a small DHCP server on this interface.
//!
//! The gadget is configured at boot by the `aris-usb-gadget` shell script
//! (called from init.d/S50usbgadget).  This module provides a Rust API for
//! runtime management — switching modes, regenerating the installer image,
//! and querying the current gadget state.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Configuration for the USB composite gadget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbGadgetConfig {
    /// Whether USB gadget mode is enabled at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// USB Vendor ID (hex string, e.g. "0x1d6b").
    #[serde(default = "default_vid")]
    pub id_vendor: String,
    /// USB Product ID (hex string, e.g. "0x0104").
    #[serde(default = "default_pid")]
    pub id_product: String,
    /// Manufacturer string shown to the host OS.
    #[serde(default = "default_manufacturer")]
    pub manufacturer: String,
    /// Product name string shown to the host OS.
    #[serde(default = "default_product")]
    pub product: String,
    /// Path to the mass-storage backing image (FAT filesystem).
    #[serde(default = "default_ms_file")]
    pub mass_storage_file: PathBuf,
    /// Virtual network IP assigned to the gateway side of the USB link.
    #[serde(default = "default_usb_ip")]
    pub usb_net_ip: String,
    /// CIDR prefix for the USB virtual network.
    #[serde(default = "default_cidr")]
    pub usb_net_cidr: u8,
    /// DHCP range start for clients on the USB network.
    #[serde(default = "default_dhcp_start")]
    pub usb_net_dhcp_start: String,
    /// DHCP range end for clients on the USB network.
    #[serde(default = "default_dhcp_end")]
    pub usb_net_dhcp_end: String,
}

impl Default for UsbGadgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            id_vendor: default_vid(),
            id_product: default_pid(),
            manufacturer: default_manufacturer(),
            product: default_product(),
            mass_storage_file: default_ms_file(),
            usb_net_ip: default_usb_ip(),
            usb_net_cidr: default_cidr(),
            usb_net_dhcp_start: default_dhcp_start(),
            usb_net_dhcp_end: default_dhcp_end(),
        }
    }
}

/// Runtime state of the USB gadget.
#[derive(Debug, Clone, Serialize)]
pub struct GadgetState {
    /// Whether the gadget is currently bound to a UDC.
    pub active: bool,
    /// The UDC (USB Device Controller) the gadget is bound to.
    pub udc: Option<String>,
    /// USB functions currently active.
    pub functions: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn default_vid() -> String {
    "0x1d6b".into()
}
fn default_pid() -> String {
    "0x0104".into()
}
fn default_manufacturer() -> String {
    "celestia-island".into()
}
fn default_product() -> String {
    "Entelecheia Gateway".into()
}
fn default_ms_file() -> PathBuf {
    PathBuf::from("/usr/share/evernight-gadget/installer.img")
}
fn default_usb_ip() -> String {
    "10.0.99.1".into()
}
fn default_cidr() -> u8 {
    24
}
fn default_dhcp_start() -> String {
    "10.0.99.100".into()
}
fn default_dhcp_end() -> String {
    "10.0.99.200".into()
}

/// Initialize the USB gadget subsystem.
///
/// Calls the `aris-usb-gadget start` shell script to configure the composite
/// gadget via configfs.  This is idempotent — if the gadget is already running,
/// it will be torn down and recreated.
pub fn init(config: &UsbGadgetConfig) -> Result<()> {
    if !config.enabled {
        info!("USB gadget disabled in config, skipping");
        return Ok(());
    }

    info!(
        "initializing USB gadget: VID={} PID={} product={:?}",
        config.id_vendor, config.id_product, config.product
    );

    // Ensure the mass-storage backing image exists
    if !config.mass_storage_file.exists() {
        warn!(
            "mass-storage backing image not found at {:?}, generating...",
            config.mass_storage_file
        );
        generate_installer_image(&config.mass_storage_file)
            .context("failed to generate installer image")?;
    }

    // Run the shell script
    let output = Command::new("/usr/sbin/aris-usb-gadget")
        .arg("start")
        .env("GADGET_MS_FILE", &config.mass_storage_file)
        .env("GADGET_VID", &config.id_vendor)
        .env("GADGET_PID", &config.id_product)
        .env("GADGET_MANUFACTURER", &config.manufacturer)
        .env("GADGET_PRODUCT", &config.product)
        .env("USB_NET_IP", &config.usb_net_ip)
        .env("USB_NET_CIDR", config.usb_net_cidr.to_string())
        .output()
        .context("failed to execute aris-usb-gadget start")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("aris-usb-gadget start failed: {stderr}");
    }

    info!("USB gadget initialized");
    Ok(())
}

/// Tear down the USB gadget.
pub fn shutdown() -> Result<()> {
    let output = Command::new("/usr/sbin/aris-usb-gadget")
        .arg("stop")
        .output()
        .context("failed to execute aris-usb-gadget stop")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("aris-usb-gadget stop failed: {stderr}");
    }
    Ok(())
}

/// Query the current gadget state.
pub fn state() -> Result<GadgetState> {
    let gadget_dir = Path::new("/sys/kernel/config/usb_gadget/aris_gadget");

    if !gadget_dir.exists() {
        return Ok(GadgetState {
            active: false,
            udc: None,
            functions: vec![],
        });
    }

    let udc_path = gadget_dir.join("UDC");
    let udc = std::fs::read_to_string(&udc_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let func_dir = gadget_dir.join("functions");
    let mut functions = vec![];
    if func_dir.exists() {
        for entry in std::fs::read_dir(&func_dir).into_iter().flatten().flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                functions.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }

    Ok(GadgetState {
        active: udc.is_some(),
        udc,
        functions,
    })
}

/// Generate (or regenerate) the FAT installer image that is exposed as
/// a virtual USB drive to the host.
///
/// This image is populated from `package/` in the firmware rootfs and
/// contains:
/// - `Windows/install_evernight.bat` / `install_evernight.exe`
/// - `Linux/install_evernight.sh`
/// - `macOS/install_evernight.command`
/// - `Android/install_evernight.apk` (or a fallback `.txt` with instructions)
/// - `README.txt` / `README_zh.txt`
/// - `evernight` binary (the gateway client for each target arch)
///
/// The image is created with `mkfs.vfat` inside a loop-mounted file.  If
/// `mkfs.vfat` or `mtools` are not available, this function returns an error
/// (the build system should have pre-generated the image during firmware
/// assembly).
pub fn generate_installer_image(dest: &Path) -> Result<()> {
    let src_dir = Path::new("/usr/share/evernight-gadget/payload");
    let image_size_mb: u64 = 32;

    // Create a zero-filled file
    let f = std::fs::File::create(dest).context("create image file")?;
    f.set_len(image_size_mb * 1024 * 1024)
        .context("set image file length")?;
    drop(f);

    // Format as FAT32
    let status = Command::new("mkfs.vfat")
        .arg("-F")
        .arg("32")
        .arg("-n")
        .arg("ARIS_GW")
        .arg(dest)
        .status()
        .context("mkfs.vfat not found — install dosfstools or pre-generate the image")?;

    if !status.success() {
        anyhow::bail!("mkfs.vfat failed on {:?}", dest);
    }

    // Copy payload files using mtools (mcopy) if available
    if src_dir.exists() {
        let status = Command::new("mcopy")
            .arg("-s")
            .arg("-i")
            .arg(dest)
            .arg(format!("{}/.", src_dir.display()))
            .arg("::")
            .status();

        match status {
            Ok(s) if s.success() => {
                info!("installer image generated at {:?}", dest);
                Ok(())
            }
            _ => {
                warn!(
                    "mcopy not available or failed; image is empty. Populate manually or install mtools."
                );
                Ok(())
            }
        }
    } else {
        warn!(
            "payload directory {:?} does not exist; installer image will be empty",
            src_dir
        );
        Ok(())
    }
}
