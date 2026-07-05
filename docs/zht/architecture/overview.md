# aris 系統架構

## 概覽

aris 是面向工業物聯網閘道的模組化嵌入式作業系統，執行 Entelecheia 生態系統。
它透過一個最小化、安全的核心層，將 evernight 協定代理橋接到實體硬體。

## 架構分層

```mermaid
flowchart TB
    subgraph App["應用層"]
        Evn["evernight\n協定代理\nModbus RTU/TCP · S7comm · EtherCAT\nCAN 匯流排 · OPC UA"]
        Core["aris-core（監控器）\n看門狗 · OTA 升級\n網路設定 · 佈建"]
        Evn ---|Unix socket / IPC| Core
    end
    subgraph Krn["核心層"]
        K1["第一階段：Linux 6.x\n(aarch64 / armv7 / riscv64)"]
        K2["第二階段：Asterinas 框架核心\n(Rust, MPL-2.0)"]
        Drv["驅動：stmmac · dw_wdt · 8250_dw\ngpio-rockchip · spi-rockchip\ni2c-rk3x · dw_mmc · phy-rockchip"]
    end
    subgraph HW["硬體層"]
        Soc["RK3566 / BCM2711 / JH7110 / Intel N100"]
        Iface["雙吉位乙太網路 · GPIO · SPI · I2C\nUART · RS-485 · CAN"]
    end
    App --> Krn
    Krn --> HW
```

## 啟動流程

```mermaid
flowchart TB
    PWR["上電"] --> ROM["遮罩 ROM"]
    ROM --> SPL["U-Boot SPL"]
    SPL --> ATF["ATF (BL31)"]
    ATF --> UBT["U-Boot 主程式"]
    UBT -->|載入核心 + DTB + initramfs| KRN["Linux 核心"]
    KRN -->|執行 /init| INIT["aris-core (PID 1)"]
    INIT --> ETH["設定 eth0 (WAN) + eth1 (LAN)"]
    INIT --> MNT["掛載持久分割區 (/data)"]
    INIT --> ID["檢查 / 佈建裝置身分"]
    INIT --> EVN["生成 evernight 常駐程序"]
    EVN --> CFG["載入 /etc/evernight/config.toml"]
    EVN --> REG["連線到 entelecheia (WebSocket)"]
    EVN --> LSN["啟動協定監聽器\nModbus :502 · S7comm :102 · OPC UA :4840"]
    INIT --> SUP["監控迴圈\n餵看門狗 · 健康檢查 evernight · 處理 OTA"]
```

## 分割區佈局（A/B 更新）

| 偏移量 | 大小 | 分割區 | 內容 |
|--------|------|-----------|----------|
| 0 | 32 KiB | (間隙) | idbloader.img |
| 32 KiB | 8 MiB | (間隙) | u-boot.itb |
| 8 MiB | 128 MiB | boot-A | Image + DTB + boot.scr |
| 136 MiB | 128 MiB | boot-B | Image + DTB + boot.scr（備用） |
| 264 MiB | 512 MiB | rootfs-A | squashfs（唯讀） |
| 776 MiB | 512 MiB | rootfs-B | squashfs（唯讀，備用） |
| 1288 MiB | - | persistent | ext4（讀寫，/data） |

## 網路拓撲

```mermaid
flowchart TB
    NET["網際網路 / 企業區網"] --> ETH0
    subgraph GW["aris 閘道"]
        ETH0["eth0 — WAN (DHCP)"]
        ETH1["eth1 — LAN (192.168.42.1/24)"]
    end
    ETH1 --> PLC["PLC\n192.168.42.5"]
    ETH1 --> SEN["感測器\n192.168.42.10"]
    ETH1 --> HMI["HMI\n192.168.42.20"]
```

## Asterinas ARM64 策略（第二階段）

ARM64 Asterinas 的主要上游來源：

- **Fork**：https://github.com/wanywhn/asterinas（分支：`arm64-support`）
- **PR**：asterinas/asterinas#3270
- **狀態**：幾乎已準備好合併；包含面向 aarch64 的 GICv3、ARM GIC、
  基本裝置樹、MMU 設定和 UART 控制台

一旦合併到 Asterinas 主線，aris 將追蹤官方儲存庫。在此之前，
`arm64-support` 分支作為開發基線。
