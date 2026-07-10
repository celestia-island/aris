# ARIS 아키텍처

## 개요

ARIS는 servo에서 파생된 브라우저 엔진입니다. 라이브러리로 Rust 앱에 임베딩하거나 스탠드얼론 브라우저로 실행할 수 있습니다. 렌더링 파이프라인은 순수 Rust 크레이트로 구성되며, servo의 SpiderMonkey / WebRender / components/script를 Boa / Vello CPU / Wasmtime으로 대체했습니다.

## 주요 대체

| Servo 컴포넌트 | ARIS 대안 | 이유 |
|---------------|----------|------|
| SpiderMonkey (C++) | boa_engine | 순수 Rust, C++ 빌드 불필요 |
| WebRender + SWGL (C++) | vello_cpu | 순수 Rust CPU 래스터화 |
| components/script | Boa 브릿지 | SpiderMonkey 결합 없음 |
| — | wasmtime | WASM Component Model + WASI |

## 디스플레이 백엔드

| 백엔드 | 용도 |
|--------|------|
| /dev/fb0 mmap | 임베디드 장치, kei 커널 |
| winit + softbuffer | 데스크톱 (Linux/macOS/Windows) |
| WASM canvas | 브라우저 임베딩 (WASM) |

## 두 가지 동작 모드

1. **임베딩 모드** (라이브러리): `render_html()`이 픽셀 버퍼를 직접 출력
2. **스탠드얼론 모드** (브라우저): `render_window` 바이너리가 전체 창을 표시

## 관련 프로젝트

- **[kei](https://github.com/celestia-island/kei)** — Rust OS 커널
- **[tairitsu](https://github.com/celestia-island/tairitsu)** — WASM UI 프레임워크
- **[hikari](https://github.com/celestia-island/hikari)** — UI 컴포넌트 라이브러리
- **[shirabe](https://github.com/celestia-island/shirabe)** — 브라우저 자동화
