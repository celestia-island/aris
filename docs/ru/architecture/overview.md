# Архитектура ARIS

## Обзор

ARIS — браузерный движок на основе servo. Можно встроить как библиотеку или запустить как автономный браузер. Конвейер рендеринга использует 100% Rust крейты, заменяя SpiderMonkey / WebRender / components/script на Boa / Vello CPU / Wasmtime.

## Ключевые замены

| Компонент Servo | Альтернатива ARIS | Причина |
|-----------------|-------------------|---------|
| SpiderMonkey (C++) | boa_engine | 100% Rust, без сборки C++ |
| WebRender + SWGL (C++) | vello_cpu | Растеризация CPU на 100% Rust |
| components/script | Мост Boa | Без привязки к SpiderMonkey |
| — | wasmtime | WASM Component Model + WASI |

## Бэкенды отображения

| Бэкенд | Применение |
|--------|-----------|
| /dev/fb0 mmap | Встраиваемые устройства, ядро kei |
| winit + softbuffer | Десктоп (Linux/macOS/Windows) |
| WASM canvas | Встраивание в браузер (WASM) |

## Два режима

1. **Встроенный режим** (библиотека): `render_html()` выдаёт пиксельный буфер
2. **Автономный режим** (браузер): `render_window` открывает полноценное окно

## Связанные проекты

- **[kei](https://github.com/celestia-island/kei)** — Ядро ОС на Rust
- **[tairitsu](https://github.com/celestia-island/tairitsu)** — UI фреймворк на WASM
- **[hikari](https://github.com/celestia-island/hikari)** — Библиотека UI компонентов
- **[shirabe](https://github.com/celestia-island/shirabe)** — Автоматизация браузера
