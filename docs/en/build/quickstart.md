# Quick Start — From Source to SD Card

## Prerequisites

- Linux x86_64 or ARM64 host
- Rust 1.85+ (via `rustup`)
- `just` command runner (`cargo install just`)
- SD card reader + microSD (≥ 8 GB)

## 1. Clone

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. Setup cross-compilation

```bash
just setup-cross
```

This installs Rust targets (`aarch64-unknown-linux-musl` etc.) and prints GCC toolchain instructions for your distro.

## 3. Build firmware

```bash
just build-board nanopi-r3s
```

Produces `output/nanopi-r3s/image.img`.

## 4. Flash to SD card

```bash
just flash-sd /dev/sdX
```

Replace `/dev/sdX` with your SD card device (check with `lsblk`).

## 5. Boot

Insert the SD card into the NanoPi R3S, connect 5V USB-C power.

- **Serial console**: Connect USB-TTL to the 3-pin debug header (GND/TX/RX), 1500000 baud, 8N1
- **SSH**: After boot, `ssh root@<ip>` (DHCP from WAN eth0)

## 6. Verify

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
