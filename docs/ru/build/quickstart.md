# Быстрый старт — Сборка ARIS

## Предварительные требования

- Rust 1.85+ (через `rustup`)
- `just` (`cargo install just`)

## 1. Клонирование

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. Сборка движка

```bash
cargo build -p aris-render --release
cargo build -p aris-render --release --features winit-backend
cargo build -p aris-wasm --release
```

## 3. Запуск примеров

```bash
cargo run -p aris-render --bin render_test
cargo run -p aris-render --bin render_lagrange -- страница.html
cargo run -p aris-render --bin render_window --features winit-backend
cargo run -p aris-wasm --bin render_wasm -- компонент.wasm
```

## 4. Настройка платформы

### Linux

```bash
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev
```

macOS / Windows: дополнительных зависимостей нет.

## 5. Тесты

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```
