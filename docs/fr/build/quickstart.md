# Démarrage rapide — De la source à la carte SD

## Prérequis

- Hôte Linux x86_64 ou ARM64
- Rust 1.85+ (via `rustup`)
- Exécuteur de commandes `just` (`cargo install just`)
- Lecteur de carte SD + microSD (≥ 8 Go)

## 1. Cloner

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. Configuration de la cross-compilation

```bash
just setup-cross
```

Cela installe les cibles Rust (`aarch64-unknown-linux-musl` etc.) et affiche les instructions de chaîne d'outils GCC pour votre distribution.

## 3. Construire le firmware

```bash
just build-board nanopi-r3s
```

Produit `output/nanopi-r3s/image.img`.

## 4. Flasher sur la carte SD

```bash
just flash-sd /dev/sdX
```

Remplacez `/dev/sdX` par votre périphérique de carte SD (vérifiez avec `lsblk`).

## 5. Démarrer

Insérez la carte SD dans le NanoPi R3S, connectez l'alimentation USB-C 5V.

- **Console série** : Connectez un USB-TTL au header de débogage 3 broches (GND/TX/RX), 1500000 bauds, 8N1
- **SSH** : Après le démarrage, `ssh root@<ip>` (DHCP depuis WAN eth0)

## 6. Vérifier

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
