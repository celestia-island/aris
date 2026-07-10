# Inicio rápido — Del código fuente a la tarjeta SD

## Requisitos previos

- Host Linux x86_64 o ARM64
- Rust 1.85+ (vía `rustup`)
- Ejecutor de comandos `just` (`cargo install just`)
- Lector de tarjetas SD + microSD (≥ 8 GB)

## 1. Clonar

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. Configurar la compilación cruzada

```bash
just setup-cross
```

Esto instala los destinos de Rust (`aarch64-unknown-linux-musl` etc.) e imprime las instrucciones de la cadena de herramientas GCC para su distribución.

## 3. Construir el firmware

```bash
just build-board nanopi-r3s
```

Produce `output/nanopi-r3s/image.img`.

## 4. Grabar en la tarjeta SD

```bash
just flash-sd /dev/sdX
```

Reemplace `/dev/sdX` con su dispositivo de tarjeta SD (verifique con `lsblk`).

## 5. Arrancar

Inserte la tarjeta SD en el NanoPi R3S, conecte la alimentación USB-C de 5V.

- **Consola serie**: Conecte un USB-TTL al header de depuración de 3 pines (GND/TX/RX), 1500000 baudios, 8N1
- **SSH**: Tras el arranque, `ssh root@<ip>` (DHCP desde WAN eth0)

## 6. Verificar

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
