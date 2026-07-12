// kei_fbtest — aris-render browser UI via /dev/fb0 write path.
//
// This binary is part of the aris-render crate and uses aris_render's
// init_logging. It draws a browser-style desktop UI (blue header, address
// bar, info cards) by writing BGRX pixels to /dev/fb0, exercising the
// aris-render fbdev display pipeline on kei.
fn main() {
    aris_render::init_logging();
    tracing::info!("starting browser UI...");

    #[cfg(unix)]
    {
        let fb_path = "/dev/fb0";
        if !std::path::Path::new(fb_path).exists() {
            tracing::info!("{} not found!", fb_path);
            return;
        }

        tracing::info!("opening {}...", fb_path);
        let mut file = match std::fs::OpenOptions::new().read(true).write(true).open(fb_path) {
            Ok(f) => f,
            Err(e) => {
                tracing::info!("open error: {}", e);
                return;
            }
        };

        let width = 640usize;
        let height = 480usize;
        let bpp = 4usize;
        let fb_size = width * height * bpp;

        tracing::info!("building UI buffer ({}x{})...", width, height);
        let mut buf = vec![0u8; fb_size];

        // BGRX colors (bytes: B, G, R, X) for kei virtio-gpu
        let header = [0xEFu8, 0xAF, 0x61, 0xFF]; // #61AFEF blue
        let bg = [0x34u8, 0x2C, 0x28, 0xFF];     // #282C34 dark
        let card = [0x2Bu8, 0x25, 0x21, 0xFF];   // #21252B card
        let addrbg = [0x23u8, 0x1F, 0x1B, 0xFF]; // address bar
        let white = [0xFFu8, 0xFF, 0xFF, 0xFF];
        let green = [0x79u8, 0xC3, 0x98, 0xFF];  // #98C379
        let accent = [0x75u8, 0x6C, 0xE0, 0xFF]; // #E06C75
        let text_c = [0xBFu8, 0xB2, 0xAB, 0xFF]; // #ABB2BF

        for y in 0..height {
            for x in 0..width {
                let c = if y < 50 { header }
                    else if y >= 58 && y < 86 { addrbg }
                    else if (y >= 100 && y < 180) || (y >= 195 && y < 275) || (y >= 290 && y < 370) { card }
                    else { bg };
                let idx = (y * width + x) * 4;
                buf[idx..idx+4].copy_from_slice(&c);
            }
        }

        // Title dots
        for x in 20..280 {
            for y in 18..38 {
                if (x % 10 < 5) && (y % 8 < 4) {
                    let idx = (y * width + x) * 4;
                    buf[idx..idx+4].copy_from_slice(&white);
                }
            }
        }
        // Indicator lines
        let draw_line = |buf: &mut [u8], y: usize, x0: usize, x1: usize, c: [u8;4]| {
            for x in x0..x1.min(width) {
                let idx = (y * width + x) * 4;
                buf[idx..idx+4].copy_from_slice(&c);
            }
        };
        draw_line(&mut buf, 130, 30, 200, green);
        draw_line(&mut buf, 132, 30, 200, green);
        draw_line(&mut buf, 150, 30, 250, text_c);
        draw_line(&mut buf, 152, 30, 250, text_c);
        draw_line(&mut buf, 225, 30, 150, accent);
        draw_line(&mut buf, 227, 30, 150, accent);
        draw_line(&mut buf, 247, 30, 250, text_c);
        draw_line(&mut buf, 249, 30, 250, text_c);
        draw_line(&mut buf, 320, 30, 250, text_c);
        draw_line(&mut buf, 322, 30, 250, text_c);
        draw_line(&mut buf, 342, 30, 250, text_c);
        draw_line(&mut buf, 344, 30, 250, text_c);
        draw_line(&mut buf, 400, 20, 280, text_c);
        draw_line(&mut buf, 402, 20, 280, text_c);
        draw_line(&mut buf, 425, 20, 200, accent);
        draw_line(&mut buf, 427, 20, 200, accent);

        tracing::info!("UI built, writing to fb0...");
        use std::io::Write;
        match file.write_all(&buf) {
            Ok(()) => tracing::info!("wrote {} bytes to fb0 OK", fb_size),
            Err(e) => tracing::info!("write error: {}", e),
        }

        tracing::info!("done.");
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
