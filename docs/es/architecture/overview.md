# Arquitectura del sistema aris

## Visión general

aris es un SO embebido modular para pasarelas IoT industriales que ejecutan el
ecosistema Entelecheia. Conecta el broker de protocolos evernight al hardware
físico a través de una capa de núcleo mínima y segura.

## Capas de arquitectura

```mermaid
flowchart TB
    subgraph App["Capa de aplicación"]
        Evn["evernight\nBroker de protocolos\nModbus RTU/TCP · S7comm · EtherCAT\nBus CAN · OPC UA"]
        Core["aris-core (supervisor)\nWatchdog · Actualización OTA\nConfig de red · Aprovisionamiento"]
        Evn ---|Socket Unix / IPC| Core
    end
    subgraph Krn["Capa de núcleo"]
        K1["Fase 1: Linux 6.x\n(aarch64 / armv7 / riscv64)"]
        K2["Fase 2: Framekernel Asterinas\n(Rust, MPL-2.0)"]
        Drv["Controladores: stmmac · dw_wdt · 8250_dw\ngpio-rockchip · spi-rockchip\ni2c-rk3x · dw_mmc · phy-rockchip"]
    end
    subgraph HW["Capa de hardware"]
        Soc["RK3566 / BCM2711 / JH7110 / Intel N100"]
        Iface["Doble GbE · GPIO · SPI · I2C\nUART · RS-485 · CAN"]
    end
    App --> Krn
    Krn --> HW
```

## Flujo de arranque

```mermaid
flowchart TB
    PWR["Encendido"] --> ROM["Mask ROM"]
    ROM --> SPL["U-Boot SPL"]
    SPL --> ATF["ATF (BL31)"]
    ATF --> UBT["U-Boot Principal"]
    UBT -->|Cargar núcleo + DTB + initramfs| KRN["Núcleo Linux"]
    KRN -->|Ejecutar /init| INIT["aris-core (PID 1)"]
    INIT --> ETH["Configurar eth0 (WAN) + eth1 (LAN)"]
    INIT --> MNT["Montar partición persistente (/data)"]
    INIT --> ID["Verificar / aprovisionar identidad del dispositivo"]
    INIT --> EVN["Lanzar demonio evernight"]
    EVN --> CFG["Cargar /etc/evernight/config.toml"]
    EVN --> REG["Conectar a entelecheia (WebSocket)"]
    EVN --> LSN["Iniciar escuchas de protocolo\nModbus :502 · S7comm :102 · OPC UA :4840"]
    INIT --> SUP["Bucle de supervisión\nAlimentar watchdog · Health-check evernight · Gestionar OTA"]
```

## Disposición de particiones (Actualización A/B)

| Desplazamiento | Tamaño | Partición | Contenido |
|----------------|--------|-----------|-----------|
| 0 | 32 KiB | (hueco) | idbloader.img |
| 32 KiB | 8 MiB | (hueco) | u-boot.itb |
| 8 MiB | 128 MiB | boot-A | Image + DTB + boot.scr |
| 136 MiB | 128 MiB | boot-B | Image + DTB + boot.scr (en espera) |
| 264 MiB | 512 MiB | rootfs-A | squashfs (ro) |
| 776 MiB | 512 MiB | rootfs-B | squashfs (ro, en espera) |
| 1288 MiB | - | persistente | ext4 (rw, /data) |

## Topología de red

```mermaid
flowchart TB
    NET["Internet / LAN empresarial"] --> ETH0
    subbox GW["Pasarela aris"]
        ETH0["eth0 — WAN (DHCP)"]
        ETH1["eth1 — LAN (192.168.42.1/24)"]
    end
    ETH1 --> PLC["PLC\n192.168.42.5"]
    ETH1 --> SEN["Sensor\n192.168.42.10"]
    ETH1 --> HMI["HMI\n192.168.42.20"]
```

## Estrategia Asterinas ARM64 (Fase 2)

Fuente principal upstream para Asterinas ARM64:

- **Fork**: https://github.com/wanywhn/asterinas (rama: `arm64-support`)
- **PR**: asterinas/asterinas#3270
- **Estado**: Casi listo para fusionar; incluye GICv3, ARM GIC, árbol de
  dispositivos básico, configuración MMU y consola UART para aarch64

Una vez fusionado en el mainline de Asterinas, aris seguirá el repositorio
oficial. Hasta entonces, la rama `arm64-support` sirve como base de desarrollo.
