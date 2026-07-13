# Inicio rápido — Compilar ARIS

## Requisitos previos

- Rust 1.85+ (vía `rustup`)
- `just` (`cargo install just`)

## 1. Clonar

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. Compilar el motor

```bash
cargo build -p aris-render --release
cargo build -p aris-render --release --features winit-backend
cargo build -p aris-wasm --release
```

## 3. Ejecutar ejemplos

```bash
cargo run -p aris-render --bin render_test
cargo run -p aris-render --bin render_lagrange -- pagina.html
cargo run -p aris-render --bin render_window --features winit-backend
cargo run -p aris-wasm --bin render_wasm -- componente.wasm
```

## 4. Configuración por plataforma

### Linux

```bash
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev
```

macOS / Windows: sin dependencias adicionales.

## 5. Tests

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```
