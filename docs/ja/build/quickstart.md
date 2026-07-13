# クイックスタート — ARIS をビルド

## 前提条件

- Rust 1.85+（`rustup` 経由）
- `just`（`cargo install just`）

## 1. クローン

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. ブラウザエンジンをビルド

```bash
cargo build -p aris-render --release
cargo build -p aris-render --release --features winit-backend
cargo build -p aris-wasm --release
```

## 3. サンプルを実行

```bash
cargo run -p aris-render --bin render_test
cargo run -p aris-render --bin render_lagrange -- path/to/page.html
cargo run -p aris-render --bin render_window --features winit-backend
cargo run -p aris-wasm --bin render_wasm -- path/to/component.wasm
```

## 4. プラットフォーム設定

### Linux

```bash
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev
```

macOS / Windows は追加依存なし。

## 5. テスト実行

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```
