// kei_fbtest — direct framebuffer write test (no Vello/Blitz).
// Renders a simple pattern directly to /dev/fb0 to verify the display pipeline.
fn main() {
    eprintln!("[kei_fbtest] starting...");

    #[cfg(unix)]
    {
        let fb_path = "/dev/fb0";
        if !std::path::Path::new(fb_path).exists() {
            eprintln!("[kei_fbtest] {} not found!", fb_path);
            return;
        }

        eprintln!("[kei_fbtest] opening {}...", fb_path);
        let mut fb = match aris_render::FbDevBackend::open(fb_path) {
            Ok(fb) => {
                let (w, h) = fb.resolution();
                eprintln!("[kei_fbtest] fb: {}x{}", w, h);
                fb
            }
            Err(e) => {
                eprintln!("[kei_fbtest] open error: {}", e);
                return;
            }
        };

        // Create a simple test frame: gradient + colored blocks
        let width = 1280u32;
        let height = 800u32;
        let mut rgba = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                // Top bar: blue (#61AFEF)
                if y < 60 {
                    rgba[idx]     = 0x61; // R
                    rgba[idx + 1] = 0xAF; // G
                    rgba[idx + 2] = 0xEF; // B
                    rgba[idx + 3] = 0xFF;
                }
                // Background gradient
                else {
                    let r = (x * 255 / width) as u8;
                    let g = (y * 255 / height) as u8;
                    let b = 0x34;
                    rgba[idx]     = r;
                    rgba[idx + 1] = g;
                    rgba[idx + 2] = b;
                    rgba[idx + 3] = 0xFF;
                }
            }
        }

        let frame = aris_render::Frame { width, height, rgba };
        eprintln!("[kei_fbtest] writing to framebuffer...");
        match fb.present(&frame) {
            Ok(()) => eprintln!("[kei_fbtest] OK! Framebuffer updated."),
            Err(e) => eprintln!("[kei_fbtest] present error: {}", e),
        }
    }

    eprintln!("[kei_fbtest] done.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
