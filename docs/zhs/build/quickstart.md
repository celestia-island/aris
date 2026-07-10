# 快速开始 — 从源码到 SD 卡

## 前置条件

- Linux x86_64 或 ARM64 主机
- Rust 1.85+（通过 `rustup`）
- `just` 命令运行器（`cargo install just`）
- SD 卡读卡器 + microSD（≥ 8 GB）

## 1. 克隆

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. 设置交叉编译

```bash
just setup-cross
```

此命令会安装 Rust 目标（`aarch64-unknown-linux-musl` 等），并打印适用于您发行版的 GCC 工具链说明。

## 3. 构建固件

```bash
just build-board nanopi-r3s
```

生成 `output/nanopi-r3s/image.img`。

## 4. 烧录到 SD 卡

```bash
just flash-sd /dev/sdX
```

将 `/dev/sdX` 替换为您的 SD 卡设备（用 `lsblk` 查看）。

## 5. 启动

将 SD 卡插入 NanoPi R3S，连接 5V USB-C 电源。

- **串口控制台**：将 USB-TTL 连接到 3 针调试排针（GND/TX/RX），1500000 波特率，8N1
- **SSH**：启动后，`ssh root@<ip>`（从 WAN eth0 通过 DHCP 获取）

## 6. 验证

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
