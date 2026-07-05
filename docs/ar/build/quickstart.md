# البدء السريع — من المصدر إلى بطاقة SD

## المتطلبات المسبقة

- مضيف Linux x86_64 أو ARM64
- Rust 1.85+ (عبر `rustup`)
- منفّذ أوامر `just` (`cargo install just`)
- قارئ بطاقات SD + بطاقة microSD (≥ 8 ج.بايت)

## 1. الاستنساخ

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. إعداد الترجمة المتقاطعة

```bash
just setup-cross
```

يُثبّت أهداف Rust (مثل `aarch64-unknown-linux-musl`) ويطبع تعليمات سلسلة أدوات GCC لتوزيعتك.

## 3. بناء البرامج الثابتة

```bash
just build-board nanopi-r3s
```

ينتج `output/nanopi-r3s/image.img`.

## 4. الكتابة على بطاقة SD

```bash
just flash-sd /dev/sdX
```

استبدل `/dev/sdX` بجهاز بطاقة SD الخاص بك (تحقق عبر `lsblk`).

## 5. الإقلاع

أدخل بطاقة SD في NanoPi R3S، ووصّل مصدر طاقة USB-C بقوة 5 فولت.

- **وحدة التحكم التسلسلية**: وصّل USB-TTL بموصّل التصحيح ذي 3 أسنان (GND/TX/RX)، بمعدل 1500000 باود، 8N1
- **SSH**: بعد الإقلاع، `ssh root@<ip>` (DHCP من WAN eth0)

## 6. التحقق

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
