// aris_browser — a minimal browser that renders web content in a winit window.
//
// This demonstrates the complete aris rendering stack:
//   - HTML ingestion (from file or lagrange SSG output)
//   - Optional Boa JS execution (with --features js)
//   - blitz-dom parsing + Stylo CSS cascade + Taffy layout
//   - Vello CPU rasterization
//   - winit + softbuffer window display
//
// Usage (with --features "js winit"):
//   aris_browser [html-file]
//
// Without an argument, renders a built-in demo page with visible text content.

fn main() {
    let html = if let Some(path) = std::env::args().nth(1) {
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                eprintln!("Loaded {} bytes from {}", s.len(), path);
                s
            }
            Err(e) => {
                eprintln!("Cannot read {}: {}", path, e);
                std::process::exit(1);
            }
        }
    } else {
        // Built-in demo page with hardcoded colors (blitz renders these well)
        r#"<!DOCTYPE html><html><head><style>
body { margin:0; background:#1a1b26; font-family: system-ui, sans-serif; color:#a9b1d6; }
.header { background:#7aa2f7; color:#1a1b26; padding:20px; font-size:28px; font-weight:bold; }
.nav { display:flex; background:#24283b; padding:12px 20px; gap:16px; }
.nav-item { color:#7dcfff; font-size:16px; }
.content { padding:24px; }
.card { background:#24283b; border-radius:12px; padding:20px; margin:16px 0; }
.card h2 { color:#bb9af7; margin:0 0 12px; font-size:22px; }
.card p { color:#9aa5ce; line-height:1.6; margin:8px 0; }
.tag { display:inline-block; background:#9ece6a; color:#1a1b26; padding:4px 12px; border-radius:9999px; font-size:13px; margin:4px; }
</style></head><body>
<div class="header">aris browser</div>
<div class="nav">
<span class="nav-item">Home</span>
<span class="nav-item">Docs</span>
<span class="nav-item">Packages</span>
<span class="nav-item">Guides</span>
</div>
<div class="content">
<div class="card">
<h2>aris-render pipeline</h2>
<p>HTML &rarr; blitz-dom (html5ever) &rarr; Stylo CSS cascade &rarr; Taffy layout &rarr; Vello CPU rasterization &rarr; winit window.</p>
<span class="tag">blitz-dom</span><span class="tag">vello-cpu</span><span class="tag">winit</span>
</div>
<div class="card">
<h2>Supported content sources</h2>
<p>Lagrange SSG HTML (hikari-styled docs), tairitsu WASM components (via wasmtime), inline HTML with Boa JS.</p>
<span class="tag">lagrange</span><span class="tag">tairitsu-ssr</span><span class="tag">boa-js</span>
</div>
<div class="card">
<h2>kei kernel integration</h2>
<p>Same rendering pipeline targets /dev/fb0 on kei (bare-metal aarch64), displaying in QEMU virtio-gpu scanout.</p>
<span class="tag">fbdev</span><span class="tag">virtio-gpu</span>
</div>
</div>
</body></html>
"#.to_string()
    };

    let config = aris_render::RenderConfig {
        width: 1024,
        height: 768,
        scale: 1.0,
    };

    eprintln!("[aris-browser] opening window ({}x{})...", config.width, config.height);
    if let Err(e) = aris_render::winit_backend::run_window(&html, &config) {
        eprintln!("[aris-browser] error: {:?}", e);
        std::process::exit(1);
    }
}
