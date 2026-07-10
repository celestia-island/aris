# ARIS 架構

## 概覽

ARIS 是源自 servo 的瀏覽器引擎。可作為庫嵌入任何 Rust 應用，或作為獨立桌面瀏覽器運行。渲染管線由純 Rust crate 組裝——html5ever、stylo、taffy、parley、vello——servo 原有的 SpiderMonkey（C++）、WebRender（C++/SWGL）、components/script 已被 Boa、Vello CPU、Wasmtime 分別替代。

## 關鍵替換

| Servo 組件 | ARIS 替代方案 | 理由 |
|-----------|-------------|------|
| SpiderMonkey (C++) | boa_engine | 純 Rust，無需 C++ 構建 |
| WebRender + SWGL (C++) | vello_cpu | 純 Rust CPU 光柵化 |
| components/script | Boa 橋接（aris-js） | 無 SpiderMonkey 耦合 |
| — | wasmtime | WASM Component Model + WASI |

## 顯示後端

| 後端 | 用途 |
|------|------|
| /dev/fb0 mmap | 嵌入式裝置、kei 核心 |
| winit + softbuffer | 桌面（Linux/macOS/Windows） |
| WASM canvas | 瀏覽器內嵌（WASM） |

## 兩種執行模式

1. **嵌入模式**（庫）：`render_html()` 函數直接產出像素緩衝
2. **獨立模式**（桌面瀏覽器）：`render_window` 二進位開啟完整桌面視窗

## 相關項目

- **[kei](https://github.com/celestia-island/kei)** — Rust OS 核心
- **[tairitsu](https://github.com/celestia-island/tairitsu)** — WASM UI 框架
- **[hikari](https://github.com/celestia-island/hikari)** — UI 組件庫
- **[shirabe](https://github.com/celestia-island/shirabe)** — 瀏覽器自動化
