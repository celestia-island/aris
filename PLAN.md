# aris — 项目状态与计划 (PLAN)

> 本文件于 **2026-07-04** 更新，记录项目当前状态、近期进展与后续计划。
> 原有详细计划已保留于文末「既有详细计划（存档）」。

## 1. 项目概述

- **名称**：`aris`
- **简介**：兼容 Linux 标准的发行版，附带为 evernight 与 shittim-chest 定制的桌面环境，对标工业 HMI 与上位机。
- **远程仓库**：本地仓库（无 origin）
- **技术栈**：Rust / just
- **类别**：firmware

## 2. 当前状态

- **当前分支**：`dev`
- **工作区**：干净
- **最近提交时间**：2026-07-04
- **最近提交**：test: cross-compile evernight fixture binaries for multi-platform installer tests

## 3. 未提交改动

无。

## 4. 近期进展

### kei 内核完整启动（2026-07-04）🎉

**kei Asterinas 内核在 QEMU arm64 上完整启动并加载用户空间 ELF 进程。**

- 修复 FDT 内存区域溢出 bug：`max_paddr` 从 128TB 降至正确的 3GB
- 修复 vbe_dispi x86 模块在 aarch64 上的编译错误
- 修复 initramfs 使用错误架构的 busybox
- 内核完整初始化：GIC、timer、SMP、page tables、net、fs、sched、process
- initramfs 解包 → rootfs ready
- **用户空间 init 进程成功加载**（`init=/init`）

### evernight 联调（2026-07-04）

宿主机点火测试（`just ignition-test`）全链路打通，**双向验证通过**：

```
Modbus TCP sim (:5020)
  → evernight sensor-poll (读取 holding registers)
  → WebSocket ws://127.0.0.1:8443/api/ws
  → evernight-server (device.register + device.telemetry)
```

**验证结果（sensor-poll 端）**：
- `Device registered on server node_id=ignition-test-01` ✅
- `Telemetry sent to gateway`（每 2 s 循环）✅

**验证结果（evernight-server 端）**：
- `Device registered node_id=ignition-test-01 stations=1` ✅
- `Telemetry received node_id=ignition-test-01`（持续接收）✅
- `Device unregistered`（断连时正常清理）✅

**发现并修复的问题**：
1. sensor-poll 默认数据目录 `/var/lib/evernight/sensor` 非 root 不可写 → 注入 `SENSOR_DATA_DIR` 环境变量
2. Modbus TCP 模拟器 MBAP 帧解析错误 → 重写为正确的 7-byte header + length-based framing
3. `EntelecheiaTriggerSink` Unix socket 转发失败为非致命（仅 WARN），不影响 gateway 遥测路径

### 核心驱动实现（2026-07-04）
- `led.rs`：GPIO LED 控制（sysfs /sys/class/gpio）
- `watchdog.rs`：/dev/watchdog ioctl WDIOC_KEEPALIVE 喂狗
- `net.rs`：网络接口 netlink 配置
- `ota.rs`：OTA 下载/dm-verity 校验/分区写入（已编译可用，真机验证前提已标注）
- 跨平台 everight fixture 二进制（aarch64/x86_64/Apple/Windows，纯 Rust 无 C 依赖）

### 既往提交

- docs: standardize License section format across all translations
- style: use uppercase ARIS / KEI throughout
- docs: add comprehensive deployment guides (guide files + mermaid)
- chore: stop tracking Cargo.lock
- feat: USB-C gadget support (composite mass-storage + NCM)
- feat: self-contained musl cross-build + fix binary paths
- feat: tri-backend QEMU ignition test (linux / kei / asterinas)

## 5. 后续计划

### 短期（本周）
1. **aarch64 交叉编译验证**——安装 `aarch64-unknown-linux-musl` target，构建 evernight gateway profile 二进制，替换 `tests/fixtures/` 中的 stub
2. **QEMU arm64 点火测试**——安装 `qemu-system-aarch64`，运行 `just qemu-ignition-linux`（Linux baseline）和 `just qemu-ignition-kei`（kei 内核）
3. **kei 内核联调**——在 QEMU virt (cortex-a55/a72) 上启动 kei，验证 initramfs → evernight 启动序列
4. 提交本轮 ignition_test.py 修复

### 中期
1. 推进 M1.3 evernight 交叉编译里程碑（gateway profile feature set）
2. 收敛 M2 ARM64 Hardening 遗留项（FDT 内存解析、GICv3 驱动）
3. 固化启动与健康检查流程（aris-core supervisor 生命周期管理）

### 长期
1. M1.5 OTA 双分区升级流程
2. M2.4 在 NanoPi R3S 上运行 kei + evernight 全栈

---

## 既有详细计划（存档）

# aris — Project Plan

## Goal

Build a Linux-standard (LSB-compatible) distribution that ships a desktop environment purpose-built for evernight and shittim-chest, targeting industrial HMI panels and supervisory host (上位机) stations.

## Architecture

```
┌────────────────────────────────────────────────┐
│ entelecheia  (Cloud/Edge AI Multi-Agent)        │
│   WebSocket JSON-RPC / Unix Socket             │
├────────────────────────────────────────────────┤
│ evernight  (Hardware Protocol Broker)           │
│   Modbus / S7comm / EtherCAT / OPC UA / CAN    │
├────────────────────────────────────────────────┤
│ aris OS  (Device Firmware Layer)                │
│   ├─ Kernel: Linux 6.x → Asterinas (Phase 2)   │
│   ├─ Init: aris-core supervisor                 │
│   ├─ Net: Dual Ethernet (WAN + LAN)             │
│   └─ OTA: A/B partition firmware update         │
├────────────────────────────────────────────────┤
│ Physical Devices  (PLC / Sensors / Valves)      │
└────────────────────────────────────────────────┘
```

## Phase 1: Linux Base (2026 Q3–Q4)

Target: boot, run evernight, talk to entelecheia.

### M1.1 — Board Bring-up
- Buildroot-style slim rootfs (musl + busybox)
- Linux 6.x kernel with RK3566 BSP
- U-Boot with verified boot
- Target board: NanoPi R3S (RK3566, dual GbE)

### M1.2 — Core Drivers
- [x] Dual Gigabit Ethernet (stmmac/rk_gmac) — WAN/LAN routing
- [ ] UART (debug + serial devices)
- [ ] GPIO (status LEDs, digital I/O)
- [ ] SPI (sensor bus)
- [ ] I2C (peripheral bus)
- [ ] eMMC/SD storage
- [ ] Hardware watchdog (RK3566 WDT)

### M1.3 — evernight Cross-compile
- Target: `aarch64-unknown-linux-musl`
- Features: `hardware, protocol, serial, sensor, s7comm, ethercat, can, bin, api, vault, manifest`
- Excluded: `screen, webrtc, remote-ssh, remote-vnc, remote-rdp, container, k8s, libvirt, vm, compile-bridge`

### M1.4 — Firmware Integration
- aris-core supervisor manages evernight daemon lifecycle
- Startup sequence: net init → evernight start → device.register → entelecheia join
- Health check + auto-restart via watchdog
- [x] Host ignition test verified (2026-07-04): evernight sensor-poll → WebSocket → evernight-server, device.register + device.telemetry 双向确认
- [ ] QEMU arm64 boot with evernight (pending QEMU install)
- [ ] aris-core supervisor lifecycle management

### M1.5 — OTA Update
- Dual A/B partition layout
- Firmware package: kernel + dtb + rootfs squashfs + verity hash
- Update flow: download → verify → set boot flag → reboot → fallback on failure

### M1.6 — Production Readiness
- Build reproducibility (deterministic image hash)
- Secure boot chain (U-Boot verified boot)
- Provisioning: unique device identity, TLS client cert
- Factory reset

## Phase 2: Asterinas ARM64 Port (2026 Q4+)

> **Key**: ARM64 support is already under active development.
> PR asterinas/asterinas#3270 by @wanywhn is nearly ready.
> We track the fork: https://github.com/wanywhn/asterinas (branch: `arm64-support`).

### M2.1 — Adopt ARM64 Fork
- Use `wanywhn/asterinas` `arm64-support` branch as development baseline
- Includes: GICv3, ARM MMU setup, UART console, basic device tree for aarch64
- Once merged into mainline, switch to official asterinas/asterinas
- Track PR #3270 status weekly

### M2.2 — RK3566 Board Support for Asterinas
Add board-specific drivers on top of the arm64-support base:
- Rockchip GPIO/pinctrl driver
- stmmac Ethernet driver (DW GMAC / RK GMAC)
- DW SPI / DW I2C master drivers
- UART 8250-compatible (DW UART) driver
- Device tree support (ostd dtb parsing)

### M2.3 — aris Asterinas Kernel Module
- `kernel/asterinas/` directory with cargo-osdk project
- Reuse Linux device tree bindings

### M2.4 — Parity Validation
- Boot Asterinas on NanoPi R3S
- Run evernight, verify all protocol features
- Performance benchmark vs Linux baseline

### M2.5 — Production Rollout
- OTA push Asterinas kernel to deployed devices
- Fallback to Linux kernel on boot failure

## Multi-Architecture Roadmap

| Arch | SoC Examples | Phase 1 | Phase 2 |
|------|-------------|---------|---------|
| aarch64 | RK3566, RK3588, BCM2711 | Now | Asterinas ARM64 |
| armv7l | BCM2837, AM335x, i.MX6 | Q4 2026 | Asterinas ARM32 (if upstream) |
| riscv64 | JH7110, TH1520, K230 | Q1 2027 | Asterinas (upstream Tier 2) |
| x86_64 | Intel N100, AMD G-Series | Q2 2027 | Asterinas (upstream Tier 1) |

## evernight Feature Flags per Target

### Gateway Profile (aarch64, headless, < 2GB RAM)
```
hardware, protocol, serial, sensor, s7comm, ethercat, can,
bin, api, vault, manifest, scripting
```

### Minimal Profile (armv7l, < 512MB RAM)
```
hardware, protocol, serial, sensor, bin, api, manifest
```

### Full Profile (x86_64, >= 4GB RAM)
```
full (all features)
```

## Board Support Matrix

| Board | SoC | Arch | RAM | Storage | Ethernet | Status |
|-------|-----|------|-----|---------|----------|--------|
| NanoPi R3S | RK3566 | aarch64 | 2GB | SD/eMMC | 2x GbE | Active |
| OrangePi 3B | RK3566 | aarch64 | 4GB | eMMC | 1x GbE | Planned |
| Raspberry Pi 4 | BCM2711 | aarch64 | 2GB | SD | 1x GbE | Planned |
| VisionFive 2 | JH7110 | riscv64 | 4GB | SD/eMMC | 2x GbE | Planned |
| Luckfox Pico | RV1103 | armv7l | 64MB | SPI NAND | 1x FE | Planned |

## Build System Design

Aris uses a custom build system (no Buildroot submodule):

```
scripts/build.sh              # Main build orchestrator
  ├── configs/<board>.toml    # Board-specific config
  ├── kernel/                 # Kernel source (downloaded)
  ├── board/<board>/          # Device tree, boot script
  ├── packages/core/          # Rust firmware (cross-compiled)
  └── overlay/<board>/        # Rootfs static files
→ output/<board>/image.img    # Bootable SD card image
```

## Key Design Decisions

1. **No git submodules** — build script downloads kernel/uboot toolchains on demand
2. **TOML-based board configs** — one config file per board, declarative
3. **A/B partition layout** — mandatory for all boards, safe OTA
4. **musl static linking** — single binary, no libc ABI issues
5. **Verified boot everywhere** — from U-Boot through kernel to rootfs

