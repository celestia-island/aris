# aris システムアーキテクチャ

## 概要

aris は Entelecheia エコシステムを実行する、産業用 IoT ゲートウェイ向けの
モジュラー組み込み OS です。最小限で安全なカーネル層を通じて、evernight
プロトコルブローカーを物理ハードウェアへ橋渡しします。

## アーキテクチャ層

```mermaid
flowchart TB
    subgraph App["アプリケーション層"]
        Evn["evernight\nプロトコルブローカー\nModbus RTU/TCP · S7comm · EtherCAT\nCAN バス · OPC UA"]
        Core["aris-core（スーパーバイザー）\nウォッチドッグ · OTA 更新\nネットワーク設定 · プロビジョニング"]
        Evn ---|Unix socket / IPC| Core
    end
    subgraph Krn["カーネル層"]
        K1["第 1 段階：Linux 6.x\n(aarch64 / armv7 / riscv64)"]
        K2["第 2 段階：Asterinas フレームカーネル\n(Rust, MPL-2.0)"]
        Drv["ドライバー：stmmac · dw_wdt · 8250_dw\ngpio-rockchip · spi-rockchip\ni2c-rk3x · dw_mmc · phy-rockchip"]
    end
    subgraph HW["ハードウェア層"]
        Soc["RK3566 / BCM2711 / JH7110 / Intel N100"]
        Iface["デュアル GbE · GPIO · SPI · I2C\nUART · RS-485 · CAN"]
    end
    App --> Krn
    Krn --> HW
```

## ブートフロー

```mermaid
flowchart TB
    PWR["電源オン"] --> ROM["マスク ROM"]
    ROM --> SPL["U-Boot SPL"]
    SPL --> ATF["ATF (BL31)"]
    ATF --> UBT["U-Boot 本体"]
    UBT -->|カーネル + DTB + initramfs をロード| KRN["Linux カーネル"]
    KRN -->|/init を実行| INIT["aris-core (PID 1)"]
    INIT --> ETH["eth0 (WAN) + eth1 (LAN) を設定"]
    INIT --> MNT["永続パーティション (/data) をマウント"]
    INIT --> ID["デバイス ID の確認 / プロビジョニング"]
    INIT --> EVN["evernight デーモンを生成"]
    EVN --> CFG["/etc/evernight/config.toml をロード"]
    EVN --> REG["entelecheia に接続 (WebSocket)"]
    EVN --> LSN["プロトコルリスナーを起動\nModbus :502 · S7comm :102 · OPC UA :4840"]
    INIT --> SUP["スーパービジョンループ\nウォッチドッグへ給餌 · evernight のヘルスチェック · OTA 処理"]
```

## パーティションレイアウト（A/B 更新）

| オフセット | サイズ | パーティション | 内容 |
|--------|------|-----------|----------|
| 0 | 32 KiB | (ギャップ) | idbloader.img |
| 32 KiB | 8 MiB | (ギャップ) | u-boot.itb |
| 8 MiB | 128 MiB | boot-A | Image + DTB + boot.scr |
| 136 MiB | 128 MiB | boot-B | Image + DTB + boot.scr（待機） |
| 264 MiB | 512 MiB | rootfs-A | squashfs（読み取り専用） |
| 776 MiB | 512 MiB | rootfs-B | squashfs（読み取り専用、待機） |
| 1288 MiB | - | persistent | ext4（読み書き、/data） |

## ネットワークトポロジー

```mermaid
flowchart TB
    NET["インターネット / 企業 LAN"] --> ETH0
    subgraph GW["aris ゲートウェイ"]
        ETH0["eth0 — WAN (DHCP)"]
        ETH1["eth1 — LAN (192.168.42.1/24)"]
    end
    ETH1 --> PLC["PLC\n192.168.42.5"]
    ETH1 --> SEN["センサー\n192.168.42.10"]
    ETH1 --> HMI["HMI\n192.168.42.20"]
```

## Asterinas ARM64 戦略（第 2 段階）

ARM64 向け Asterinas の主なアップストリームソース：

- **Fork**：https://github.com/wanywhn/asterinas（ブランチ：`arm64-support`）
- **PR**：asterinas/asterinas#3270
- **状態**：マージ準備ほぼ完了。aarch64 向けの GICv3、ARM GIC、
  基本的なデバイスツリー、MMU セットアップ、UART コンソールを含む

Asterinas 本線へマージされ次第、aris は公式リポジトリを追跡します。
それまでは `arm64-support` ブランチが開発ベースラインとなります。
