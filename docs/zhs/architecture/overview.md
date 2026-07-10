# aris 系统架构

## 概览

aris 是面向工业物联网网关的模块化嵌入式操作系统，运行 Entelecheia 生态系统。
它通过一个最小化、安全的内核层，将 evernight 协议代理桥接到物理硬件。

## 架构分层

```mermaid
flowchart TB
    subgraph App["应用层"]
        Evn["evernight\n协议代理\nModbus RTU/TCP · S7comm · EtherCAT\nCAN 总线 · OPC UA"]
        Core["aris-core（监控器）\n看门狗 · OTA 升级\n网络配置 · 配网"]
        Evn ---|Unix socket / IPC| Core
    end
    subgraph Krn["内核层"]
        K1["第一阶段：Linux 6.x\n(aarch64 / armv7 / riscv64)"]
        K2["第二阶段：Asterinas 框架内核\n(Rust, MPL-2.0)"]
        Drv["驱动：stmmac · dw_wdt · 8250_dw\ngpio-rockchip · spi-rockchip\ni2c-rk3x · dw_mmc · phy-rockchip"]
    end
    subgraph HW["硬件层"]
        Soc["RK3566 / BCM2711 / JH7110 / Intel N100"]
        Iface["双千兆以太网 · GPIO · SPI · I2C\nUART · RS-485 · CAN"]
    end
    App --> Krn
    Krn --> HW
```

## 启动流程

```mermaid
flowchart TB
    PWR["上电"] --> ROM["掩模 ROM"]
    ROM --> SPL["U-Boot SPL"]
    SPL --> ATF["ATF (BL31)"]
    ATF --> UBT["U-Boot 主程序"]
    UBT -->|加载内核 + DTB + initramfs| KRN["Linux 内核"]
    KRN -->|执行 /init| INIT["aris-core (PID 1)"]
    INIT --> ETH["配置 eth0 (WAN) + eth1 (LAN)"]
    INIT --> MNT["挂载持久分区 (/data)"]
    INIT --> ID["检查 / 配置设备身份"]
    INIT --> EVN["生成 evernight 守护进程"]
    EVN --> CFG["加载 /etc/evernight/config.toml"]
    EVN --> REG["连接到 entelecheia (WebSocket)"]
    EVN --> LSN["启动协议监听器\nModbus :502 · S7comm :102 · OPC UA :4840"]
    INIT --> SUP["监控循环\n喂看门狗 · 健康检查 evernight · 处理 OTA"]
```

## 分区布局（A/B 更新）

| 偏移量 | 大小 | 分区 | 内容 |
|--------|------|-----------|----------|
| 0 | 32 KiB | (间隙) | idbloader.img |
| 32 KiB | 8 MiB | (间隙) | u-boot.itb |
| 8 MiB | 128 MiB | boot-A | Image + DTB + boot.scr |
| 136 MiB | 128 MiB | boot-B | Image + DTB + boot.scr（备用） |
| 264 MiB | 512 MiB | rootfs-A | squashfs（只读） |
| 776 MiB | 512 MiB | rootfs-B | squashfs（只读，备用） |
| 1288 MiB | - | persistent | ext4（读写，/data） |

## 网络拓扑

```mermaid
flowchart TB
    NET["互联网 / 企业局域网"] --> ETH0
    subgraph GW["aris 网关"]
        ETH0["eth0 — WAN (DHCP)"]
        ETH1["eth1 — LAN (192.168.42.1/24)"]
    end
    ETH1 --> PLC["PLC\n192.168.42.5"]
    ETH1 --> SEN["传感器\n192.168.42.10"]
    ETH1 --> HMI["HMI\n192.168.42.20"]
```

## Asterinas ARM64 策略（第二阶段）

ARM64 Asterinas 的主要上游来源：

- **Fork**：https://github.com/wanywhn/asterinas（分支：`arm64-support`）
- **PR**：asterinas/asterinas#3270
- **状态**：几乎已准备好合并；包含面向 aarch64 的 GICv3、ARM GIC、
  基本设备树、MMU 设置和 UART 控制台

一旦合并到 Asterinas 主线，aris 将跟踪官方仓库。在此之前，
`arm64-support` 分支作为开发基线。
