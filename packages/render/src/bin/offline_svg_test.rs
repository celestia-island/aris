// Offline SVG rendering test.
//
// Creates an HTML page with an inline SVG data URI as an <img> source, renders
// it via the aris pipeline, and checks for non-black pixels in the image area.
//
//   cargo run -p aris-render --features "desktop" --bin offline_svg_test

fn main() {
    aris_render::init_logging();
    // A simple SVG: red circle on white background.
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
      <rect width="100" height="100" fill="white"/>
      <circle cx="50" cy="50" r="40" fill="red"/>
    </svg>"#;
    let svg_b64 = base64_encode(svg.as_bytes());
    let data_uri = format!("data:image/svg+xml;base64,{}", svg_b64);
    let html = format!(
        "<!DOCTYPE html><html><head><style>body{{margin:0;background:#000;}}</style></head>\
         <body><img src=\"{}\" width=\"100\" height=\"100\"></body></html>",
        data_uri
    );
    let config = aris_render::RenderConfig {
        width: 200,
        height: 200,
        scale: 1.0,
    };
    match aris_render::render_html(&html, &config) {
        Ok(frame) => {
            // Check for red pixels (the circle).
            let red = frame
                .rgba
                .chunks_exact(4)
                .filter(|px| px[0] > 150 && px[1] < 80 && px[2] < 80)
                .count();
            let white = frame
                .rgba
                .chunks_exact(4)
                .filter(|px| px[0] > 200 && px[1] > 200 && px[2] > 200)
                .count();
            println!("red pixels: {} (SVG circle)", red);
            println!("white pixels: {} (SVG background)", white);
            if red > 0 {
                println!("OK: SVG image rendered with {} red pixels", red);
            } else {
                println!("WARN: no red pixels — SVG may not render in this blitz version");
            }
        }
        Err(e) => {
            println!("error: {:?}", e);
            std::process::exit(1);
        }
    }
}

/// Simple base64 encoder (avoids pulling in a base64 crate).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 63) as usize] as char);
        out.push(CHARS[((triple >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
