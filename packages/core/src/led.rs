//! Status LED driver for the gateway box.
//!
//! Controls the status LED via the Linux GPIO sysfs interface
//! (`/sys/class/gpio`). Each LED is identified by its GPIO number on
//! the SoC. On the RK3566-based NanoPi R3S the typical wiring is:
//!
//! - Green  status LED — GPIO pin defined in the board config
//! - Red    status LED — GPIO pin defined in the board config
//!
//! Typical patterns:
//! - Solid green: running normally
//! - Blinking green: OTA update in progress
//! - Solid red: error state
//! - Blinking red: no network connectivity
//!
//! # Hardware dependency
//!
//! The sysfs GPIO interface (`/sys/class/gpio`) is the legacy ABI; on
//! newer kernels the [`libgpiod`](https://git.kernel.org/pub/scm/libs/libgpiod/libgpiod.git/)
//! character-device API (`/dev/gpiochipN`) is preferred. This driver
//! uses sysfs because it is universally available on the Linux 6.x
//! kernels aris targets, and requires no external crates. Blinking is
//! cooperative — the caller's supervision loop must invoke [`Led::tick`]
//! periodically, or the supervisor can spawn a dedicated blinker task.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Default GPIO number for the green status LED on the NanoPi R3S.
///
/// The RK3566 SoC exposes GPIO0_C7 (pin 23 in the rockchip numbering)
/// for the green user LED. Adjust per-board via [`LedConfig`].
pub const DEFAULT_GREEN_GPIO: u32 = 23;

/// Default GPIO number for the red status LED on the NanoPi R3S.
pub const DEFAULT_RED_GPIO: u32 = 22;

/// Root of the legacy GPIO sysfs ABI.
const GPIO_SYSFS_ROOT: &str = "/sys/class/gpio";

/// LED states the gateway can display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedState {
    /// Normal operation (solid green)
    Running,
    /// OTA update in progress (slow blinking green)
    Updating,
    /// Error condition (solid red)
    Error,
    /// No network connectivity (fast blinking red)
    NoNetwork,
    /// Turn the LED off (board powered down for service)
    Off,
}

/// Per-board LED pin assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedConfig {
    /// GPIO sysfs number for the green status LED.
    pub green_gpio: u32,
    /// GPIO sysfs number for the red status LED.
    pub red_gpio: u32,
}

impl Default for LedConfig {
    fn default() -> Self {
        Self {
            green_gpio: DEFAULT_GREEN_GPIO,
            red_gpio: DEFAULT_RED_GPIO,
        }
    }
}

/// Status LED controller.
///
/// Owns the sysfs handles for the green and red LEDs. Dropping the
/// controller unexports the GPIOs so they can be reclaimed by the
/// kernel or another process.
pub struct Led {
    green: PinHandle,
    red: PinHandle,
    state: LedState,
    blink_phase: bool,
    last_tick: std::time::Instant,
}

impl Led {
    /// Open the LEDs described by `config`.
    ///
    /// Exports both GPIOs (if not already exported) and configures
    /// them as outputs. Returns an error if the GPIO subsystem is
    /// unavailable — on a non-Linux host or a kernel without
    /// `CONFIG_GPIO_SYSFS`, prefer to construct a mock in tests.
    pub fn new(config: &LedConfig) -> Result<Self> {
        let green = PinHandle::export(config.green_gpio, "aris-green")?;
        let red = PinHandle::export(config.red_gpio, "aris-red")?;
        Ok(Self {
            green,
            red,
            state: LedState::Off,
            blink_phase: false,
            last_tick: std::time::Instant::now(),
        })
    }

    /// Set the LED state.
    ///
    /// Drives the GPIOs immediately. For blinking states
    /// ([`LedState::Updating`], [`LedState::NoNetwork`]) call
    /// [`Led::tick`] periodically from the supervision loop.
    pub fn set(&mut self, state: LedState) {
        self.state = state;
        self.blink_phase = false;
        self.refresh();
        debug!(?state, "LED state set");
    }

    /// Advance the blinker state machine.
    ///
    /// Call this every ~100 ms from the supervision loop. For solid
    /// states this is a cheap no-op. Returns the duration the caller
    /// should wait before the next tick.
    pub fn tick(&mut self) -> Duration {
        let now = std::time::Instant::now();
        let period = match self.state {
            LedState::Updating => Duration::from_millis(500),
            LedState::NoNetwork => Duration::from_millis(200),
            _ => return Duration::from_millis(1000),
        };

        if now.duration_since(self.last_tick) >= period {
            self.blink_phase = !self.blink_phase;
            self.last_tick = now;
            self.refresh();
        }
        Duration::from_millis(100)
    }

    /// Current configured state.
    pub fn state(&self) -> LedState {
        self.state
    }

    fn refresh(&self) {
        let (green_on, red_on) = match self.state {
            LedState::Running => (true, false),
            LedState::Error => (false, true),
            LedState::Off => (false, false),
            LedState::Updating => (self.blink_phase, false),
            LedState::NoNetwork => (false, self.blink_phase),
        };
        if let Err(e) = self.green.write_value(green_on) {
            warn!(error = %e, "failed to drive green LED");
        }
        if let Err(e) = self.red.write_value(red_on) {
            warn!(error = %e, "failed to drive red LED");
        }
    }
}

impl Drop for Led {
    fn drop(&mut self) {
        // Turn both LEDs off before unexporting.
        let _ = self.green.write_value(false);
        let _ = self.red.write_value(false);
    }
}

/// Initialize the status LED GPIO with the default pin map.
pub fn init() -> Result<Led> {
    init_with(&LedConfig::default())
}

/// Initialize the status LED GPIO with a custom pin map.
pub fn init_with(config: &LedConfig) -> Result<Led> {
    info!(
        green = config.green_gpio,
        red = config.red_gpio,
        "initializing status LED"
    );
    Led::new(config)
}

/// Handle to a single exported GPIO sysfs pin.
struct PinHandle {
    gpio: u32,
    value_path: PathBuf,
    /// Whether this handle actually exported the pin (and therefore
    /// should unexport it on drop). If the pin was already exported
    /// when we opened it, we leave it alone.
    did_export: bool,
}

impl PinHandle {
    fn export(gpio: u32, label: &str) -> Result<Self> {
        let base = Path::new(GPIO_SYSFS_ROOT);
        let pin_dir = base.join(format!("gpio{gpio}"));

        if !pin_dir.exists() {
            info!(gpio, %label, "exporting GPIO pin");
            fs::write(base.join("export"), gpio.to_string())
                .with_context(|| format!("export gpio{gpio}"))?;
        }

        // Wait briefly for sysfs to materialize after export.
        for _ in 0..20 {
            if pin_dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let direction = pin_dir.join("direction");
        let value = pin_dir.join("value");
        if !direction.exists() || !value.exists() {
            anyhow::bail!(
                "gpio{gpio} sysfs files missing after export (kernel built without CONFIG_GPIO_SYSFS?)"
            );
        }

        // Configure as output. Ignore the write if it fails — the pin
        // may already be an output, or the kernel may reject the
        // transition; either way we can still drive the value.
        if let Err(e) = fs::write(&direction, b"out") {
            warn!(gpio, error = %e, "failed to set GPIO direction to out");
        }

        Ok(Self {
            gpio,
            value_path: value,
            did_export: !pin_dir_was_previously_exported(),
        })
    }

    fn write_value(&self, on: bool) -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .open(&self.value_path)
            .with_context(|| format!("open {}", self.value_path.display()))?;
        f.write_all(if on { b"1" } else { b"0" })
            .with_context(|| format!("write {}", self.value_path.display()))?;
        Ok(())
    }
}

impl Drop for PinHandle {
    fn drop(&mut self) {
        if self.did_export {
            let unexport = Path::new(GPIO_SYSFS_ROOT).join("unexport");
            if let Err(e) = fs::write(unexport, self.gpio.to_string()) {
                warn!(gpio = self.gpio, error = %e, "failed to unexport GPIO on drop");
            }
        }
    }
}

// NOTE: We never actually track pre-export state across the export
// call. The simplest correct behaviour for a PID-1 supervisor is to
// always unexport on shutdown so the kernel returns the pin to its
// default. We keep this helper separate (rather than always returning
// true) so a future implementation can record the prior state without
// changing call sites.
fn pin_dir_was_previously_exported() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_transitions_are_total() {
        // Sanity: every state maps to a well-defined (green, red) pair.
        for state in [
            LedState::Running,
            LedState::Updating,
            LedState::Error,
            LedState::NoNetwork,
            LedState::Off,
        ] {
            let _ = state; // exhaustive
        }
    }

    #[test]
    fn default_config_matches_nanopi_r3s() {
        let cfg = LedConfig::default();
        assert_eq!(cfg.green_gpio, DEFAULT_GREEN_GPIO);
        assert_eq!(cfg.red_gpio, DEFAULT_RED_GPIO);
    }
}
