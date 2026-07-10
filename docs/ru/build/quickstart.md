# Быстрый старт — От исходного кода до SD-карты

## Предварительные требования

- Хост Linux x86_64 или ARM64
- Rust 1.85+ (через `rustup`)
- Командный раннер `just` (`cargo install just`)
- Картридер SD + microSD (≥ 8 ГБ)

## 1. Клонирование

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. Настройка кросс-компиляции

```bash
just setup-cross
```

Это устанавливает целевые платформы Rust (`aarch64-unknown-linux-musl` и т. д.) и выводит инструкции по цепочке инструментов GCC для вашего дистрибутива.

## 3. Сборка прошивки

```bash
just build-board nanopi-r3s
```

Создаёт `output/nanopi-r3s/image.img`.

## 4. Запись на SD-карту

```bash
just flash-sd /dev/sdX
```

Замените `/dev/sdX` на ваше устройство SD-карты (проверьте через `lsblk`).

## 5. Загрузка

Вставьте SD-карту в NanoPi R3S, подключите питание USB-C 5V.

- **Последовательная консоль**: Подключите USB-TTL к 3-контактному отладочному разъёму (GND/TX/RX), 1500000 бод, 8N1
- **SSH**: После загрузки `ssh root@<ip>` (DHCP от WAN eth0)

## 6. Проверка

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
