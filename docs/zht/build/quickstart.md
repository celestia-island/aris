# 快速開始 — 從原始碼到 SD 卡

## 前置條件

- Linux x86_64 或 ARM64 主機
- Rust 1.85+（透過 `rustup`）
- `just` 命令執行器（`cargo install just`）
- SD 卡讀卡機 + microSD（≥ 8 GB）

## 1. 複製

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. 設定交叉編譯

```bash
just setup-cross
```

此命令會安裝 Rust 目標（`aarch64-unknown-linux-musl` 等），並列印適用於您發行版的 GCC 工具鏈說明。

## 3. 建置韌體

```bash
just build-board nanopi-r3s
```

產生 `output/nanopi-r3s/image.img`。

## 4. 燒錄到 SD 卡

```bash
just flash-sd /dev/sdX
```

將 `/dev/sdX` 替換為您的 SD 卡裝置（用 `lsblk` 查看）。

## 5. 啟動

將 SD 卡插入 NanoPi R3S，連接 5V USB-C 電源。

- **序列控制台**：將 USB-TTL 連接到 3 針除錯排針（GND/TX/RX），1500000 鮑率，8N1
- **SSH**：啟動後，`ssh root@<ip>`（從 WAN eth0 透過 DHCP 取得）

## 6. 驗證

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
