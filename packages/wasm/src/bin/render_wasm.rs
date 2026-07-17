// Render a tairitsu/hikari WASM component through the full aris pipeline:
// Wasmtime host (tairitsu-ssr) → HTML → blitz-dom + Vello CPU → PPM.
//
// This is the Phase 1B end-to-end binary. It proves the complete
// "wasmtime → blitz" data flow: a WASM Component is executed by Wasmtime
// (with the tairitsu-browser:full WIT world), the resulting HTML is parsed
// by blitz-dom, laid out by Taffy, and rasterized by Vello CPU.
//
// Usage: render_wasm <component.wasm> [output.ppm]

fn main() {
    let wasm_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tests/fixtures/tairitsu_website.wasm".to_string());
    let out_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "wasm_render.ppm".to_string());

    let wasm_bytes = match std::fs::read(&wasm_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Cannot read {}: {}", wasm_path, e);
            std::process::exit(1);
        }
    };
    eprintln!("Loaded {} bytes from {}", wasm_bytes.len(), wasm_path);

    let config = aris_render::RenderConfig {
        width: 1280,
        height: 800,
        scale: 1.0,
    };

    // Step 1: Execute the WASM component via Wasmtime → HTML string
    eprintln!("[wasm] executing component via tairitsu-ssr...");
    let html = match aris_wasm::render_component_to_html(
        &wasm_bytes,
        config.width as i32,
        config.height as i32,
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[wasm] SSR error: {:?}", e);
            std::process::exit(1);
        }
    };
    eprintln!("[wasm] SSR produced {} bytes of HTML", html.len());
    let preview: String = html.chars().take(200).collect();
    eprintln!("[wasm] HTML preview: {}...", preview);

    // Step 2: Render the HTML through blitz → Frame
    match aris_render::render_html(&html, &config) {
        Ok(frame) => {
            let total = frame.width as usize * frame.height as usize;
            let non_black = frame
                .rgba
                .chunks_exact(4)
                .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
                .count();
            let pct = non_black
                .checked_mul(100)
                .and_then(|n| n.checked_div(total))
                .unwrap_or(0);
            eprintln!(
                "[wasm] rendered {}x{}: non-black {}/{} ({}%)",
                frame.width, frame.height, non_black, total, pct
            );

            match frame.save_ppm(&out_path) {
                Ok(()) => eprintln!("[wasm] saved: {}", out_path),
                Err(e) => eprintln!("[wasm] save error: {}", e),
            }

            if pct < 1 {
                eprintln!("[wasm] FAIL: non-black ratio {}% below 1%", pct);
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("[wasm] render error: {:?}", e);
            std::process::exit(1);
        }
    }
}
