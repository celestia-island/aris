// aris_browser — a browser that renders web content in a winit window.
//
// Features:
//   - HiDPI super-sampled rendering (sharp text on Retina/200% displays)
//   - Hot reload: pass an HTML file and edit it; the window updates live
//   - Interactive: mouse hover/click/scroll, F5 to reload, ESC to quit
//
// Usage:
//   aris_browser                 # built-in demo page
//   aris_browser page.html       # load file + watch for changes (hot reload)
//
// When a file is passed, aris_browser watches it and re-renders on save.
// Press F5 to force a reload, ESC to quit.

fn main() {
    let arg = std::env::args().nth(1);
    let config = aris_render::RenderConfig {
        width: 1024,
        height: 768,
        scale: 1.0,
    };

    if let Some(path) = arg {
        eprintln!("[aris-browser] loading {} (hot reload enabled)", path);
        if let Err(e) = aris_render::winit_backend::run_window_file(&path, &config) {
            eprintln!("[aris-browser] error: {:?}", e);
            std::process::exit(1);
        }
    } else {
        let html = r#"<!DOCTYPE html><html><head><style>
body { margin:0; background:#1a1b26; font-family: system-ui, sans-serif; color:#a9b1d6; }
.header { background:#7aa2f7; color:#1a1b26; padding:20px; font-size:28px; font-weight:bold; }
.nav { display:flex; background:#24283b; padding:12px 20px; gap:16px; }
.nav-item { color:#7dcfff; font-size:16px; cursor: pointer; transition: color 0.2s; }
.nav-item:hover { color:#bb9af7; }
.content { padding:24px; }
.card { background:#24283b; border-radius:12px; padding:20px; margin:16px 0; transition: transform 0.15s; }
.card:hover { transform: translateY(-2px); }
.card h2 { color:#bb9af7; margin:0 0 12px; font-size:22px; }
.card p { color:#9aa5ce; line-height:1.6; margin:8px 0; }
.tag { display:inline-block; background:#9ece6a; color:#1a1b26; padding:4px 12px; border-radius:9999px; font-size:13px; margin:4px; cursor: pointer; transition: background 0.2s; }
.tag:hover { background:#7dcfff; }
button { background:#7aa2f7; color:#1a1b26; border:none; padding:10px 20px; border-radius:8px; font-size:15px; cursor: pointer; transition: background 0.2s; }
button:hover { background:#bb9af7; }
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
<h2>Interactive features</h2>
<p>Hover over nav items and tags to see CSS :hover effects. Click buttons to test interaction. Press F5 to reload, ESC to quit.</p>
<button onclick="alert('clicked')">Click me</button>
</div>
<div class="card">
<h2>HiDPI rendering</h2>
<p>This page is rendered at full physical pixel resolution (e.g. 4096x3072 on a 200% DPI display). Text should be sharp, not blurry.</p>
<span class="tag">hidpi</span><span class="tag">super-sampled</span>
</div>
</div>
</body></html>"#;

        eprintln!("[aris-browser] opening window ({}x{})...", config.width, config.height);
        if let Err(e) = aris_render::winit_backend::run_window(&html, &config) {
            eprintln!("[aris-browser] error: {:?}", e);
            std::process::exit(1);
        }
    }
}
