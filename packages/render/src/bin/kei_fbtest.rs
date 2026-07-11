// kei_fbtest — direct framebuffer test via mmap (no per-write syscalls).
// mmaps /dev/fb0 and writes pixels directly to the DMA buffer.
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
        let file = match std::fs::OpenOptions::new().read(true).write(true).open(fb_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[kei_fbtest] open error: {}", e);
                return;
            }
        };

        let width = 1280usize;
        let height = 800usize;
        let fb_size = width * height * 4;

        // Try mmap
        eprintln!("[kei_fbtest] attempting mmap ({} bytes)...", fb_size);
        use std::os::fd::AsRawFd;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                fb_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            eprintln!("[kei_fbtest] mmap failed, falling back to write()");
            // Fallback: just write a single blue row
            use std::io::{Seek, Write};
            let mut file = file;
            let _ = file.seek(std::io::SeekFrom::Start(0));
            let blue_row: Vec<u8> = (0..width).flat_map(|_| [0xEFu8, 0xAF, 0x61, 0xFF]).collect();
            for _ in 0..height {
                let _ = file.write_all(&blue_row);
            }
            eprintln!("[kei_fbtest] wrote via fallback");
        } else {
            eprintln!("[kei_fbtest] mmap OK at {:p}, drawing pattern...", ptr);

            // Write directly to the mmap'd DMA buffer — no per-pixel syscalls!
            let fb = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, fb_size) };

            // Blue header (first 60 rows)
            let blue_pixel = [0xEFu8, 0xAF, 0x61, 0xFF];
            let third = width / 3;
            for y in 0..height {
                for x in 0..width {
                    let idx = (y * width + x) * 4;
                    if y < 60 {
                        fb[idx..idx+4].copy_from_slice(&blue_pixel);
                    } else if x < third {
                        fb[idx] = 0x34; fb[idx+1] = 0x2C; fb[idx+2] = 0x28; fb[idx+3] = 0xFF;
                    } else if x < third * 2 {
                        fb[idx] = 0x79; fb[idx+1] = 0xC3; fb[idx+2] = 0x98; fb[idx+3] = 0xFF;
                    } else {
                        fb[idx] = 0x75; fb[idx+1] = 0x6C; fb[idx+2] = 0xE0; fb[idx+3] = 0xFF;
                    }
                }
                if y % 200 == 0 {
                    eprintln!("[kei_fbtest] drew row {}/{}", y, height);
                }
            }

            eprintln!("[kei_fbtest] pattern drawn, msync...");
            unsafe { libc::msync(ptr, fb_size, libc::MS_SYNC); }
            eprintln!("[kei_fbtest] msync done");
        }
    }

    eprintln!("[kei_fbtest] done.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
