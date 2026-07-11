// kei_fbtest — direct framebuffer write test (no Vello/Blitz).
// Renders a simple pattern directly to /dev/fb0 to verify the display pipeline.
// Uses raw file I/O — no ioctl, no mmap — to avoid kei kernel fbdev bugs.
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
        let mut file = match std::fs::OpenOptions::new().read(true).write(true).open(fb_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[kei_fbtest] open error: {}", e);
                return;
            }
        };

        let width = 1280u32;
        let height = 800u32;
        eprintln!("[kei_fbtest] writing {}x{} pattern...", width, height);

        // Build BGRX pixel data directly (kei framebuffer format).
        // Bytes in memory: B, G, R, X for each pixel.
        let mut bgrx = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                if y < 60 {
                    // Blue header bar (#61AFEF)
                    bgrx[idx]     = 0xEF; // B
                    bgrx[idx + 1] = 0xAF; // G
                    bgrx[idx + 2] = 0x61; // R
                    bgrx[idx + 3] = 0xFF; // X
                } else if x < 400 {
                    // Left panel: dark gray (#282C34)
                    bgrx[idx]     = 0x34;
                    bgrx[idx + 1] = 0x2C;
                    bgrx[idx + 2] = 0x28;
                    bgrx[idx + 3] = 0xFF;
                } else if x < 800 {
                    // Center: green-ish (#98C379)
                    bgrx[idx]     = 0x79;
                    bgrx[idx + 1] = 0xC3;
                    bgrx[idx + 2] = 0x98;
                    bgrx[idx + 3] = 0xFF;
                } else {
                    // Right: red-ish (#E06C75)
                    bgrx[idx]     = 0x75;
                    bgrx[idx + 1] = 0x6C;
                    bgrx[idx + 2] = 0xE0;
                    bgrx[idx + 3] = 0xFF;
                }
            }
        }

        eprintln!("[kei_fbtest] writing {} bytes in chunks...", bgrx.len());
        use std::io::{Seek, Write};
        match file.seek(std::io::SeekFrom::Start(0)) {
            Ok(_) => {}
            Err(e) => { eprintln!("[kei_fbtest] seek error: {}", e); return; }
        }
        // Write in 256KB chunks to balance kernel allocation and syscall count.
        let chunk_size = 262144usize;
        let mut total_written = 0usize;
        for chunk in bgrx.chunks(chunk_size) {
            match file.write_all(chunk) {
                Ok(()) => total_written += chunk.len(),
                Err(e) => {
                    eprintln!("[kei_fbtest] write error at offset {}: {}", total_written, e);
                    break;
                }
            }
        }
        eprintln!("[kei_fbtest] Wrote {} bytes to fb.", total_written);

        // Trigger a single framebuffer flush by writing to offset 0 again
        // with a special marker byte sequence, then calling fsync.
        // The kernel fbdev driver's flush is triggered by fsync/msync.
        // Actually, let's use the FBIOPAN_DISPLAY ioctl or just write 1 byte
        // at offset 0 to trigger a single flush.
        // For now, the kernel write_at doesn't flush. QEMU screendump will
        // show the DMA buffer only after a TRANSFER_TO_HOST_2D.
        eprintln!("[kei_fbtest] attempting fsync to flush display...");
        let _ = file.sync_all();
        eprintln!("[kei_fbtest] fsync done.");
    }

    eprintln!("[kei_fbtest] done.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
