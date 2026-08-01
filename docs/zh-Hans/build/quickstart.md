# 快速开始 — 构建 ARIS

## 前置条件

- Rust 1.85+（通过 `rustup`）
- `just` 命令运行器（`cargo install just`）

## 1. 克隆

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. 构建浏览器引擎

```bash
# 构建渲染管线（无窗口）
cargo build -p aris-render --release

# 构建桌面窗口支持
cargo build -p aris-render --release --features winit-backend

# 构建 WASM host（用于 tairitsu 组件）
cargo build -p aris-wasm --release
```

## 3. 运行示例

```bash
# 将 HTML 渲染为像素（无头模式）
cargo run -p aris-render --bin render_test

# 渲染 lagrange 文档页面
cargo run -p aris-render --bin render_lagrange -- path/to/page.html

# 打开桌面浏览器窗口
cargo run -p aris-render --bin render_window --features winit-backend

# 渲染 WASM 组件（tairitsu UI）
cargo run -p aris-wasm --bin render_wasm -- path/to/component.wasm
```

## 4. 平台特定设置

### Linux

```bash
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev
cargo build -p aris-render --release --features winit-backend
```

### macOS / Windows

无需额外依赖。

## 5. 运行测试

```bash
cargo test -p aris-render
cargo test -p aris-js
cargo test -p aris-wasm
```
