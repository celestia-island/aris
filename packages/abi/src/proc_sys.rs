// SPDX-License-Identifier: BUSL-1.1

//! Minimal /proc and /sys emulation for kei.
//!
//! kei does not implement procfs or sysfs. Standard Linux binaries
//! often read from `/proc/cpuinfo`, `/proc/meminfo`, `/proc/self/maps`,
//! `/sys/class/net/`, etc. This module generates synthetic files that
//! return plausible values.

use std::collections::HashMap;
use std::io;

/// Provides synthetic /proc and /sys file contents.
pub struct ProcSysEmulator {
    /// Pre-generated file contents keyed by path.
    files: HashMap<String, String>,
}

impl Default for ProcSysEmulator {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcSysEmulator {
    pub fn new() -> Self {
        let mut files = HashMap::new();

        // /proc/cpuinfo
        files.insert("/proc/cpuinfo".to_string(), generate_cpuinfo());
        files.insert("/proc/meminfo".to_string(), generate_meminfo());
        files.insert("/proc/uptime".to_string(), "0.00 0.00\n".to_string());
        files.insert("/proc/loadavg".to_string(), "0.00 0.00 0.00 1/1 1\n".to_string());
        files.insert("/proc/version".to_string(), generate_version());

        // /proc/self
        files.insert("/proc/self/status".to_string(), generate_process_status());

        // /sys entries
        files.insert("/sys/class/net/eth0/operstate".to_string(), "up\n".to_string());

        Self { files }
    }

    /// Returns the synthetic content for a /proc or /sys path.
    pub fn read(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(|s| s.as_str())
    }

    /// Writes synthetic files to a directory (for tmpfs overlay).
    pub fn write_to_dir(&self, dir: &str) -> io::Result<()> {
        for (path, content) in &self.files {
            let full_path = format!("{}{}", dir, path);
            if let Some(parent) = std::path::Path::new(&full_path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&full_path, content)?;
        }
        Ok(())
    }

    /// Adds or replaces a synthetic file.
    pub fn set(&mut self, path: &str, content: &str) {
        self.files.insert(path.to_string(), content.to_string());
    }
}

fn generate_cpuinfo() -> String {
    "processor\t: 0\n\
     BogoMIPS\t: 100.00\n\
     Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics\n\
     CPU implementer\t: 0x41\n\
     CPU architecture: 8\n\
     CPU variant\t: 0x0\n\
     CPU part\t: 0xd08\n\
     CPU revision\t: 3\n\n"
        .to_string()
}

fn generate_meminfo() -> String {
    "MemTotal:         2097152 kB\n\
     MemFree:          1048576 kB\n\
     MemAvailable:     1572864 kB\n\
     Buffers:            8192 kB\n\
     Cached:           524288 kB\n\
     SwapCached:            0 kB\n\
     Active:           262144 kB\n\
     Inactive:         262144 kB\n"
        .to_string()
}

fn generate_version() -> String {
    "Linux version 6.12.0-kei (kei@celestia) (Rust OS kernel, Asterinas fork)\n"
        .to_string()
}

fn generate_process_status() -> String {
    "Name:\tinit\n\
     Umask:\t0022\n\
     State:\tS (sleeping)\n\
     Tgid:\t1\n\
     Pid:\t1\n\
     PPid:\t0\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_cpuinfo() {
        let emu = ProcSysEmulator::new();
        let cpuinfo = emu.read("/proc/cpuinfo").unwrap();
        assert!(cpuinfo.contains("processor"));
        assert!(cpuinfo.contains("BogoMIPS"));
    }

    #[test]
    fn test_read_meminfo() {
        let emu = ProcSysEmulator::new();
        let meminfo = emu.read("/proc/meminfo").unwrap();
        assert!(meminfo.contains("MemTotal"));
        assert!(meminfo.contains("MemFree"));
    }

    #[test]
    fn test_set_custom_file() {
        let mut emu = ProcSysEmulator::new();
        emu.set("/proc/custom", "test data\n");
        assert_eq!(emu.read("/proc/custom"), Some("test data\n"));
    }

    #[test]
    fn test_missing_path_returns_none() {
        let emu = ProcSysEmulator::new();
        assert_eq!(emu.read("/proc/nonexistent"), None);
    }
}
