// Render a real-world hikari/lagrange documentation page through the aris
// rendering pipeline (blitz-dom + Vello CPU) and report pixel statistics.
//
// This is the Phase 1A end-to-end baseline: it proves the aris renderer can
// ingest a complete HTML document produced by lagrange (a WASI-rendered
// Markdown static-site generator that emits hikari-styled HTML) and rasterize
// it to visible pixels. No JS engine (Boa) or WASM runtime (Wasmtime) is
// involved yet — lagrange pre-renders the markdown to static HTML at build
// time, so this path exercises the HTML/CSS/layout/raster pipeline only.
//
// Usage: render_lagrange [path-to-html] [output.ppm]
//   Defaults: tests/fixtures/lagrange_index.html, lagrange_render.ppm

use std::path::Path;

fn main() {
    let html_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tests/fixtures/lagrange_index.html".to_string());
    let out_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "lagrange_render.ppm".to_string());

    let html = match std::fs::read_to_string(&html_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot read {}: {}", html_path, e);
            std::process::exit(1);
        }
    };
    eprintln!(
        "Loaded {} ({} bytes) from {}",
        html.len(),
        html.len(),
        html_path
    );

    let config = aris_render::RenderConfig {
        width: 1280,
        height: 800,
        scale: 1.0,
    };

    match aris_render::render_html(&html, &config) {
        Ok(frame) => {
            let total = frame.width as usize * frame.height as usize;
            let non_black = frame
                .rgba
                .chunks_exact(4)
                .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
                .count();
            let pct = if total > 0 {
                non_black * 100 / total
            } else {
                0
            };
            eprintln!(
                "Rendered {}x{}: non-black {}/{} ({}%)",
                frame.width, frame.height, non_black, total, pct
            );

            // Try /dev/fb0 if present (kei / Linux fbdev)
            #[cfg(unix)]
            if Path::new("/dev/fb0").exists() {
                eprintln!("Opening /dev/fb0...");
                if let Ok(mut fb) = aris_render::FbDevBackend::open("/dev/fb0") {
                    eprintln!("fb0: {}x{}", fb.resolution().0, fb.resolution().1);
                    if let Err(e) = fb.present(&frame) {
                        eprintln!("Present error: {}", e);
                    } else {
                        eprintln!("Presented to /dev/fb0 OK");
                    }
                }
            }

            match frame.save_ppm(&out_path) {
                Ok(()) => eprintln!("Saved: {}", out_path),
                Err(e) => eprintln!("Save error: {}", e),
            }

            // Exit non-zero if the render is essentially blank, so CI/verifiers
            // can detect a broken pipeline.
            if pct < 10 {
                eprintln!(
                    "FAIL: non-black pixel ratio {}% is below 10% threshold",
                    pct
                );
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("Render error: {:?}", e);
            std::process::exit(1);
        }
    }
}
