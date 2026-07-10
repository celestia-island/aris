# Démarrage rapide — Compiler ARIS

## Prérequis

- Rust 1.85+ (via `rustup`)
- `just` (`cargo install just`)

## 1. Cloner

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. Compiler le moteur

```bash
cargo build -p aris-render --release
cargo build -p aris-render --release --features winit-backend
cargo build -p aris-wasm --release
```

## 3. Exécuter les exemples

```bash
cargo run -p aris-render --bin render_test
cargo run -p aris-render --bin render_lagrange -- page.html
cargo run -p aris-render --bin render_window --features winit-backend
cargo run -p aris-wasm --bin render_wasm -- composant.wasm
```

## 4. Configuration par plateforme

### Linux

```bash
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev
```

macOS / Windows : aucune dépendance supplémentaire.

## 5. Tests

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```
