// Offline test for Canvas 2D via the Boa JS bridge.
//
// Runs a <script> that creates a canvas, gets a 2d context, sets fillStyle,
// and calls fillRect. Verifies the canvas pixel buffer was modified.
//
//   cargo run -p aris-render --features "desktop winit js" --bin offline_canvas_js_test

use std::sync::Arc;

use aris_render::browser::{BrowserNavigationProvider, BrowserShellProvider, HttpNetProvider};
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::Viewport;

fn main() {
    aris_render::init_logging();

    let html = r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><title>canvas test</title>
<script>
  var c = document.createElement('canvas');
  var ctx = c.getContext('2d');
  ctx.fillStyle = '#ff0000';
  ctx.fillRect(10, 10, 50, 50);
</script>
</head><body></body></html>"#;

    let viewport = Viewport {
        window_size: (300, 150),
        hidpi_scale: 1.0,
        ..Default::default()
    };
    let state = Arc::new(aris_render::browser::BrowserState::new());
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        net_provider: Some(Arc::new(HttpNetProvider::new())),
        navigation_provider: Some(Arc::new(BrowserNavigationProvider::new(Arc::clone(&state)))),
        shell_provider: Some(Arc::new(BrowserShellProvider::new(Arc::clone(&state)))),
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, doc_config);
    doc.resolve(0.0);

    let mut rt = aris_render::js_runtime::JsRuntime::new();
    let scripts = aris_js::extract_scripts(html);
    rt.bind_and_run(&mut doc, &scripts.join("\n;\n"));

    // Check the bridge's canvas buffers for red pixels.
    let red_count = rt.canvas_red_pixels();
    println!("total red pixels across canvases: {}", red_count);

    if red_count > 0 {
        println!(
            "OK: JS canvas.getContext('2d').fillRect() produced {} red pixels",
            red_count
        );
    } else {
        println!("FAIL: no red pixels in any canvas buffer");
        std::process::exit(2);
    }
}
