# 快速開始 — 構建 ARIS

## 前置條件

- Rust 1.85+（透過 `rustup`）
- `just` 命令執行器（`cargo install just`）

## 1. 複製

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. 構建瀏覽器引擎

```bash
cargo build -p aris-render --release
cargo build -p aris-render --release --features winit-backend
cargo build -p aris-wasm --release
```

## 3. 執行範例

```bash
cargo run -p aris-render --bin render_test
cargo run -p aris-render --bin render_lagrange -- path/to/page.html
cargo run -p aris-render --bin render_window --features winit-backend
cargo run -p aris-wasm --bin render_wasm -- path/to/component.wasm
```

## 4. 平台設定

### Linux

```bash
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev
```

macOS / Windows 無需額外依賴。

## 5. 執行測試

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```
