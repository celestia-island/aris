// kei_ui — aris-render HTML browser UI for kei OS.
// Uses Blitz DOM + Vello CPU to render HTML, then writes to /dev/fb0.
// Avoids tracing-subscriber init (musl hang) — uses libc::write instead.
fn main() {
    let msg = b"kei_ui: starting\n";
    unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len() as _); }

    let html = r#"<!DOCTYPE html><html><head><style>
* { margin: 0; padding: 0; box-sizing: border-box; }
body { background: #282C34; color: #ABB2BF; height: 100vh; }
.header { background: #61AFEF; height: 60px; padding: 0 24px; }
.header h1 { color: white; font-size: 24px; }
.content { padding: 24px; }
.card { background: #21252B; border-radius: 8px; padding: 20px; margin-bottom: 16px; }
.card h2 { color: #61AFEF; font-size: 20px; }
.stat { background: #E06C75; color: white; padding: 8px 16px; }
</style></head><body>
<div class="header"><h1>kei OS</h1></div>
<div class="content">
<div class="card"><h2>System Status</h2><p>aris-render pipeline OK</p></div>
<div class="card"><h2>Resources</h2><span class="stat">CPU 12%</span></div>
</div>
</body></html>"#;

    let config = aris_render::RenderConfig {
        width: 640,
        height: 480,
        scale: 1.0,
    };

    let msg = b"kei_ui: rendering HTML\n";
    unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len() as _); }

    let frame = match aris_render::render_html(html, &config) {
        Ok(f) => f,
        Err(e) => {
            let m = format!("kei_ui: render error: {:?}\n", e);
            unsafe { libc::write(2, m.as_ptr() as *const _, m.len() as _); }
            std::process::exit(1);
        }
    };

    let total = (frame.width as usize) * (frame.height as usize);
    let non_black = frame.rgba.chunks_exact(4)
        .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
        .count();
    let msg = format!("kei_ui: rendered {}/{} non-black ({:.1}%)\n",
        non_black, total, 100.0 * non_black as f64 / total as f64);
    unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len() as _); }

    // Write to /dev/fb0 row-by-row (avoids large write hang)
    #[cfg(unix)]
    {
        let fb_path = std::env::args().nth(1).unwrap_or_else(|| "/dev/fb0".to_string());
        if std::path::Path::new(&fb_path).exists() {
            let msg = b"kei_ui: writing to fb0\n";
            unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len() as _); }

            if let Ok(mut fb) = std::fs::OpenOptions::new().read(true).write(true).open(&fb_path) {
                use std::io::{Seek, Write};
                let row_bytes = frame.width as usize * 4;
                for y in 0..frame.height as usize {
                    let _ = fb.seek(std::io::SeekFrom::Start((y * row_bytes) as u64));
                    // Convert RGBA row to BGRX
                    let mut bgrx_row = vec![0u8; row_bytes];
                    for x in 0..frame.width as usize {
                        let src = (y * frame.width as usize + x) * 4;
                        let dst = x * 4;
                        bgrx_row[dst] = frame.rgba[src + 2];   // B
                        bgrx_row[dst + 1] = frame.rgba[src + 1]; // G
                        bgrx_row[dst + 2] = frame.rgba[src];     // R
                        bgrx_row[dst + 3] = 0xFF;                // X
                    }
                    let _ = fb.write_all(&bgrx_row);
                }
                let msg = b"kei_ui: fb write done\n";
                unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len() as _); }
            }
        }
    }

    let msg = b"kei_ui: UI active\n";
    unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len() as _); }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
