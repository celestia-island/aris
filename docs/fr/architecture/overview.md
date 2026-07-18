# Architecture ARIS

## Aperçu

ARIS est un moteur de navigateur dérivé de servo. Il peut être intégré comme bibliothèque ou exécuté comme navigateur autonome. Le pipeline de rendu utilise des crates 100% Rust, remplaçant SpiderMonkey / WebRender / components/script de servo par Boa / Vello CPU / Wasmtime.

## Remplacements clés

| Composant Servo | Alternative ARIS | Raison |
|-----------------|-----------------|--------|
| SpiderMonkey (C++) | boa_engine | 100% Rust, sans build C++ |
| WebRender + SWGL (C++) | vello_cpu | Rastérisation CPU 100% Rust |
| components/script | Pont Boa | Sans couplage SpiderMonkey |
| — | wasmtime | WASM Component Model + WASI |

## Backends d'affichage

| Backend | Usage |
|---------|-------|
| /dev/fb0 mmap | Embarqué, noyau kei |
| winit + softbuffer | Bureau (Linux/macOS/Windows) |
| WASM canvas | Intégration navigateur (WASM) |

## Deux modes

1. **Mode intégré** (bibliothèque) : `render_html()` produit un buffer de pixels
2. **Mode autonome** (navigateur) : `render_window` ouvre une fenêtre complète


