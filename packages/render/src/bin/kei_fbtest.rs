// kei_fbtest — direct framebuffer write test (no Vello/Blitz).
// Writes a simple pattern to /dev/fb0 as fast as possible.
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

        let width = 1280usize;
        let height = 800usize;
        let row_bytes = width * 4;

        // Build ONE row of each color, then write it repeatedly.
        // This is much faster than per-pixel computation on QEMU TCG.
        eprintln!("[kei_fbtest] building row patterns...");
        let blue_row: Vec<u8> = (0..width).flat_map(|_| [0xEFu8, 0xAF, 0x61, 0xFF]).collect();
        let dark_row: Vec<u8> = (0..width).flat_map(|_| [0x34u8, 0x2C, 0x28, 0xFF]).collect();
        let green_row: Vec<u8> = (0..width).flat_map(|_| [0x79u8, 0xC3, 0x98, 0xFF]).collect();
        let red_row: Vec<u8> = (0..width).flat_map(|_| [0x75u8, 0x6C, 0xE0, 0xFF]).collect();

        eprintln!("[kei_fbtest] writing rows...");
        use std::io::{Seek, Write};
        let _ = file.seek(std::io::SeekFrom::Start(0));

        let mut total = 0usize;
        for y in 0..height {
            // Split screen into thirds
            let row = if y < 60 {
                &blue_row
            } else {
                // Write 1/3 of each color per row
                let third = width / 3;
                // Seek to position and write each third
                let _ = file.seek(std::io::SeekFrom::Start((y * row_bytes) as u64));
                let _ = file.write_all(&dark_row[..third * 4]);
                total += third * 4;
                let _ = file.write_all(&green_row[..third * 4]);
                total += third * 4;
                let _ = file.write_all(&red_row[..(width - third * 2) * 4]);
                total += (width - third * 2) * 4;
                continue;
            };
            let _ = file.write_all(row);
            total += row_bytes;
            if y % 100 == 0 {
                eprintln!("[kei_fbtest] row {}/{}", y, height);
            }
        }

        eprintln!("[kei_fbtest] Wrote {} bytes.", total);
    }

    eprintln!("[kei_fbtest] done.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
