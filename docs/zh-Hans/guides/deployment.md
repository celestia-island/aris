# aris 部署指南

## 概述

本指南涵盖将 aris 固件部署到物理硬件的全过程——从工厂预置到现场安装和持续
维护。

## 硬件组装

### NanoPi R3S

对于参考开发板（NanoPi R3S），您需要：

1. **NanoPi R3S 开发板**（RK3566，2GB RAM）
2. **microSD 卡**（≥ 8 GB，推荐 UHS-I）
3. **USB-C 电源适配器**（5V / 3A）
4. **USB-TTL 串口适配器**（3.3V 逻辑电平，CP2102 或 FTDI）
5. **以太网线**（WAN + LAN 各一根）
6. **外壳**（可选，推荐 DIN 导轨安装）

```mermaid
flowchart LR
    PWR["5V USB-C Power"] --> Board["NanoPi R3S"]
    SD["microSD\n(≥ 8 GB)"] --> Board
    TTL["USB-TTL\n(CP2102/FTDI)"] -->|GND/TX/RX| Board
    WAN["eth0 — WAN\n(upstream network)"] --> Board
    Board --> LAN["eth1 — LAN\n(192.168.42.0/24)"]
    LAN --> PLC["PLC / 传感器 / HMI"]
```

### 接线参考

| 开发板引脚 | USB-TTL 适配器 | 备注 |
|-------------|-----------------|-------|
| Pin 1 (GND) | GND | 共地 |
| Pin 2 (TX) | RX | 开发板发送 → 适配器接收 |
| Pin 3 (RX) | TX | 开发板接收 ← 适配器发送 |

调试串口波特率为 **1500000 baud，8N1**。大多数终端模拟器（`picocom`、
`minicom`、`screen`）支持此波特率。

## 工厂预置

新设备预置遵循以下步骤：

```mermaid
flowchart TB
    FLASH["烧录固件\n(just flash-sd)"] --> FIRST["首次启动\n串口控制台"]
    FIRST --> ID["生成设备身份\n(/data/device.toml)"]
    ID --> NET["配置网络\n(WAN DHCP + LAN 静态)"]
    NET --> REG["注册到 entelecheia\n(WebSocket 握手)"]
    REG --> CFG["应用设备特定\n配置模板"]
    CFG --> DONE["设备就绪\n可进行现场部署"]
```

### 设备身份

每台 aris 设备都有存储在 `/data/device.toml` 中的唯一身份：

```toml
[device]
node_id = "aris-nanopi-r3s-001"
hardware = "nanopi-r3s"
serial = "RK3566-SN-XXXXXXXX"

[entitlecheia]
endpoint = "wss://entelecheia.example.com/ws"
psk = "/data/keys/device.psk"
```

身份在首次启动时生成并持久化到可写持久分区。预共享密钥（`device.psk`）
用于与 entelecheia 的会话生命周期进行身份验证。

## 网络拓扑

典型的现场部署如下：

```mermaid
flowchart TB
    subgraph Plant["工厂网络"]
        CORP["企业局域网 / 互联网"]
        GW["aris 网关\n(DIN 导轨安装)"]
        CORP -->|eth0 — WAN| GW
    end
    subgraph Field["现场网络 (eth1)"]
        GW -->|192.168.42.0/24| PLC["PLC\n192.168.42.5"]
        GW -->|Modbus TCP :502| SENS["温度传感器\n192.168.42.10"]
        GW -->|OPC UA :4840| HMI["HMI 面板\n192.168.42.20"]
    end
    GW -->|"TLS WebSocket\n(wss://)"| ENT["entelecheia 云端"]
```

- **eth0 (WAN)**：连接到上游企业网络或直接连接互联网。默认使用 DHCP；
  可通过 `/data/network.toml` 配置静态 IP。
- **eth1 (LAN)**：为本地现场总线网络提供服务，地址为 `192.168.42.0/24`。
  PLC、传感器和 HMI 在此连接。

## OTA 更新

aris 支持 A/B 双槽位更新，实现安全、可回滚的固件升级：

```mermaid
flowchart TB
    CURRENT["当前槽位\nSlot A (活跃)"] -->|"entelecheia\n推送更新"| DL["下载新镜像\n到备用槽位"]
    DL --> VERIFY["验证镜像\n校验和 + 签名"]
    VERIFY -->|无效| ABORT["中止 — 当前槽位\n不受影响"]
    VERIFY -->|有效| SWITCH["切换启动槽位\n(uboot env)"]
    SWITCH --> REBOOT["重启"]
    REBOOT --> NEW["Slot B (活跃)"]
    NEW -->|"健康检查\n通过 (5 分钟)"| COMMIT["提交 — 新槽位\n成为永久"]
    NEW -->|"健康检查\n失败"| ROLLBACK["回滚 —\n启动之前的槽位"]
```

分区布局支持 `boot` 和 `rootfs` 的 A/B 双份：

| 槽位 | boot 分区 | rootfs 分区 | 状态 |
|------|---------------|-----------------|--------|
| A | `boot-A` (128 MiB) | `rootfs-A` (512 MiB) | 主 |
| B | `boot-B` (128 MiB) | `rootfs-B` (512 MiB) | 备用 |

## 现场部署检查清单

将设备部署到物理现场之前，请验证：

1. **硬件**：所有线缆已插好，电源充足，外壳已密封
2. **存储**：SD 卡已正确插入，未启用写保护开关
3. **网络**：eth0 和 eth1 均已连接到正确的网络
4. **串口**：USB-TTL 可用以进行紧急控制台访问
5. **启动**：上电，通过串口控制台监控启动消息
6. **服务**：`aris-core`（PID 1）和 `evernight` 守护进程正在运行
7. **注册**：设备出现在 entelecheia 仪表板中
8. **协议**：Modbus/S7comm/OPC UA 监听器可从现场设备访问
9. **OTA**：测试一个虚拟 OTA 更新以验证分区布局
10. **看门狗**：通过终止 `aris-core` 测试看门狗 — 设备应重新启动

```bash
# Verify services on the device (via SSH or serial)
ps aux | grep aris-core
ps aux | grep evernight

# Check network interfaces
ip addr show eth0
ip addr show eth1

# Check partition layout
cat /proc/partitions

# Check boot slot
fw_printenv boot_slot

# Trigger manual health check
aris-core --health-check
```

## 监控

部署后，请监控以下指标：

| 指标 | 数据源 | 告警阈值 |
|--------|--------|----------------|
| CPU 温度 | `/sys/class/thermal/thermal_zone0/temp` | > 80°C |
| 内存使用率 | `/proc/meminfo` | > 90% |
| 存储磨损 | `/data/wear_level.txt` | > 80% rated cycles |
| 网络链路 | `ethtool eth0` / `ethtool eth1` | Link down |
| evernight 状态 | `systemctl status evernight` | Not running |
| entelecheia 连接 | `/var/log/evernight.log` | Disconnected > 60s |

所有指标通过 evernight 协议代理上报到 entelecheia。告警显示在 entelecheia
仪表板中，并可触发自动响应（重启、故障切换、派遣技术人员）。
