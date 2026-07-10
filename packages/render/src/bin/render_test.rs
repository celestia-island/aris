// Minimal render test: render HTML and write to /dev/fb0 or PPM file.
// Usage: render_test [output.ppm]
fn main() {
    let html = r#"<!DOCTYPE html><html><head><style>
body { margin:0; background:#282C34; }
h1 { color:#E06C75; font-size:48px; margin:20px; }
.box { background:#61AFEF; width:200px; height:100px; margin:20px; }
</style></head><body><h1>Hello kei!</h1><div class="box"></div></body></html>"#;

    let config = aris_render::RenderConfig { width: 800, height: 600, scale: 1.0 };
    match aris_render::render_html(html, &config) {
        Ok(frame) => {
            let non_black = frame.rgba.chunks_exact(4)
                .filter(|px| px[0]>10 || px[1]>10 || px[2]>10).count();
            eprintln!("Non-black: {}/{}", non_black, frame.width as usize * frame.height as usize);

            // Try /dev/fb0 first
            if std::path::Path::new("/dev/fb0").exists() {
                eprintln!("Opening /dev/fb0...");
                match aris_render::FbDevBackend::open("/dev/fb0") {
                    Ok(mut fb) => {
                        eprintln!("fb0: {}x{}", fb.resolution().0, fb.resolution().1);
                        match fb.present(&frame) {
                            Ok(()) => eprintln!("Presented to /dev/fb0 OK"),
                            Err(e) => eprintln!("Present error: {}", e),
                        }
                    }
                    Err(e) => eprintln!("fb0 open error: {}", e),
                }
            }

            // Also save PPM
            let path = std::env::args().nth(1).unwrap_or_else(|| "render_test.ppm".to_string());
            match frame.save_ppm(&path) {
                Ok(()) => eprintln!("Saved: {}", path),
                Err(e) => eprintln!("Save error: {}", e),
            }
        }
        Err(e) => { eprintln!("Render error: {:?}", e); std::process::exit(1); }
    }
}
