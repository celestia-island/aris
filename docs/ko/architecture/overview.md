# aris 시스템 아키텍처

## 개요

aris는 Entelecheia 생태계를 실행하는 산업용 IoT 게이트웨이용 모듈형 임베디드 OS입니다.
최소화되고 안전한 커널 계층을 통해 evernight 프로토콜 브로커를 물리 하드웨어로
브리지합니다.

## 아키텍처 계층

```mermaid
flowchart TB
    subgraph App["애플리케이션 계층"]
        Evn["evernight\n프로토콜 브로커\nModbus RTU/TCP · S7comm · EtherCAT\nCAN 버스 · OPC UA"]
        Core["aris-core (수퍼바이저)\n와치독 · OTA 업데이트\n네트워크 설정 · 프로비저닝"]
        Evn ---|Unix socket / IPC| Core
    end
    subgraph Krn["커널 계층"]
        K1["1단계: Linux 6.x\n(aarch64 / armv7 / riscv64)"]
        K2["2단계: Asterinas 프레임커널\n(Rust, MPL-2.0)"]
        Drv["드라이버: stmmac · dw_wdt · 8250_dw\ngpio-rockchip · spi-rockchip\ni2c-rk3x · dw_mmc · phy-rockchip"]
    end
    subgraph HW["하드웨어 계층"]
        Soc["RK3566 / BCM2711 / JH7110 / Intel N100"]
        Iface["듀얼 GbE · GPIO · SPI · I2C\nUART · RS-485 · CAN"]
    end
    App --> Krn
    Krn --> HW
```

## 부트 플로우

```mermaid
flowchart TB
    PWR["전원 인가"] --> ROM["마스크 ROM"]
    ROM --> SPL["U-Boot SPL"]
    SPL --> ATF["ATF (BL31)"]
    ATF --> UBT["U-Boot 본체"]
    UBT -->|커널 + DTB + initramfs 로드| KRN["Linux 커널"]
    KRN -->|/init 실행| INIT["aris-core (PID 1)"]
    INIT --> ETH["eth0 (WAN) + eth1 (LAN) 설정"]
    INIT --> MNT["영구 파티션 (/data) 마운트"]
    INIT --> ID["디바이스 ID 확인 / 프로비저닝"]
    INIT --> EVN["evernight 데몬 생성"]
    EVN --> CFG["/etc/evernight/config.toml 로드"]
    EVN --> REG["entelecheia에 연결 (WebSocket)"]
    EVN --> LSN["프로토콜 리스너 시작\nModbus :502 · S7comm :102 · OPC UA :4840"]
    INIT --> SUP["수퍼비전 루프\n와치독 급여 · evernight 헬스 체크 · OTA 처리"]
```

## 파티션 레이아웃 (A/B 업데이트)

| 오프셋 | 크기 | 파티션 | 내용 |
|--------|------|-----------|----------|
| 0 | 32 KiB | (갭) | idbloader.img |
| 32 KiB | 8 MiB | (갭) | u-boot.itb |
| 8 MiB | 128 MiB | boot-A | Image + DTB + boot.scr |
| 136 MiB | 128 MiB | boot-B | Image + DTB + boot.scr (대기) |
| 264 MiB | 512 MiB | rootfs-A | squashfs (읽기 전용) |
| 776 MiB | 512 MiB | rootfs-B | squashfs (읽기 전용, 대기) |
| 1288 MiB | - | persistent | ext4 (읽기/쓰기, /data) |

## 네트워크 토폴로지

```mermaid
flowchart TB
    NET["인터넷 / 기업 LAN"] --> ETH0
    subgraph GW["aris 게이트웨이"]
        ETH0["eth0 — WAN (DHCP)"]
        ETH1["eth1 — LAN (192.168.42.1/24)"]
    end
    ETH1 --> PLC["PLC\n192.168.42.5"]
    ETH1 --> SEN["센서\n192.168.42.10"]
    ETH1 --> HMI["HMI\n192.168.42.20"]
```

## Asterinas ARM64 전략 (2단계)

ARM64용 Asterinas의 주요 업스트림 소스:

- **Fork**: https://github.com/wanywhn/asterinas (브랜치: `arm64-support`)
- **PR**: asterinas/asterinas#3270
- **상태**: 병합 준비 거의 완료. aarch64용 GICv3, ARM GIC,
  기본 디바이스 트리, MMU 설정, UART 콘솔을 포함

Asterinas 메인라인에 병합되는 즉시 aris는 공식 리포지토리를 추적합니다.
그 전까지는 `arm64-support` 브랜치가 개발 베이스라인으로 사용됩니다.
