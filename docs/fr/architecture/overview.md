# Architecture du système aris

## Aperçu

aris est un OS embarqué modulaire pour les passerelles IoT industrielles exécutant
l'écosystème Entelecheia. Il relie le courtier de protocoles evernight au
matériel physique via une couche noyau minimale et sécurisée.

## Couches d'architecture

```mermaid
flowchart TB
    subgraph App["Couche applicative"]
        Evn["evernight\nCourtier de protocoles\nModbus RTU/TCP · S7comm · EtherCAT\nBus CAN · OPC UA"]
        Core["aris-core (superviseur)\nWatchdog · Mise à jour OTA\nConfig réseau · Provisionnement"]
        Evn ---|Socket Unix / IPC| Core
    end
    subgraph Krn["Couche noyau"]
        K1["Phase 1 : Linux 6.x\n(aarch64 / armv7 / riscv64)"]
        K2["Phase 2 : Framekernel Asterinas\n(Rust, MPL-2.0)"]
        Drv["Pilotes : stmmac · dw_wdt · 8250_dw\ngpio-rockchip · spi-rockchip\ni2c-rk3x · dw_mmc · phy-rockchip"]
    end
    subgraph HW["Couche matérielle"]
        Soc["RK3566 / BCM2711 / JH7110 / Intel N100"]
        Iface["Double GbE · GPIO · SPI · I2C\nUART · RS-485 · CAN"]
    end
    App --> Krn
    Krn --> HW
```

## Flux de démarrage

```mermaid
flowchart TB
    PWR["Mise sous tension"] --> ROM["Mask ROM"]
    ROM --> SPL["U-Boot SPL"]
    SPL --> ATF["ATF (BL31)"]
    ATF --> UBT["U-Boot Principal"]
    UBT -->|Charger noyau + DTB + initramfs| KRN["Noyau Linux"]
    KRN -->|Exécuter /init| INIT["aris-core (PID 1)"]
    INIT --> ETH["Configurer eth0 (WAN) + eth1 (LAN)"]
    INIT --> MNT["Monter partition persistante (/data)"]
    INIT --> ID["Vérifier / provisionner l'identité du périphérique"]
    INIT --> EVN["Lancer le daemon evernight"]
    EVN --> CFG["Charger /etc/evernight/config.toml"]
    EVN --> REG["Se connecter à entelecheia (WebSocket)"]
    EVN --> LSN["Démarrer les écouteurs de protocole\nModbus :502 · S7comm :102 · OPC UA :4840"]
    INIT --> SUP["Boucle de supervision\nNourrir watchdog · Health-check evernight · Gérer OTA"]
```

## Disposition des partitions (Mise à jour A/B)

| Décalage | Taille | Partition | Contenu |
|----------|--------|-----------|---------|
| 0 | 32 Kio | (écart) | idbloader.img |
| 32 Kio | 8 Mio | (écart) | u-boot.itb |
| 8 Mio | 128 Mio | boot-A | Image + DTB + boot.scr |
| 136 Mio | 128 Mio | boot-B | Image + DTB + boot.scr (secours) |
| 264 Mio | 512 Mio | rootfs-A | squashfs (ro) |
| 776 Mio | 512 Mio | rootfs-B | squashfs (ro, secours) |
| 1288 Mio | - | persistante | ext4 (rw, /data) |

## Topologie réseau

```mermaid
flowchart TB
    NET["Internet / LAN d'entreprise"] --> ETH0
    subbox GW["Passerelle aris"]
        ETH0["eth0 — WAN (DHCP)"]
        ETH1["eth1 — LAN (192.168.42.1/24)"]
    end
    ETH1 --> PLC["PLC\n192.168.42.5"]
    ETH1 --> SEN["Capteur\n192.168.42.10"]
    ETH1 --> HMI["HMI\n192.168.42.20"]
```

## Stratégie Asterinas ARM64 (Phase 2)

Source amont principale pour Asterinas ARM64 :

- **Fork** : https://github.com/wanywhn/asterinas (branche : `arm64-support`)
- **PR** : asterinas/asterinas#3270
- **Statut** : Prêt à être fusionné ; inclut GICv3, ARM GIC, arbre de
  périphériques de base, configuration MMU et console UART pour aarch64

Une fois fusionné dans le mainline Asterinas, aris suivra le dépôt officiel.
D'ici là, la branche `arm64-support` sert de base de développement.
