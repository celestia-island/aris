# Архитектура системы aris

## Обзор

aris — это модульная встраиваемая ОС для промышленных IoT-шлюзов, работающая в
экосистеме Entelecheia. Она связывает брокер протоколов evernight с физическим
оборудованием через минимальный защищённый слой ядра.

## Слои архитектуры

```mermaid
flowchart TB
    subgraph App["Слой приложений"]
        Evn["evernight\nБрокер протоколов\nModbus RTU/TCP · S7comm · EtherCAT\nШина CAN · OPC UA"]
        Core["aris-core (супервизор)\nWatchdog · OTA-обновление\nСетевые настройки · Инициализация"]
        Evn ---|Unix-сокет / IPC| Core
    end
    subgraph Krn["Слой ядра"]
        K1["Фаза 1: Linux 6.x\n(aarch64 / armv7 / riscv64)"]
        K2["Фаза 2: Фрейм-ядро Asterinas\n(Rust, MPL-2.0)"]
        Drv["Драйверы: stmmac · dw_wdt · 8250_dw\ngpio-rockchip · spi-rockchip\ni2c-rk3x · dw_mmc · phy-rockchip"]
    end
    subgraph HW["Слой оборудования"]
        Soc["RK3566 / BCM2711 / JH7110 / Intel N100"]
        Iface["Двойной GbE · GPIO · SPI · I2C\nUART · RS-485 · CAN"]
    end
    App --> Krn
    Krn --> HW
```

## Процесс загрузки

```mermaid
flowchart TB
    PWR["Включение питания"] --> ROM["Mask ROM"]
    ROM --> SPL["U-Boot SPL"]
    SPL --> ATF["ATF (BL31)"]
    ATF --> UBT["U-Boot Proper"]
    UBT -->|Загрузка ядра + DTB + initramfs| KRN["Ядро Linux"]
    KRN -->|Выполнение /init| INIT["aris-core (PID 1)"]
    INIT --> ETH["Настройка eth0 (WAN) + eth1 (LAN)"]
    INIT --> MNT["Монтирование постоянного раздела (/data)"]
    INIT --> ID["Проверка / инициализация идентичности устройства"]
    INIT --> EVN["Запуск демона evernight"]
    EVN --> CFG["Загрузка /etc/evernight/config.toml"]
    EVN --> REG["Подключение к entelecheia (WebSocket)"]
    EVN --> LSN["Запуск слушателей протоколов\nModbus :502 · S7comm :102 · OPC UA :4840"]
    INIT --> SUP["Цикл супервизии\nКормление watchdog · Health-check evernight · Обработка OTA"]
```

## Схема разделов (Обновление A/B)

| Смещение | Размер | Раздел | Содержимое |
|----------|--------|--------|------------|
| 0 | 32 КиБ | (промежуток) | idbloader.img |
| 32 КиБ | 8 МиБ | (промежуток) | u-boot.itb |
| 8 МиБ | 128 МиБ | boot-A | Image + DTB + boot.scr |
| 136 МиБ | 128 МиБ | boot-B | Image + DTB + boot.scr (резерв) |
| 264 МиБ | 512 МиБ | rootfs-A | squashfs (ro) |
| 776 МиБ | 512 МиБ | rootfs-B | squashfs (ro, резерв) |
| 1288 МиБ | - | постоянный | ext4 (rw, /data) |

## Сетевая топология

```mermaid
flowchart TB
    NET["Интернет / Корпоративная LAN"] --> ETH0
    subbox GW["Шлюз aris"]
        ETH0["eth0 — WAN (DHCP)"]
        ETH1["eth1 — LAN (192.168.42.1/24)"]
    end
    ETH1 --> PLC["ПЛК\n192.168.42.5"]
    ETH1 --> SEN["Датчик\n192.168.42.10"]
    ETH1 --> HMI["HMI\n192.168.42.20"]
```

## Стратегия Asterinas ARM64 (Фаза 2)

Основной upstream-источник для Asterinas ARM64:

- **Форк**: https://github.com/wanywhn/asterinas (ветка: `arm64-support`)
- **PR**: asterinas/asterinas#3270
- **Статус**: Почти готов к слиянию; включает GICv3, ARM GIC, базовое дерево
  устройств, настройку MMU и UART-консоль для aarch64

После слияния в mainline Asterinas aris будет отслеживать официальный репозиторий.
До тех пор ветка `arm64-support` служит базой разработки.
