# بنية نظام aris

## نظرة عامة

aris هو نظام تشغيل مدمج معياري لبوابات إنترنت الأشياء الصناعية يشغّل منظومة
Entelecheia. يربط وسيط البروتوكولات evernight بالعتاد المادي عبر طبقة نواة
أمنية ومختصرة.

## طبقات البنية

```mermaid
flowchart TB
    subgraph App["طبقة التطبيقات"]
        Evn["evernight\nوسيط البروتوكولات\nModbus RTU/TCP · S7comm · EtherCAT\nناقل CAN · OPC UA"]
        Core["aris-core (مُشرف)\nWatchdog · تحديث OTA\nتكوين الشبكة · التزويد"]
        Evn ---|مقبس Unix / IPC| Core
    end
    subgraph Krn["طبقة النواة"]
        K1["المرحلة 1: Linux 6.x\n(aarch64 / armv7 / riscv64)"]
        K2["المرحلة 2: نواة إطار Asterinas\n(Rust, MPL-2.0)"]
        Drv["التعريفات: stmmac · dw_wdt · 8250_dw\ngpio-rockchip · spi-rockchip\ni2c-rk3x · dw_mmc · phy-rockchip"]
    end
    subgraph HW["طبقة العتاد"]
        Soc["RK3566 / BCM2711 / JH7110 / Intel N100"]
        Iface["GbE مزدوج · GPIO · SPI · I2C\nUART · RS-485 · CAN"]
    end
    App --> Krn
    Krn --> HW
```

## تسلسل الإقلاع

```mermaid
flowchart TB
    PWR["تشغيل الطاقة"] --> ROM["Mask ROM"]
    ROM --> SPL["U-Boot SPL"]
    SPL --> ATF["ATF (BL31)"]
    ATF --> UBT["U-Boot Principal"]
    UBT -->|تحميل النواة + DTB + initramfs| KRN["نواة Linux"]
    KRN -->|تنفيذ /init| INIT["aris-core (PID 1)"]
    INIT --> ETH["تكوين eth0 (WAN) + eth1 (LAN)"]
    INIT --> MNT["تحميل القسم الدائم (/data)"]
    INIT --> ID["فحص / تزويد هوية الجهاز"]
    INIT --> EVN["تشغيل عملية evernight الخلفية"]
    EVN --> CFG["تحميل /etc/evernight/config.toml"]
    EVN --> REG["الاتصال بـ entelecheia (WebSocket)"]
    EVN --> LSN["بدء مستمعي البروتوكول\nModbus :502 · S7comm :102 · OPC UA :4840"]
    INIT --> SUP["حلقة الإشراف\nتغذية Watchdog · فحص صحة evernight · معالجة OTA"]
```

## تخطيط الأقسام (تحديث A/B)

| الإزاحة | الحجم | القسم | المحتوى |
|---------|--------|-----------|----------|
| 0 | 32 ك.بايت | (فجوة) | idbloader.img |
| 32 ك.بايت | 8 م.بايت | (فجوة) | u-boot.itb |
| 8 م.بايت | 128 م.بايت | boot-A | Image + DTB + boot.scr |
| 136 م.بايت | 128 م.بايت | boot-B | Image + DTB + boot.scr (احتياطي) |
| 264 م.بايت | 512 م.بايت | rootfs-A | squashfs (ro) |
| 776 م.بايت | 512 م.بايت | rootfs-B | squashfs (ro، احتياطي) |
| 1288 م.بايت | - | دائم | ext4 (rw، /data) |

## طوبولوجيا الشبكة

```mermaid
flowchart TB
    NET["الإنترنت / شبكة LAN للمؤسسة"] --> ETH0
    subbox GW["بوابة aris"]
        ETH0["eth0 — WAN (DHCP)"]
        ETH1["eth1 — LAN (192.168.42.1/24)"]
    end
    ETH1 --> PLC["PLC\n192.168.42.5"]
    ETH1 --> SEN["مستشعر\n192.168.42.10"]
    ETH1 --> HMI["HMI\n192.168.42.20"]
```

## استراتيجية Asterinas ARM64 (المرحلة 2)

المصدر الرئيسي لـ Asterinas ARM64:

- **التفرّع**: https://github.com/wanywhn/asterinas (الفرع: `arm64-support`)
- **طلب الدمج**: asterinas/asterinas#3270
- **الحالة**: شبه جاهز للدمج؛ يشمل GICv3 وARM GIC وشجرة أجهزة أساسية
  وإعداد MMU ووحدة تحكم UART لـ aarch64

بمجرد دمجه في الفرع الرئيسي لـ Asterinas، سيتتبع aris المستودع الرسمي. حتى
ذلك الحين، يعمل فرع `arm64-support` كأساس للتطوير.
