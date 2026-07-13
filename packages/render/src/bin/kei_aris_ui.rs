// kei_aris_ui — aris-render powered desktop UI without Vello.
//
// Uses aris_render::Frame + aris_render::FbDevBackend to present a
// browser-style UI to /dev/fb0. This exercises the aris-render fbdev
// pipeline (the "aris core" for display output) without depending on
// the Vello CPU rasterizer, which has a NULL-deref incompatibility
// with musl/kei.
//
// The UI is drawn programmatically (header bar, address bar, cards)
// into an aris_render::Frame, then presented via FbDevBackend::present
// which converts RGBA to BGRX and writes to the framebuffer.
fn main() {
    aris_render::init_logging();
    tracing::info!("kei_aris_ui: starting (aris-render fbdev path, no Vello)");

    let width = 640u32;
    let height = 480u32;

    // Build the frame using aris_render::Frame (the aris core pixel buffer)
    let mut frame = aris_render::Frame::new(width, height);
    tracing::info!("frame allocated: {}x{}", width, height);

    // Colors as RGBA bytes (aris Frame uses RGBA)
    let bg = [0x28, 0x2C, 0x34, 0xFF]; // dark background
    let header = [0x61, 0xAF, 0xEF, 0xFF]; // blue header
    let card = [0x21, 0x25, 0x2B, 0xFF]; // card bg
    let accent = [0xE0, 0x6C, 0x75, 0xFF]; // red
    let green = [0x98, 0xC3, 0x79, 0xFF]; // green
    let text = [0xAB, 0xB2, 0xBF, 0xFF]; // light text
    let white = [0xFF, 0xFF, 0xFF, 0xFF];

    let put = |frame: &mut [u8], w: u32, x: u32, y: u32, c: [u8; 4]| {
        let idx = ((y * w + x) * 4) as usize;
        if idx + 3 < frame.len() {
            frame[idx..idx + 4].copy_from_slice(&c);
        }
    };
    let fill =
        |frame: &mut [u8], w: u32, h: u32, x0: u32, y0: u32, fw: u32, fh: u32, c: [u8; 4]| {
            for y in y0..(y0 + fh).min(h) {
                for x in x0..(x0 + fw).min(w) {
                    put(frame, w, x, y, c);
                }
            }
        };

    // Background
    fill(&mut frame.rgba, width, height, 0, 0, width, height, bg);
    // Header bar
    fill(&mut frame.rgba, width, height, 0, 0, width, 50, header);
    // Address bar
    fill(
        &mut frame.rgba,
        width,
        height,
        10,
        58,
        width - 20,
        28,
        [0x1B, 0x1F, 0x23, 0xFF],
    );
    // Cards
    fill(
        &mut frame.rgba,
        width,
        height,
        20,
        100,
        width - 40,
        80,
        card,
    );
    fill(
        &mut frame.rgba,
        width,
        height,
        20,
        195,
        width - 40,
        80,
        card,
    );
    fill(
        &mut frame.rgba,
        width,
        height,
        20,
        290,
        width - 40,
        80,
        card,
    );

    // Simple text-like patterns (colored dots representing text)
    // "KEI BROWSER" title area — white dots
    for x in 20..300 {
        for y in 15..40 {
            if (x % 8 < 4) && (y % 8 < 4) {
                put(&mut frame.rgba, width, x, y, white);
            }
        }
    }
    // Status indicators
    for x in 30..200 {
        put(&mut frame.rgba, width, x, 130, green);
        put(&mut frame.rgba, width, x, 132, green);
    }
    for x in 30..150 {
        put(&mut frame.rgba, width, x, 225, accent);
        put(&mut frame.rgba, width, x, 227, accent);
    }
    for x in 30..250 {
        put(&mut frame.rgba, width, x, 320, text);
        put(&mut frame.rgba, width, x, 322, text);
    }

    tracing::info!("frame rendered, presenting to fb0...");

    // Present via aris-render FbDevBackend (the aris core fbdev path)
    let fb_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/fb0".to_string());
    match aris_render::FbDevBackend::open(&fb_path) {
        Ok(mut fb) => {
            let (fw, fh) = fb.resolution();
            tracing::info!("fbdev: {}x{}", fw, fh);
            match fb.present(&frame) {
                Ok(()) => tracing::info!("presented to {} OK", fb_path),
                Err(e) => tracing::error!("present error: {}", e),
            }
        }
        Err(e) => tracing::error!("fb open error: {}", e),
    }

    tracing::info!("kei_aris_ui: done. Keeping alive.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
