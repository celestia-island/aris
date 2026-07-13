# Quick Start — Build ARIS

## Prerequisites

- Rust 1.85+ (via `rustup`)
- `just` command runner (`cargo install just`)
- For winit backend: system dependencies for `winit` (see below)

## 1. Clone

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. Build the Browser Engine

```bash
# Build the render pipeline (no window)
cargo build -p aris-render --release

# Build with desktop window support
cargo build -p aris-render --release --features winit-backend

# Build the WASM host (for tairitsu components)
cargo build -p aris-wasm --release
```

## 3. Run Examples

```bash
# Render an HTML string to pixels (headless)
cargo run -p aris-render --bin render_test

# Render a lagrange documentation page
cargo run -p aris-render --bin render_lagrange -- path/to/page.html

# Open a desktop browser window
cargo run -p aris-render --bin render_window --features winit-backend

# Render a WASM component (tairitsu UI)
cargo run -p aris-wasm --bin render_wasm -- path/to/component.wasm
```

## 4. Platform-Specific Setup

### Linux

```bash
# Ubuntu/Debian
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev

# Build with winit
cargo build -p aris-render --release --features winit-backend
```

### macOS

No extra dependencies needed.

### Windows

No extra dependencies needed for winit.

## 5. Cross-Compile for Embedded (aarch64)

```bash
just setup-cross
cargo build -p aris-render --release --target aarch64-unknown-linux-musl
```

## 6. Run Tests

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```

## Next Steps

- [Architecture Overview](../architecture/overview.md) — understand the rendering pipeline
- [Deployment Guide](../guides/deployment.md) — deploy on embedded hardware
