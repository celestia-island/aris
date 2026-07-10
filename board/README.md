# NanoPi R3S — Board support directory

This directory contains board-specific files for the NanoPi R3S:

- `defconfig` — Buildroot-style full board definition (composite config)
- `linux.config` — Linux kernel .config fragment
- `uboot.config` — U-Boot .config fragment
- `genimage.cfg` — SD card image partition layout
- `boot.cmd` — U-Boot boot script source (compiled to boot.scr)
- `device-tree/` — Device tree source files (.dts)

## Hardware Specs

| Component    | Specification                          |
|--------------|----------------------------------------|
| SoC          | Rockchip RK3566                        |
| CPU          | Quad-core Cortex-A55 @ 1.8 GHz         |
| NPU          | 1 TOPS                                 |
| RAM          | 2 GB LPDDR4                            |
| Storage      | MicroSD + optional eMMC                |
| Ethernet     | 2x Gigabit (RTL8211F)                  |
| USB          | 1x USB 3.0 Type-A (host) + 1x USB-C (DRD) |
| UART         | 3-pin debug header (3.3V TTL)          |
| GPIO         | 40-pin header (SPI, I2C, UART, PWM)    |
| Power        | 5V/3A via USB-C                        |

## USB-C Gadget Mode

The USB-C port (wired to the RK3566's DWC3 DRD controller) supports
**dual-role operation** — it can act as a USB host (for flashing) or
as a **USB device/gadget** (the normal runtime mode).

When configured as a composite gadget, the board presents two functions
to the connected host (PC, phone, tablet):

```
  ┌──────────────────────┐         ┌──────────────────┐
  │  Host PC / Phone     │◄──USB-C──┤  NanoPi R3S      │
  │                      │         │  (gadget mode)   │
  │  Sees:               │         │                  │
  │   • USB Drive (FAT)  │         │   mass_storage   │
  │     with installers  │         │   + NCM Ethernet │
  │   • NCM Net Adapter  │         │                  │
  └──────────────────────┘         └──────────────────┘
```

### What the host sees

1. **Mass Storage** — A 32 MB FAT32 drive labeled `ARIS_GW` containing:
   - `autorun.inf` + `windows/install_evernight.bat` — Windows auto-installer
   - `linux/install_evernight.sh` — Linux installer
   - `macos/install_evernight.command` — macOS installer
   - `android/install_evernight.txt` — Android instructions
   - `common/README.txt` — Quick start guide
   - `common/evernight-{windows,linux,darwin}-{amd64,arm64}` — Cross-compiled binaries

2. **CDC-NCM** — A virtual Ethernet adapter. The gateway assigns itself
   `10.0.99.1/24` and the host gets `10.0.99.100`. This provides a direct
   IP link for the dashboard at `http://10.0.99.1:8080`.

### Runtime management

| Action              | Command                                   |
|---------------------|-------------------------------------------|
| Start gadget        | `/usr/sbin/aris-usb-gadget start`         |
| Stop gadget         | `/usr/sbin/aris-usb-gadget stop`          |
| Check status        | `/usr/sbin/aris-usb-gadget status`        |
| Rebuild installer   | `just build-installer-image`              |
| Manual mode switch  | Write `host`/`peripheral` to `/sys/.../mode` |

### Device tree configuration

The DTS enables the DWC3 controller in OTG mode:

```dts
&usbdrd_dwc3_0 {
    dr_mode = "otg";           /* auto-switch host/peripheral */
    /* dr_mode = "peripheral"; */ /* gadget-only (no host mode) */
    status = "okay";
};
```

For production deployments where the USB-C port is exclusively used
for gadget mode (never as a host), set `dr_mode = "peripheral"` to
skip the host controller initialization.
