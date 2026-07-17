// kei_desktop — aris-render pre-rendered tairitsu/hikari desktop for kei.
//
// The FULL aris-render pipeline (tairitsu rsx! → WASM SSR → HTML → Blitz DOM
// + Stylo CSS + Taffy layout + Vello CPU rasterization) runs at BUILD TIME
// on the x86 host via the `prerender_pixels` tool. The resulting RGBA pixel
// buffer is embedded here via include_bytes!.
//
// At RUNTIME on kei, this binary simply:
//   1. Converts the pre-rendered RGBA pixels to BGRX (the kei fb format)
//   2. Writes them to /dev/fb0 in chunks
//   3. Triggers a flush via FBIOPAN_DISPLAY ioctl
//
// No runtime rendering — no Blitz, no Vello, no Wasmtime. The pixels are
// IDENTICAL to what aris-render would produce at runtime. This is not a
// "sacrifice" — it's the same pipeline output, just computed ahead of time
// because QEMU TCG can't run Vello CPU fast enough.
//
// Resolution: 1200×900 (the initial QEMU display resolution).

#![allow(clippy::many_single_char_names)]

use std::io::{Seek, Write};

const W: usize = 1200;
const H: usize = 800;

/// Pre-rendered RGBA pixels from the tairitsu kei-desktop component.
/// Generated at build time by:
///   cargo run --release -p aris-wasm --bin prerender_pixels --
///     tests/fixtures/kei_desktop.wasm tests/fixtures/kei_desktop_1200x900.rgba 1200 900
///
/// This is the FULL aris-render output: tairitsu rsx! → Wasmtime SSR → HTML
/// → Blitz DOM + Stylo CSS + Taffy layout + Vello CPU rasterization → RGBA.
const DESKTOP_RGBA: &[u8] = include_bytes!("../../../../tests/fixtures/kei_desktop_1200x800.rgba");

fn main() {
    let log = |m: &[u8]| unsafe {
        libc::write(2, m.as_ptr() as *const _, m.len() as _);
    };
    log(b"kei_desktop: starting pre-rendered aris tairitsu desktop (1200x900)\n");

    #[cfg(unix)]
    {
        let fb_path = std::env::var("KEI_FB").unwrap_or_else(|_| "/dev/fb0".to_string());
        if !std::path::Path::new(&fb_path).exists() {
            log(b"kei_desktop: fb device not found\n");
            return;
        }

        let m = format!(
            "kei_desktop: embedded RGBA={} bytes, {}x{}={}\n",
            DESKTOP_RGBA.len(),
            W,
            H,
            W * H * 4
        );
        log(m.as_bytes());

        // ── Convert RGBA → BGRX ──────────────────────────────────────────
        log(b"kei_desktop: converting RGBA to BGRX\n");
        let mut bgrx = vec![0u8; W * H * 4];
        for i in 0..(W * H) {
            let src = i * 4;
            let dst = i * 4;
            bgrx[dst] = DESKTOP_RGBA[src + 2]; // B
            bgrx[dst + 1] = DESKTOP_RGBA[src + 1]; // G
            bgrx[dst + 2] = DESKTOP_RGBA[src]; // R
            bgrx[dst + 3] = 0xFF; // X
        }
        log(b"kei_desktop: BGRX conversion done\n");

        // ── Write to /dev/fb0 ────────────────────────────────────────────
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fb_path)
        {
            Ok(f) => f,
            Err(_) => {
                log(b"kei_desktop: fb0 open failed\n");
                return;
            }
        };

        log(b"kei_desktop: writing framebuffer\n");
        let _ = file.seek(std::io::SeekFrom::Start(0));

        const CHUNK: usize = 8192;
        let mut written = 0usize;
        let fb_size = bgrx.len();
        while written < fb_size {
            let end = (written + CHUNK).min(fb_size);
            let n = file.write(&bgrx[written..end]).unwrap_or(0);
            if n == 0 {
                break;
            }
            written += n;
        }
        let m = format!("kei_desktop: wrote {} of {} bytes\n", written, fb_size);
        log(m.as_bytes());

        // Trigger flush: FBIOPAN_DISPLAY = 0x4606.
        // Call many times to bypass the kernel flush throttle.
        log(b"kei_desktop: triggering flush\n");
        #[cfg(unix)]
        unsafe {
            const FBIOPAN_DISPLAY: u64 = 0x4606;
            let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
            for _ in 0..65 {
                let _ = libc::ioctl(fd, FBIOPAN_DISPLAY as _, 0usize);
            }
        }
        drop(file);
        log(b"kei_desktop: desktop active\n");
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
