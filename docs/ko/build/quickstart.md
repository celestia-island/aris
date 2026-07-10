# 빠른 시작 — ARIS 빌드

## 사전 요구 사항

- Rust 1.85+ (`rustup` 경유)
- `just` (`cargo install just`)

## 1. 클론

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. 브라우저 엔진 빌드

```bash
cargo build -p aris-render --release
cargo build -p aris-render --release --features winit-backend
cargo build -p aris-wasm --release
```

## 3. 예제 실행

```bash
cargo run -p aris-render --bin render_test
cargo run -p aris-render --bin render_lagrange -- path/to/page.html
cargo run -p aris-render --bin render_window --features winit-backend
cargo run -p aris-wasm --bin render_wasm -- path/to/component.wasm
```

## 4. 플랫폼 설정

### Linux

```bash
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev
```

macOS / Windows는 추가 의존성 없음.

## 5. 테스트 실행

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```
