// aris_browser — a browser that renders web content in a winit window.
//
// Features:
//   - Browser chrome: address bar, back/forward/reload buttons
//   - Navigation: clicking <a href>, form submission, address-bar URL entry
//   - Networking: HTTP(S) and file:// fetching of pages + subresources
//   - Text input: typing into <input>/<textarea>
//   - HiDPI super-sampled rendering
//
// Usage:
//   aris_browser                 # start page (new-tab bookmarks)
//   aris_browser page.html       # load local HTML file
//   aris_browser ./docs          # load local directory index (file://)
//   aris_browser https://example.com   # fetch and render a web page
//   aris_browser "search terms"  # web search
//
// Keyboard:
//   Ctrl+L  focus the address bar       Ctrl+R / F5  reload
//   Enter   (in address bar) navigate   Esc          blur address bar / quit
//   Alt+Left/Right  back/forward (via address bar typing not yet)

fn main() {
    aris_render::init_logging();
    let arg = std::env::args().nth(1);
    let config = aris_render::RenderConfig {
        width: 1024,
        height: 768,
        scale: 1.0,
    };

    if let Some(target) = arg {
        tracing::info!("loading {}", target);
        if let Err(e) = aris_render::winit_backend::run_window_url(&target, &config) {
            tracing::error!("error: {:?}", e);
            std::process::exit(1);
        }
    } else {
        // No argument: show the start page.
        tracing::info!("opening start page ({}x{})...", config.width, config.height);
        let html = start_page();
        if let Err(e) = aris_render::winit_backend::run_window(&html, &config) {
            tracing::error!("error: {:?}", e);
            std::process::exit(1);
        }
    }
}

fn start_page() -> String {
    r#"<!DOCTYPE html><html lang="en"><head><meta charset="UTF-8">
<title>aris — new tab</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{background:#1e1e2e;color:#cdd6f4;font-family:system-ui,Segoe UI,DejaVu Sans,sans-serif;display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:100vh}
.wrap{max-width:600px;text-align:center;padding:40px 24px}
.logo{font-size:64px;font-weight:900;color:#cba6f7;letter-spacing:-2px;margin-bottom:4px}
.sub{color:#585b70;font-size:15px;margin-bottom:36px}
.bookmarks{display:flex;flex-wrap:wrap;gap:10px;justify-content:center}
a.card{display:block;background:#313244;color:#cdd6f4;text-decoration:none;padding:14px 20px;border-radius:8px;min-width:160px;text-align:left}
a.card:hover{background:#45475a}
.card .t{color:#89b4fa;font-weight:600;font-size:15px;display:block}
.card .d{color:#6c7086;font-size:12px;margin-top:3px}
.hint{margin-top:36px;color:#585b70;font-size:13px}
kbd{background:#313244;padding:2px 6px;border-radius:4px;color:#89dceb;font-size:12px}
</style></head><body>
<div class="wrap">
  <div class="logo">aris</div>
  <p class="sub">Pure-Rust browser engine · Blitz + Stylo + Vello</p>
  <div class="bookmarks">
    <a class="card" href="https://example.com"><span class="t">example.com</span><span class="d">test page</span></a>
    <a class="card" href="about:about"><span class="t">about:about</span><span class="d">engine info</span></a>
    <a class="card" href="https://info.cern.ch"><span class="t">info.cern.ch</span><span class="d">first website</span></a>
    <a class="card" href="file:///"><span class="t">file:///</span><span class="d">local files</span></a>
  </div>
  <p class="hint">Type a URL or search, or press <kbd>Ctrl+L</kbd> for address bar</p>
</div>
</body></html>"#.to_string()
}
