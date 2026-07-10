//! Hardware watchdog driver for the gateway box.
//!
//! Uses the SoC's built-in watchdog timer (WDT) to detect firmware
//! hangs and trigger an automatic reboot. On the RK3566 the WDT is
//! the Rockchip DesignWare watchdog exposed at `/dev/watchdog`.
//!
//! # Linux watchdog ABI
//!
//! The Linux watchdog ABI (`Documentation/watchdog/watchdog-api.rst`)
//! offers two equivalent ways to pat the dog:
//!
//! 1. **ioctl `WDIOC_KEEPALIVE`** — preferred for C programs.
//! 2. **`write(2)` any byte** to `/dev/watchdog` — preferred for
//!    safe Rust: writing to the device is treated by the kernel as a
//!    keepalive. This driver uses the write path to avoid `unsafe`
//!    ioctl plumbing entirely.
//!
//! The timeout is configured by writing the requested number of
//! seconds to `/sys/class/watchdog/watchdog0/timeout` (the standard
//! sysfs attribute, available since Linux 3.0). The hardware rounds
//! the value to a legal interval; we read it back.
//!
//! The Linux magic-close convention: writing the ASCII byte `'V'`
//! before `close(2)` instructs the kernel to disable the watchdog,
//! so a clean shutdown does not trigger a reboot. We honor this in
//! [`Drop`] so a supervisor that exits normally does not reboot the
//! board.
//!
//! # Hardware dependency
//!
//! On a host without `/dev/watchdog` (CI, dev laptop) [`init`]
//! returns a [`WatchdogGuard`] in `unarmed` mode: `feed()` is a
//! no-op and `is_armed()` is false. This lets the supervisor run
//! end-to-end on a development host.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

/// Default watchdog device node on Linux.
pub const WATCHDOG_DEVICE: &str = "/dev/watchdog";

/// Default sysfs class root for the first watchdog device.
pub const WATCHDOG_SYSFS: &str = "/sys/class/watchdog";

/// Default timeout (seconds). The RK3566 WDT supports 1–600 s;
/// 30 s gives the supervisor enough headroom to complete a slow
/// OTA write or fsck under load.
pub const DEFAULT_TIMEOUT_SECS: u32 = 30;

/// Magic byte written to `/dev/watchdog` to request a clean disable.
///
/// Defined in `<linux/watchdog.h>` as `WATCHDOG_NOWAYOUT`'s opposite:
/// any character other than `'V'` followed by a close *will* reboot
/// the board; writing `'V'` first signals "I am shutting down on
/// purpose".
pub const WATCHDOG_DISABLE_MAGIC: u8 = b'V';

/// Watchdog guard. Feeding the dog prevents a reboot.
///
/// The guard owns the `/dev/watchdog` file descriptor. Drop it (or
/// call [`WatchdogGuard::disable`]) to request a clean shutdown; the
/// supervisor should call [`WatchdogGuard::feed`] periodically — more
/// often than the configured timeout.
pub struct WatchdogGuard {
    file: Option<Mutex<std::fs::File>>,
    /// Effective timeout after hardware rounding, in seconds.
    timeout_secs: u32,
    /// Whether the watchdog is actually armed. Host builds and unit
    /// tests construct an unarmed guard so the kernel ABI is never
    /// exercised on a host without a watchdog.
    armed: bool,
}

impl WatchdogGuard {
    /// Feed the watchdog to prevent a timeout reboot.
    ///
    /// Writes any byte to `/dev/watchdog`; the kernel treats every
    /// successful write as a keepalive. On an unarmed guard this is
    /// a no-op.
    pub fn feed(&self) {
        if !self.armed {
            return;
        }
        match self.feed_inner() {
            Ok(()) => debug!(timeout = self.timeout_secs, "watchdog patted"),
            Err(e) => warn!(error = %e, "watchdog keepalive failed"),
        }
    }

    fn feed_inner(&self) -> Result<()> {
        let guard = self
            .file
            .as_ref()
            .context("watchdog not open (armed guard without file)")?;
        let mut f = guard
            .lock()
            .expect("watchdog mutex poisoned (supervisor panicked mid-feed)");
        f.write_all(b"\0")
            .with_context(|| format!("keepalive write to {WATCHDOG_DEVICE}"))?;
        Ok(())
    }

    /// Effective timeout after the hardware rounded our request.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs as u64)
    }

    /// Whether this guard is actually arming a real watchdog.
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Disable the watchdog cleanly.
    ///
    /// Writes the magic `'V'` byte so the kernel knows not to reboot
    /// when the fd is closed. After this the guard is inert; further
    /// [`feed`](Self::feed) calls are no-ops.
    pub fn disable(mut self) {
        if !self.armed {
            return;
        }
        if let Some(m) = self.file.take() {
            if let Ok(mut f) = m.into_inner() {
                if let Err(e) = f.write_all(&[WATCHDOG_DISABLE_MAGIC]) {
                    warn!(error = %e, "failed to write watchdog disable magic");
                }
            }
        }
        self.armed = false;
    }
}

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // On graceful shutdown, request a clean disable by writing
        // the magic 'V' byte. If that fails the kernel will reboot
        // when the fd closes — which is the safe default for a
        // supervisor that died without acknowledging the watchdog.
        if let Some(ref m) = self.file {
            if let Ok(mut f) = m.lock() {
                if let Err(e) = f.write_all(&[WATCHDOG_DISABLE_MAGIC]) {
                    warn!(error = %e, "watchdog magic-close write failed; board will reboot");
                }
            }
        }
    }
}

/// Initialize the hardware watchdog with the default 30 s timeout.
pub fn init() -> Result<WatchdogGuard> {
    init_with_timeout(DEFAULT_TIMEOUT_SECS)
}

/// Initialize the hardware watchdog, requesting `timeout_secs`.
///
/// On a host without `/dev/watchdog` (CI, dev laptop) this returns an
/// *unarmed* guard: `is_armed()` is false and `feed()` is a no-op.
/// This lets the supervisor run end-to-end on a development host.
pub fn init_with_timeout(timeout_secs: u32) -> Result<WatchdogGuard> {
    let dev = Path::new(WATCHDOG_DEVICE);
    if !dev.exists() {
        info!(timeout_secs, "watchdog device absent; running unarmed");
        return Ok(WatchdogGuard {
            file: None,
            timeout_secs,
            armed: false,
        });
    }

    info!(timeout_secs, device = %WATCHDOG_DEVICE, "arming hardware watchdog");

    // Best-effort: request the timeout via sysfs before opening the
    // device. Opening the device starts the timer; setting the
    // timeout afterwards risks a race where the kernel fires before
    // our write lands.
    let effective = set_timeout_sysfs(timeout_secs).unwrap_or_else(|e| {
        warn!(error = %e, "could not set watchdog timeout via sysfs; using kernel default");
        timeout_secs
    });

    let file = OpenOptions::new()
        .write(true)
        .open(dev)
        .with_context(|| format!("open {WATCHDOG_DEVICE}"))?;

    Ok(WatchdogGuard {
        file: Some(Mutex::new(file)),
        timeout_secs: effective,
        armed: true,
    })
}

/// Write the requested timeout to the first watchdog device's sysfs
/// attribute and return the value the kernel accepted.
fn set_timeout_sysfs(secs: u32) -> Result<u32> {
    let timeout_path = watchdog_sysfs_path()?.join("timeout");
    fs::write(&timeout_path, secs.to_string())
        .with_context(|| format!("write {}", timeout_path.display()))?;
    let accepted = fs::read_to_string(&timeout_path)
        .with_context(|| format!("read back {}", timeout_path.display()))?
        .trim()
        .parse::<u32>()
        .context("non-numeric timeout read back from sysfs")?;
    Ok(accepted)
}

/// Resolve the sysfs directory for the default watchdog. The Linux
/// convention is `/sys/class/watchdog/watchdog0`, but some SoCs
/// enumerate additional instances; we pick the first that exposes a
/// `timeout` attribute.
fn watchdog_sysfs_path() -> Result<PathBuf> {
    let root = Path::new(WATCHDOG_SYSFS);
    if !root.is_dir() {
        anyhow::bail!("watchdog sysfs root {WATCHDOG_SYSFS} not found");
    }
    for entry in fs::read_dir(root).context("enumerate watchdog sysfs entries")? {
        let entry = entry?;
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && entry.path().join("timeout").exists()
        {
            return Ok(entry.path());
        }
    }
    anyhow::bail!("no /sys/class/watchdog/* instance with a timeout attribute")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_magic_byte() {
        assert_eq!(WATCHDOG_DISABLE_MAGIC, b'V');
    }

    #[test]
    fn default_timeout_is_reasonable() {
        assert!(DEFAULT_TIMEOUT_SECS >= 5 && DEFAULT_TIMEOUT_SECS <= 120);
    }

    #[test]
    fn unarmed_guard_is_noop() {
        let g = WatchdogGuard {
            file: None,
            timeout_secs: 30,
            armed: false,
        };
        g.feed(); // must not panic
        assert!(!g.is_armed());
        assert_eq!(g.timeout(), Duration::from_secs(30));
    }

    #[test]
    fn init_returns_unarmed_on_host_without_device() {
        if Path::new(WATCHDOG_DEVICE).exists() {
            return; // skip on hosts that actually have a watchdog
        }
        let g = init().expect("init should succeed unarmed");
        assert!(!g.is_armed());
    }
}
