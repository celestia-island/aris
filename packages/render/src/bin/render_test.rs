// SPDX-License-Identifier: BUSL-1.1

//! Test binary: renders a simple HTML page and saves as PPM.
//!
//! Run: cargo run --bin render_test
//! Output: render_test_output.ppm

use aris_render::{render_html, RenderConfig};

fn main() {
    let html = r#"<!DOCTYPE html>
<html>
<head><style>
body { margin: 0; background: #282C34; }
h1 { color: #E06C75; font-size: 48px; margin: 20px; }
p { color: #DCDFE4; font-size: 20px; margin: 20px; }
.box { background: #61AFEF; width: 200px; height: 100px; margin: 20px; }
</style></head>
<body>
<h1>Hello from aris-render!</h1>
<p>This page was rendered by Blitz + Vello CPU.</p>
<div class="box"></div>
</body>
</html>"#;

    println!("Rendering HTML ({}) bytes...", html.len());

    let config = RenderConfig {
        width: 800,
        height: 600,
        scale: 1.0,
    };

    match render_html(html, &config) {
        Ok(frame) => {
            // Count non-black pixels
            let non_black = frame.rgba.chunks_exact(4)
                .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
                .count();
            let total = (frame.width * frame.height) as usize;
            let pct = if total > 0 { non_black * 100 / total } else { 0 };

            println!("Frame: {}x{} ({} bytes)", frame.width, frame.height, frame.rgba.len());
            println!("Non-black pixels: {}/{} ({}%)", non_black, total, pct);

            // Check for specific colors (One Half Dark background #282C34)
            let bg_match = frame.rgba.chunks_exact(4)
                .filter(|px| px[0] >= 0x28 && px[0] <= 0x38 && px[2] >= 0x28 && px[2] <= 0x3A)
                .count();
            println!("Background-like pixels: {}", bg_match);

            // Check for red text (#E06C75)
            let red_match = frame.rgba.chunks_exact(4)
                .filter(|px| px[0] > 180 && px[1] < 130 && px[2] < 130)
                .count();
            println!("Red-like pixels (text): {}", red_match);

            // Check for blue box (#61AFEF)
            let blue_match = frame.rgba.chunks_exact(4)
                .filter(|px| px[2] > 180 && px[0] < 130)
                .count();
            println!("Blue-like pixels (box): {}", blue_match);

            // Save PPM
            match frame.save_ppm("render_test_output.ppm") {
                Ok(()) => println!("Saved: render_test_output.ppm"),
                Err(e) => eprintln!("Save failed: {}", e),
            }

            // Verify rendering is non-trivial
            if non_black < 10 {
                eprintln!("FAIL: Frame is essentially all black");
                std::process::exit(1);
            }
            println!("PASS: Frame contains rendered content");
        }
        Err(e) => {
            eprintln!("Render failed: {:?}", e);
            std::process::exit(1);
        }
    }
}
