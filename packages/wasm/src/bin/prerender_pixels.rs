// Build-time full pipeline: tairitsu WASM → SSR → HTML → Blitz + Vello → RGBA pixels.
//
// Runs on the x86 host (fast Wasmtime + fast Vello CPU) and outputs a raw RGBA
// pixel buffer that kei_desktop embeds via include_bytes!. At runtime on kei,
// no rendering is needed — just copy pre-rendered pixels to /dev/fb0.
//
// This gives us the FULL aris-render pipeline (real CSS layout, real Vello
// rasterization, real colors/shapes/borders) without any QEMU TCG performance
// penalty. No functionality is sacrificed — the pixels are identical to what
// aris-render would produce at runtime.
//
// Usage:
//   cargo run --release -p aris-wasm --bin prerender_pixels --
//     <input.wasm> <output.rgba> [width] [height]

fn main() {
    let wasm_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tests/fixtures/kei_desktop.wasm".to_string());
    let out_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "kei_desktop.rgba".to_string());
    let width: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1200);
    let height: u32 = std::env::args()
        .nth(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(900);

    let wasm_bytes = match std::fs::read(&wasm_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Cannot read {}: {}", wasm_path, e);
            std::process::exit(1);
        }
    };
    eprintln!("[prerender] Loaded {} bytes from {}", wasm_bytes.len(), wasm_path);

    // Step 1: Execute the tairitsu component via Wasmtime SSR → HTML
    eprintln!("[prerender] SSR rendering at {}x{}...", width, height);
    let ssr_html = match aris_wasm::render_component_to_html(&wasm_bytes, width as i32, height as i32) {
        Ok(h) => {
            eprintln!("[prerender] SSR produced {} bytes of HTML", h.len());
            // Also dump HTML for debugging
            let _ = std::fs::write("tests/fixtures/kei_desktop_rendered.html", &h);
            h
        }
        Err(e) => {
            eprintln!("[prerender] SSR error: {:?}", e);
            std::process::exit(1);
        }
    };

    // Wrap SSR HTML in a proper HTML document with body background.
    // Blitz renders <body> as black by default; we need to set it to match
    // the desktop wallpaper color so there's no black bar at the top.
    let html = format!(
        "<!DOCTYPE html><html><head><style>body{{margin:0;padding:0;background:#B8F7F8;}}</style></head>{}",
        ssr_html
    );

    // Step 2: Render the HTML through Blitz + Vello CPU → RGBA pixels
    let config = aris_render::RenderConfig {
        width,
        height,
        scale: 1.0,
    };

    eprintln!("[prerender] Rasterizing via Blitz + Vello CPU (render_html_with_font)...");
    let frame = match aris_render::render_html_with_font(&html, &config) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[prerender] render error: {:?}", e);
            std::process::exit(1);
        }
    };

    let total = (frame.width * frame.height) as usize;
    let non_black = frame
        .rgba
        .chunks_exact(4)
        .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
        .count();
    let pct = non_black.checked_mul(100).and_then(|n| n.checked_div(total)).unwrap_or(0);
    eprintln!(
        "[prerender] Rasterized {}x{}: non-black {}/{} ({}%)",
        frame.width, frame.height, non_black, total, pct
    );

    // Step 3: Save raw RGBA pixels
    match std::fs::write(&out_path, &frame.rgba) {
        Ok(()) => {
            eprintln!(
                "[prerender] Wrote {} bytes of RGBA to {} ({}x{})",
                frame.rgba.len(),
                out_path,
                frame.width,
                frame.height
            );
        }
        Err(e) => {
            eprintln!("[prerender] Write error: {}", e);
            std::process::exit(1);
        }
    }

    if pct < 1 {
        eprintln!("[prerender] WARNING: non-black ratio {}% below 1%", pct);
        std::process::exit(2);
    }
}
