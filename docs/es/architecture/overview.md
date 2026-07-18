# Arquitectura ARIS

## Visión general

ARIS es un motor de navegador derivado de servo. Puede integrarse como biblioteca o ejecutarse como navegador autónomo. El pipeline de renderizado usa crates 100% Rust, reemplazando SpiderMonkey / WebRender / components/script de servo por Boa / Vello CPU / Wasmtime.

## Reemplazos clave

| Componente Servo | Alternativa ARIS | Razón |
|-----------------|------------------|-------|
| SpiderMonkey (C++) | boa_engine | 100% Rust, sin build C++ |
| WebRender + SWGL (C++) | vello_cpu | Rasterización CPU 100% Rust |
| components/script | Puente Boa | Sin acoplamiento SpiderMonkey |
| — | wasmtime | WASM Component Model + WASI |

## Backends de pantalla

| Backend | Uso |
|---------|-----|
| /dev/fb0 mmap | Embebido, núcleo kei |
| winit + softbuffer | Escritorio (Linux/macOS/Windows) |
| WASM canvas | Integración en navegador (WASM) |

## Dos modos

1. **Modo integrado** (biblioteca): `render_html()` produce un buffer de píxeles
2. **Modo autónomo** (navegador): `render_window` abre una ventana completa


