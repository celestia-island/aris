// Open a desktop window and render HTML into it via winit + softbuffer.
//
// This is the Phase 2 deliverable: proves aris-render can display rendered
// HTML in a native OS window on Windows/Linux/macOS, not just write a PPM
// file or /dev/fb0.
//
// Usage: render_window [html-file]
//   Defaults: the lagrange docs fixture (tests/fixtures/lagrange_index.html)

fn main() {
    let html_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tests/fixtures/lagrange_index.html".to_string());

    let html = match std::fs::read_to_string(&html_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot read {}: {}", html_path, e);
            std::process::exit(1);
        }
    };
    eprintln!("Loaded {} bytes from {}", html.len(), html_path);

    let config = aris_render::RenderConfig {
        width: 1280,
        height: 800,
        scale: 1.0,
    };

    if let Err(e) = aris_render::winit_backend::run_window(&html, &config) {
        eprintln!("Window error: {:?}", e);
        std::process::exit(1);
    }
}
