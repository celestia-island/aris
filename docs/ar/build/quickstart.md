# البدء السريع — بناء ARIS

## المتطلبات المسبقة

- Rust 1.85+ (عبر `rustup`)
- `just` (`cargo install just`)

## 1. استنساخ

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. بناء المحرك

```bash
cargo build -p aris-render --release
cargo build -p aris-render --release --features winit-backend
cargo build -p aris-wasm --release
```

## 3. تشغيل الأمثلة

```bash
cargo run -p aris-render --bin render_test
cargo run -p aris-render --bin render_lagrange -- صفحة.html
cargo run -p aris-render --bin render_window --features winit-backend
cargo run -p aris-wasm --bin render_wasm -- مكون.wasm
```

## 4. إعدادات المنصة

### Linux

```bash
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev
```

macOS / Windows: لا توجد اعتماديات إضافية.

## 5. الاختبارات

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```
