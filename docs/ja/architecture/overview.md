# ARIS アーキテクチャ

## 概要

ARIS は servo から派生したブラウザエンジンです。ライブラリとして Rust アプリに組み込むことも、スタンドアロンブラウザとして実行することも可能です。レンダリングパイプラインは純 Rust クレートで構成され、servo の SpiderMonkey / WebRender / components/script を Boa / Vello CPU / Wasmtime に置き換えています。

## 主要な置き換え

| Servo コンポーネント | ARIS 代替 | 理由 |
|---------------------|----------|------|
| SpiderMonkey (C++) | boa_engine | 純 Rust、C++ ビルド不要 |
| WebRender + SWGL (C++) | vello_cpu | 純 Rust CPU ラスタライズ |
| components/script | Boa ブリッジ | SpiderMonkey 結合なし |
| — | wasmtime | WASM Component Model + WASI |

## ディスプレイバックエンド

| バックエンド | 用途 |
|-------------|------|
| /dev/fb0 mmap | 組み込みデバイス、kei カーネル |
| winit + softbuffer | デスクトップ（Linux/macOS/Windows） |
| WASM canvas | ブラウザ埋め込み（WASM） |

## 2 つの動作モード

1. **組み込みモード**（ライブラリ）: `render_html()` がピクセルバッファを直接出力
2. **スタンドアロンモード**（ブラウザ）: `render_window` バイナリがフルウィンドウを表示

## 関連プロジェクト

- **[kei](https://github.com/celestia-island/kei)** — Rust OS カーネル
- **[tairitsu](https://github.com/celestia-island/tairitsu)** — WASM UI フレームワーク
- **[hikari](https://github.com/celestia-island/hikari)** — UI コンポーネントライブラリ
- **[shirabe](https://github.com/celestia-island/shirabe)** — ブラウザ自動化
