//! Shared types and constants for the aris project.

use serde::{Deserialize, Serialize};

/// Supported CPU architectures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Architecture {
    /// ARM 64-bit (aarch64)
    #[serde(rename = "aarch64")]
    Aarch64,
    /// ARM 32-bit (armv7l)
    #[serde(rename = "armv7l")]
    Armv7l,
    /// RISC-V 64-bit
    #[serde(rename = "riscv64")]
    Riscv64,
    /// x86 64-bit
    #[serde(rename = "x86_64")]
    X8664,
}

impl Architecture {
    /// Rust target triple for this architecture (musl).
    pub fn rust_target(&self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64-unknown-linux-musl",
            Self::Armv7l => "armv7-unknown-linux-musleabihf",
            Self::Riscv64 => "riscv64gc-unknown-linux-musl",
            Self::X8664 => "x86_64-unknown-linux-musl",
        }
    }

    /// GCC target prefix (e.g. "aarch64-linux-musl").
    pub fn gcc_target(&self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64-linux-musl",
            Self::Armv7l => "arm-linux-musleabihf",
            Self::Riscv64 => "riscv64-linux-musl",
            Self::X8664 => "x86_64-linux-musl",
        }
    }
}

/// Partition role in the A/B update scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionRole {
    /// Active (currently booted) partition
    Active,
    /// Inactive (standby for OTA) partition
    Inactive,
    /// Persistent data partition (not A/B)
    Persistent,
}

/// Board configuration loaded from `configs/{name}.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardConfig {
    /// Human-readable board name
    pub name: String,
    /// CPU architecture
    #[serde(default = "default_arch")]
    pub arch: Architecture,
    /// SoC identifier
    #[serde(default)]
    pub soc: String,
    /// RAM size in megabytes
    #[serde(default)]
    pub ram_mb: u32,
    /// Kernel version to build
    #[serde(default = "default_kernel_version")]
    pub kernel_version: String,
    /// Kernel defconfig name
    #[serde(default)]
    pub kernel_defconfig: String,
    /// U-Boot defconfig name
    #[serde(default)]
    pub uboot_defconfig: String,
    /// Device tree filename (without .dts extension)
    #[serde(default)]
    pub dtb: String,
    /// Ethernet interface list
    #[serde(default)]
    pub eth_interfaces: Vec<String>,
    /// evernight feature flags to enable
    #[serde(default = "default_evernight_features")]
    pub evernight_features: Vec<String>,
    /// Entelecheia server URL for device registration
    #[serde(default)]
    pub entelecheia_server: String,
}

fn default_arch() -> Architecture {
    Architecture::Aarch64
}

fn default_kernel_version() -> String {
    "6.12".into()
}

fn default_evernight_features() -> Vec<String> {
    vec![
        "hardware".into(),
        "protocol".into(),
        "serial".into(),
        "sensor".into(),
        "bin".into(),
        "api".into(),
        "manifest".into(),
    ]
}

impl Default for BoardConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            arch: default_arch(),
            soc: String::new(),
            ram_mb: 2048,
            kernel_version: default_kernel_version(),
            kernel_defconfig: String::new(),
            uboot_defconfig: String::new(),
            dtb: String::new(),
            eth_interfaces: vec!["eth0".into(), "eth1".into()],
            evernight_features: default_evernight_features(),
            entelecheia_server: String::new(),
        }
    }
}
