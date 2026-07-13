// kei_desktop — aris-render Windows-style desktop UI via /dev/fb0.
//
// Renders a Windows-like desktop environment to the kei framebuffer:
//   * "什亭之匣" day-mode wallpaper (sampled from shittim-chest bg.webp:
//     light cyan sky #b8f7f8 fading to soft blue #e9f1fc)
//   * Desktop icons (top-left grid: aris browser, files, terminal)
//   * Bottom taskbar with Start button + clock + system tray
//   * A Start menu popup panel
//   * A window titled "aris · kei" with content
//
// This is the aris-render fbdev display path (the "aris core" for pixel
// output) — the visual layout mirrors what the tairitsu/hikari component
// stack would render once the full Vello/Blitz pipeline runs on kei.
//
// IMPORTANT: avoids tracing-subscriber init (triggers musl malloc init that
// hangs on kei). Uses raw libc::write for stderr output. Writes the fb
// row-by-row to avoid the large-write hang in the kei fb write_at path.
//
// Resolution: 800x600 (BGRX = 4 bytes/pixel).

#![allow(clippy::many_single_char_names)]

use std::io::{Seek, Write};

// BGRX pixel (bytes in framebuffer memory: B, G, R, X).
type Bgrx = [u8; 4];

fn bgrx(b: u8, g: u8, r: u8) -> Bgrx {
    [b, g, r, 0xFF]
}

const W: usize = 640;
const H: usize = 480;
const BPP: usize = 4;

// --- Wallpaper palette (sampled from shittim-chest bg.webp day mode) ---
// Vertical gradient bands sampled at every 10% of the 4496px-tall source.
// We reconstruct a smooth vertical gradient by lerping between these stops.
const WALL_STOPS: &[(f32, [u8; 3])] = &[
    (0.00, [0xB8, 0xF7, 0xF8]), // top — light cyan
    (0.10, [0xB4, 0xF0, 0xFC]),
    (0.20, [0xD7, 0xFF, 0xFF]),
    (0.30, [0xE8, 0xFF, 0xFE]),
    (0.40, [0xBD, 0xFD, 0xFE]),
    (0.50, [0xEE, 0xFE, 0xFD]),
    (0.60, [0xE4, 0xFD, 0xFE]),
    (0.70, [0xF8, 0xFF, 0xFD]),
    (0.80, [0xF1, 0xFC, 0xFF]),
    (0.90, [0xFA, 0xFB, 0xF6]),
    (1.00, [0xE9, 0xF1, 0xFC]), // bottom — soft blue
];

/// Sample the wallpaper gradient at vertical fraction `t` (0.0=top..=1.0=bottom).
/// Returns RGB.
fn wallpaper_at(t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let mut prev = WALL_STOPS[0];
    for &(stop_t, stop_c) in WALL_STOPS {
        if t <= stop_t {
            let span = (stop_t - prev.0).max(1e-6);
            let f = ((t - prev.0) / span).clamp(0.0, 1.0);
            let lerp =
                |a: u8, b: u8| -> u8 { (a as f32 + (b as f32 - a as f32) * f).round() as u8 };
            return [
                lerp(prev.1[0], stop_c[0]),
                lerp(prev.1[1], stop_c[1]),
                lerp(prev.1[2], stop_c[2]),
            ];
        }
        prev = (stop_t, stop_c);
    }
    WALL_STOPS.last().unwrap().1
}

fn main() {
    // Avoid tracing-subscriber (musl malloc init hangs on kei). Raw write(2).
    let log = |m: &[u8]| unsafe {
        libc::write(2, m.as_ptr() as *const _, m.len() as _);
    };
    log(b"kei_desktop: starting Windows-style desktop UI (aris-render fbdev)\n");

    #[cfg(unix)]
    {
        // Allow overriding the fb path via env (host testing / alt devices).
        let fb_path_owned = std::env::var("KEI_FB").unwrap_or_else(|_| "/dev/fb0".to_string());
        let fb_path = fb_path_owned.as_str();
        if !std::path::Path::new(fb_path).exists() {
            let m = b"kei_desktop: fb device not found\n";
            log(m);
            return;
        }

        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(fb_path)
        {
            Ok(f) => f,
            Err(_) => {
                log(b"kei_desktop: fb0 open failed\n");
                return;
            }
        };

        let fb_size = W * H * BPP;
        log(b"kei_desktop: building 800x600 framebuffer\n");
        let mut buf = vec![0u8; fb_size];

        // ---- 1. Wallpaper gradient (full screen) ----
        for y in 0..H {
            let t = y as f32 / (H - 1) as f32;
            let [r, g, b] = wallpaper_at(t);
            let c = bgrx(b, g, r);
            let row_start = y * W * BPP;
            for x in 0..W {
                let idx = row_start + x * BPP;
                buf[idx..idx + 4].copy_from_slice(&c);
            }
        }

        // Helper closures operate on the raw buffer.
        let fill = |buf: &mut [u8], x0: usize, y0: usize, fw: usize, fh: usize, c: Bgrx| {
            let x1 = (x0 + fw).min(W);
            let y1 = (y0 + fh).min(H);
            for y in y0..y1 {
                let row = y * W * BPP;
                for x in x0..x1 {
                    let idx = row + x * BPP;
                    buf[idx..idx + 4].copy_from_slice(&c);
                }
            }
        };
        // Rectangle outline (1px border).
        let rect_outline = |buf: &mut [u8], x0: usize, y0: usize, fw: usize, fh: usize, c: Bgrx| {
            let x1 = (x0 + fw).min(W);
            let y1 = (y0 + fh).min(H);
            for x in x0..x1 {
                for &y in &[y0, y1.saturating_sub(1)] {
                    let idx = y * W * BPP + x * BPP;
                    if idx + 4 <= buf.len() {
                        buf[idx..idx + 4].copy_from_slice(&c);
                    }
                }
            }
            for y in y0..y1 {
                for &x in &[x0, x1.saturating_sub(1)] {
                    let idx = y * W * BPP + x * BPP;
                    if idx + 4 <= buf.len() {
                        buf[idx..idx + 4].copy_from_slice(&c);
                    }
                }
            }
        };
        // Filled disc (circle) for icons.
        let fill_disc = |buf: &mut [u8], cx: usize, cy: usize, radius: usize, c: Bgrx| {
            let r2 = (radius * radius) as isize;
            for dy in -(radius as isize)..=(radius as isize) {
                for dx in -(radius as isize)..=(radius as isize) {
                    if dx * dx + dy * dy <= r2 {
                        let x = cx.wrapping_add(dx as usize);
                        let y = cy.wrapping_add(dy as usize);
                        if x < W && y < H {
                            let idx = y * W * BPP + x * BPP;
                            if idx + 4 <= buf.len() {
                                buf[idx..idx + 4].copy_from_slice(&c);
                            }
                        }
                    }
                }
            }
        };
        // Filled rounded rectangle (approx: rect + corner discs).
        let fill_round =
            |buf: &mut [u8], x0: usize, y0: usize, fw: usize, fh: usize, rad: usize, c: Bgrx| {
                fill(buf, x0 + rad, y0, fw - 2 * rad, fh, c);
                fill(buf, x0, y0 + rad, fw, fh - 2 * rad, c);
                let corners = [
                    (x0 + rad, y0 + rad),
                    (x0 + fw - rad - 1, y0 + rad),
                    (x0 + rad, y0 + fh - rad - 1),
                    (x0 + fw - rad - 1, y0 + fh - rad - 1),
                ];
                for &(cx, cy) in &corners {
                    fill_disc(buf, cx, cy, rad, c);
                }
            };

        // ---- Palette ----
        // Taskbar / Start (Windows-10-like deep blue-grey).
        let taskbar = bgrx(0x2B, 0x2D, 0x31); // #312D2B dark
        let taskbar_hi = bgrx(0x4A, 0x4D, 0x52);
        let start_btn = bgrx(0x1F, 0x78, 0x32); // green Start (Windows flag-like)
        let start_btn_hi = bgrx(0x3E, 0xA0, 0x4E);
        let start_menu_bg = bgrx(0xF3, 0xF3, 0xF3); // light grey panel
        let start_menu_border = bgrx(0xCC, 0xCC, 0xCC);
        let start_menu_accent = bgrx(0x16, 0x76, 0x00); // green accent strip
        let text_dark = bgrx(0x22, 0x22, 0x22);
        let text_light = bgrx(0xFF, 0xFF, 0xFF);
        let text_grey = bgrx(0x88, 0x88, 0x88);
        let clock_bg = bgrx(0x55, 0x57, 0x5B);
        let tray_bg = bgrx(0x3A, 0x3D, 0x41);
        let window_bg = bgrx(0xFF, 0xFF, 0xFF);
        let window_title = bgrx(0xE6, 0xEE, 0xF7); // pale blue title bar
        let window_title_text = bgrx(0x1A, 0x3A, 0x6E);
        let window_border = bgrx(0x9C, 0xB4, 0xD9);
        let _icon_blue = bgrx(0xE0, 0xA0, 0x2E); // amber-ish folder (reserved)
        let icon_folder = bgrx(0xE6, 0xC2, 0x4A);
        let icon_term = bgrx(0x1E, 0x1E, 0x1E);
        let icon_term_text = bgrx(0x4C, 0xDC, 0x4C);
        let icon_browse = bgrx(0x36, 0x84, 0xE0); // browser blue
        let accent_blue = bgrx(0xCC, 0x7A, 0x10);

        // ---- 2. Desktop icons (top-left 4x2 grid) ----
        // Each icon: 48x48 colored tile + 2px gap + label area.
        let icon_col_x = [24, 88, 24, 88];
        let icon_row_y = [20, 20, 92, 92];
        let icon_colors = [icon_browse, icon_folder, icon_term, accent_blue];
        for i in 0..4 {
            let x0 = icon_col_x[i];
            let y0 = icon_row_y[i];
            // icon body (rounded)
            fill_round(&mut buf, x0, y0, 48, 48, 8, icon_colors[i]);
            // highlight stripe
            fill(&mut buf, x0 + 6, y0 + 6, 36, 6, bgrx(0xFF, 0xFF, 0xFF));
            // label underline box
            fill_round(&mut buf, x0 - 2, y0 + 52, 52, 14, 3, bgrx(0x00, 0x66, 0xCC));
            // tiny white "document" mark inside (different per icon)
            match i {
                0 => {
                    // browser: globe rings
                    fill_disc(&mut buf, x0 + 24, y0 + 26, 10, text_light);
                    fill_disc(&mut buf, x0 + 24, y0 + 26, 6, icon_browse);
                }
                1 => {
                    // folder: paper sheet
                    fill(&mut buf, x0 + 10, y0 + 16, 28, 22, text_light);
                    fill(&mut buf, x0 + 10, y0 + 16, 28, 4, icon_folder);
                }
                2 => {
                    // terminal: prompt chevron + cursor
                    fill(&mut buf, x0 + 10, y0 + 18, 28, 20, icon_term);
                    // ">" chevron dots
                    for &(dx, dy) in &[(14, 22), (17, 25), (14, 28), (20, 22), (23, 25), (20, 28)] {
                        fill(&mut buf, x0 + dx, y0 + dy, 2, 2, icon_term_text);
                    }
                    fill(&mut buf, x0 + 28, y0 + 30, 6, 2, icon_term_text);
                }
                _ => {
                    // settings: gear-ish dots ring
                    fill_disc(&mut buf, x0 + 24, y0 + 26, 12, text_light);
                    fill_disc(&mut buf, x0 + 24, y0 + 26, 5, accent_blue);
                }
            }
        }

        // ---- 3. An "aris · kei" window centered on the desktop ----
        let win_w = 340usize;
        let win_h = 200usize;
        let win_x = (W - win_w) / 2 + 60; // shifted right to avoid start menu overlap
        let win_y = 80;
        // Drop shadow (offset dark rect)
        fill_round(
            &mut buf,
            win_x + 4,
            win_y + 4,
            win_w,
            win_h,
            6,
            bgrx(0x10, 0x20, 0x30),
        );
        // Window body
        fill_round(&mut buf, win_x, win_y, win_w, win_h, 6, window_bg);
        // Title bar
        fill(&mut buf, win_x + 6, win_y + 2, win_w - 12, 26, window_title);
        rect_outline(&mut buf, win_x, win_y, win_w, win_h, window_border);
        // Title text — block letters spelling "aris"
        draw_text(
            &mut buf,
            "aris - kei",
            win_x + 12,
            win_y + 8,
            window_title_text,
        );
        // Window control buttons (min/max/close) on the right
        let ctl_y = win_y + 8;
        let mut ctl_x = win_x + win_w - 14;
        for &(sym, col) in &[
            ('_', text_grey),
            ('O', text_grey),
            ('X', bgrx(0xCC, 0x55, 0x55)),
        ] {
            fill(&mut buf, ctl_x - 2, ctl_y - 2, 14, 14, window_bg);
            rect_outline(&mut buf, ctl_x - 2, ctl_y - 2, 14, 14, window_border);
            draw_glyph(&mut buf, sym, ctl_x, ctl_y, col);
            ctl_x = ctl_x.saturating_sub(16);
        }
        // Window content lines (a fake "browser content")
        let cy = win_y + 42;
        fill(&mut buf, win_x + 16, cy, win_w - 32, 22, window_title); // address bar
        draw_text(
            &mut buf,
            "aris://desktop",
            win_x + 24,
            cy + 6,
            window_title_text,
        );
        // content paragraphs (grey bars)
        let mut ly = cy + 34;
        let content_lines: [(usize, Bgrx); 4] = [
            (win_w - 60, bgrx(0xCC, 0xCC, 0xCC)),
            (win_w - 90, bgrx(0xD6, 0xD6, 0xD6)),
            (win_w - 70, bgrx(0xCC, 0xCC, 0xCC)),
            (win_w - 110, bgrx(0xD6, 0xD6, 0xD6)),
        ];
        for (i, (wlen, shade)) in content_lines.iter().enumerate() {
            fill(&mut buf, win_x + 20, ly, *wlen, 6, *shade);
            ly += 14;
            if i >= 2 {
                break;
            }
        }
        // a small colored "screenshot" tile inside the window
        fill(&mut buf, win_x + 20, ly + 4, 80, 50, window_title);
        fill(&mut buf, win_x + 28, ly + 12, 64, 8, bgrx(0x9C, 0xB4, 0xD9));
        fill(&mut buf, win_x + 28, ly + 24, 48, 8, bgrx(0xCC, 0x7A, 0x10));

        // ---- 4. Start menu (popped up above the Start button) ----
        let sm_w = 240usize;
        let sm_h = 280usize;
        let sm_x = 0usize;
        let sm_y = (H - 40).saturating_sub(sm_h); // sits above taskbar, left-aligned
        // Shadow
        fill_round(
            &mut buf,
            sm_x + 3,
            sm_y + 3,
            sm_w,
            sm_h,
            4,
            bgrx(0x20, 0x20, 0x20),
        );
        // Panel
        fill_round(&mut buf, sm_x, sm_y, sm_w, sm_h, 4, start_menu_bg);
        rect_outline(&mut buf, sm_x, sm_y, sm_w, sm_h, start_menu_border);
        // Left accent strip (green, Windows-10-like)
        fill(&mut buf, sm_x + 4, sm_y + 4, 6, sm_h - 8, start_menu_accent);
        // "aris" wordmark at top
        draw_text_big(&mut buf, "aris", sm_x + 22, sm_y + 16, text_dark);
        draw_text(&mut buf, "kei desktop", sm_x + 22, sm_y + 40, text_grey);
        // Search box
        fill_round(
            &mut buf,
            sm_x + 22,
            sm_y + 60,
            sm_w - 44,
            22,
            3,
            bgrx(0xFF, 0xFF, 0xFF),
        );
        rect_outline(
            &mut buf,
            sm_x + 22,
            sm_y + 60,
            sm_w - 44,
            22,
            start_menu_border,
        );
        draw_text(
            &mut buf,
            "Type here to search",
            sm_x + 30,
            sm_y + 66,
            text_grey,
        );
        // App tiles (2 columns x 3 rows)
        let apps = [
            ("Browser", icon_browse),
            ("Files", icon_folder),
            ("Terminal", icon_term),
            ("Settings", accent_blue),
            ("aris", bgrx(0x7A, 0x4A, 0xC0)),
            ("tairitsu", bgrx(0xC0, 0x4A, 0x7A)),
        ];
        let tile_w = (sm_w - 44 - 8) / 2;
        let tile_h = 44;
        for (i, (name, col)) in apps.iter().enumerate() {
            let tx = sm_x + 22 + (i % 2) * (tile_w + 8);
            let ty = sm_y + 92 + (i / 2) * (tile_h + 8);
            fill_round(&mut buf, tx, ty, tile_w, tile_h, 3, bgrx(0xEA, 0xEA, 0xEA));
            // icon swatch
            fill_round(&mut buf, tx + 4, ty + 6, 32, 32, 4, *col);
            // app name (truncate to fit)
            let nm: String = name.chars().take(8).collect();
            draw_text(&mut buf, &nm, tx + 42, ty + 18, text_dark);
        }
        // Power button at bottom-right of menu
        let pwr_x = sm_x + sm_w - 28;
        let pwr_y = sm_y + sm_h - 24;
        fill_disc(&mut buf, pwr_x, pwr_y, 8, bgrx(0xCC, 0x55, 0x55));
        fill_disc(&mut buf, pwr_x, pwr_y, 4, start_menu_bg);
        // power stem
        for yy in 0..6 {
            let idx = (pwr_y - 6 + yy) * W * BPP + pwr_x * BPP;
            if idx + 4 <= buf.len() {
                buf[idx..idx + 4].copy_from_slice(&bgrx(0xCC, 0x55, 0x55));
            }
        }

        // ---- 5. Taskbar (bottom strip, full width) ----
        let tb_h = 40usize;
        let tb_y = H - tb_h;
        fill(&mut buf, 0, tb_y, W, tb_h, taskbar);
        // top highlight line on the taskbar
        fill(&mut buf, 0, tb_y, W, 1, taskbar_hi);
        // Start button (green rounded square, left)
        fill_round(&mut buf, 4, tb_y + 4, 56, 32, 4, start_btn);
        // 4-pane Windows-flag-like glyph inside the Start button
        let sx = 4 + 14;
        let sy = tb_y + 4 + 9;
        fill(&mut buf, sx, sy, 11, 11, text_light);
        fill(&mut buf, sx + 13, sy, 11, 11, text_light);
        fill(&mut buf, sx, sy + 13, 11, 11, text_light);
        fill(&mut buf, sx + 13, sy + 13, 11, 11, text_light);
        // Start label
        draw_text(&mut buf, "Start", 4 + 64, tb_y + 14, text_light);

        // Pinned/running app tiles on the taskbar
        let pinned = [
            (icon_browse, "Browser"),
            (icon_folder, "Files"),
            (icon_term, "Terminal"),
        ];
        let mut px = 120usize;
        for (col, _name) in &pinned {
            // tile background
            fill(&mut buf, px, tb_y + 6, 36, 28, bgrx(0x3A, 0x3D, 0x41));
            // running indicator underline
            fill(&mut buf, px, tb_y + 34, 36, 2, start_btn_hi);
            fill_round(&mut buf, px + 4, tb_y + 10, 28, 20, 3, *col);
            px += 44;
        }

        // System tray (right side): a few indicators + clock
        let tray_w = 180usize;
        let tray_x = W - tray_w;
        fill(&mut buf, tray_x, tb_y, tray_w, tb_h, tray_bg);
        // tray icons (3 small dots/squares)
        for i in 0..3 {
            let ix = tray_x + 12 + i * 22;
            fill_round(&mut buf, ix, tb_y + 12, 16, 16, 3, clock_bg);
            fill_disc(&mut buf, ix + 8, tb_y + 20, 4, text_light);
        }
        // clock area
        let clk_x = tray_x + 90;
        fill(&mut buf, clk_x, tb_y + 6, tray_w - 96, 28, clock_bg);
        // clock text: "12:00" (block digits)
        draw_clock(&mut buf, clk_x + 8, tb_y + 12, text_light);
        // date underneath clock (tiny)
        draw_text_small(&mut buf, "2026-07-13", clk_x + 8, tb_y + 26, text_grey);

        // ---- 6. Write framebuffer to /dev/fb0 ----
        // Write in moderate chunks, then trigger a SINGLE flush via the
        // FBIOPAN_DISPLAY ioctl. The kernel fb write_at path does NOT flush on
        // each write (to avoid a deterministic ostd page-table crash under
        // repeated flushes). This single ioctl-triggered flush pushes the whole
        // frame to the virtio-gpu scanout in one TRANSFER_TO_HOST_2D.
        log(b"kei_desktop: writing framebuffer to /dev/fb0\n");
        let _ = file.seek(std::io::SeekFrom::Start(0));
        const CHUNK: usize = 8192;
        let mut written = 0usize;
        while written < fb_size {
            let end = (written + CHUNK).min(fb_size);
            let n = file.write(&buf[written..end]).unwrap_or(0);
            if n == 0 {
                break;
            }
            written += n;
        }
        let m = format!("kei_desktop: wrote {} of {} bytes\n", written, fb_size);
        log(m.as_bytes());

        // Trigger the single flush: FBIOPAN_DISPLAY = 0x4606 (kei hijacks this
        // ioctl to flush the Blit-backed framebuffer to the scanout).
        log(b"kei_desktop: triggering flush (FBIOPAN_DISPLAY)\n");
        #[cfg(unix)]
        unsafe {
            // ioctl(fd, FBIOPAN_DISPLAY, 0) — no argument data needed.
            const FBIOPAN_DISPLAY: u64 = 0x4606;
            let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
            let _ = libc::ioctl(fd, FBIOPAN_DISPLAY as _, 0usize);
        }
        drop(file);
        log(b"kei_desktop: done - desktop rendered.\n");
    }

    // Keep the process alive so the framebuffer stays visible.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

// ---------------------------------------------------------------------------
// Minimal 5x7 bitmap font (ASCII printable subset). Each glyph is 5 cols wide,
// 7 rows tall, stored as 7 bytes (MSB of each byte = leftmost pixel). This
// keeps the binary dependency-free and matches the kei fb pixel path.
// ---------------------------------------------------------------------------

const FONT_5X7: &[(&str, [u8; 7])] = &[
    (" ", [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    ("!", [0x00, 0x00, 0x5F, 0x00, 0x00, 0x00, 0x00]),
    ("\"", [0x00, 0x07, 0x00, 0x07, 0x00, 0x00, 0x00]),
    ("#", [0x14, 0x7F, 0x14, 0x7F, 0x14, 0x00, 0x00]),
    ("(", [0x00, 0x1C, 0x22, 0x41, 0x00, 0x00, 0x00]),
    (")", [0x00, 0x41, 0x22, 0x1C, 0x00, 0x00, 0x00]),
    ("*", [0x08, 0x2A, 0x1C, 0x2A, 0x08, 0x00, 0x00]),
    ("+", [0x08, 0x08, 0x3E, 0x08, 0x08, 0x00, 0x00]),
    (",", [0x00, 0x50, 0x30, 0x00, 0x00, 0x00, 0x00]),
    ("-", [0x08, 0x08, 0x08, 0x08, 0x08, 0x00, 0x00]),
    (".", [0x00, 0x60, 0x60, 0x00, 0x00, 0x00, 0x00]),
    ("/", [0x20, 0x10, 0x08, 0x04, 0x02, 0x00, 0x00]),
    ("0", [0x3E, 0x51, 0x49, 0x45, 0x3E, 0x00, 0x00]),
    ("1", [0x00, 0x42, 0x7F, 0x40, 0x00, 0x00, 0x00]),
    ("2", [0x42, 0x61, 0x51, 0x49, 0x46, 0x00, 0x00]),
    ("3", [0x21, 0x41, 0x45, 0x4B, 0x31, 0x00, 0x00]),
    ("4", [0x18, 0x14, 0x12, 0x7F, 0x10, 0x00, 0x00]),
    ("5", [0x27, 0x45, 0x45, 0x45, 0x39, 0x00, 0x00]),
    ("6", [0x3C, 0x4A, 0x49, 0x49, 0x30, 0x00, 0x00]),
    ("7", [0x01, 0x71, 0x09, 0x05, 0x03, 0x00, 0x00]),
    ("8", [0x36, 0x49, 0x49, 0x49, 0x36, 0x00, 0x00]),
    ("9", [0x06, 0x49, 0x49, 0x29, 0x1E, 0x00, 0x00]),
    (":", [0x00, 0x36, 0x36, 0x00, 0x00, 0x00, 0x00]),
    (";", [0x00, 0x56, 0x36, 0x00, 0x00, 0x00, 0x00]),
    ("<", [0x00, 0x08, 0x14, 0x22, 0x41, 0x00, 0x00]),
    ("=", [0x14, 0x14, 0x14, 0x14, 0x14, 0x00, 0x00]),
    (">", [0x41, 0x22, 0x14, 0x08, 0x00, 0x00, 0x00]),
    ("?", [0x02, 0x01, 0x51, 0x09, 0x06, 0x00, 0x00]),
    ("@", [0x32, 0x49, 0x79, 0x41, 0x3E, 0x00, 0x00]),
    ("A", [0x7E, 0x11, 0x11, 0x11, 0x7E, 0x00, 0x00]),
    ("B", [0x7F, 0x49, 0x49, 0x49, 0x36, 0x00, 0x00]),
    ("C", [0x3E, 0x41, 0x41, 0x41, 0x22, 0x00, 0x00]),
    ("D", [0x7F, 0x41, 0x41, 0x22, 0x1C, 0x00, 0x00]),
    ("E", [0x7F, 0x49, 0x49, 0x49, 0x41, 0x00, 0x00]),
    ("F", [0x7F, 0x09, 0x09, 0x01, 0x01, 0x00, 0x00]),
    ("G", [0x3E, 0x41, 0x41, 0x51, 0x32, 0x00, 0x00]),
    ("H", [0x7F, 0x08, 0x08, 0x08, 0x7F, 0x00, 0x00]),
    ("I", [0x00, 0x41, 0x7F, 0x41, 0x00, 0x00, 0x00]),
    ("J", [0x20, 0x40, 0x41, 0x3F, 0x01, 0x00, 0x00]),
    ("K", [0x7F, 0x08, 0x14, 0x22, 0x41, 0x00, 0x00]),
    ("L", [0x7F, 0x40, 0x40, 0x40, 0x40, 0x00, 0x00]),
    ("M", [0x7F, 0x02, 0x04, 0x02, 0x7F, 0x00, 0x00]),
    ("N", [0x7F, 0x04, 0x08, 0x10, 0x7F, 0x00, 0x00]),
    ("O", [0x3E, 0x41, 0x41, 0x41, 0x3E, 0x00, 0x00]),
    ("P", [0x7F, 0x09, 0x09, 0x09, 0x06, 0x00, 0x00]),
    ("Q", [0x3E, 0x41, 0x51, 0x21, 0x5E, 0x00, 0x00]),
    ("R", [0x7F, 0x09, 0x19, 0x29, 0x46, 0x00, 0x00]),
    ("S", [0x46, 0x49, 0x49, 0x49, 0x31, 0x00, 0x00]),
    ("T", [0x01, 0x01, 0x7F, 0x01, 0x01, 0x00, 0x00]),
    ("U", [0x3F, 0x40, 0x40, 0x40, 0x3F, 0x00, 0x00]),
    ("V", [0x1F, 0x20, 0x40, 0x20, 0x1F, 0x00, 0x00]),
    ("W", [0x7F, 0x20, 0x18, 0x20, 0x7F, 0x00, 0x00]),
    ("X", [0x63, 0x14, 0x08, 0x14, 0x63, 0x00, 0x00]),
    ("Y", [0x03, 0x04, 0x78, 0x04, 0x03, 0x00, 0x00]),
    ("Z", [0x61, 0x51, 0x49, 0x45, 0x43, 0x00, 0x00]),
    ("[", [0x00, 0x7F, 0x41, 0x41, 0x00, 0x00, 0x00]),
    ("]", [0x00, 0x41, 0x41, 0x7F, 0x00, 0x00, 0x00]),
    ("_", [0x40, 0x40, 0x40, 0x40, 0x40, 0x00, 0x00]),
    ("a", [0x20, 0x54, 0x54, 0x54, 0x78, 0x00, 0x00]),
    ("b", [0x7F, 0x48, 0x44, 0x44, 0x38, 0x00, 0x00]),
    ("c", [0x38, 0x44, 0x44, 0x44, 0x20, 0x00, 0x00]),
    ("d", [0x38, 0x44, 0x44, 0x48, 0x7F, 0x00, 0x00]),
    ("e", [0x38, 0x54, 0x54, 0x54, 0x18, 0x00, 0x00]),
    ("f", [0x08, 0x7E, 0x09, 0x01, 0x02, 0x00, 0x00]),
    ("g", [0x08, 0x14, 0x54, 0x54, 0x54, 0x3C, 0x00]),
    ("h", [0x7F, 0x08, 0x04, 0x04, 0x78, 0x00, 0x00]),
    ("i", [0x00, 0x44, 0x7D, 0x40, 0x00, 0x00, 0x00]),
    ("j", [0x20, 0x40, 0x44, 0x3D, 0x00, 0x00, 0x00]),
    ("k", [0x7F, 0x10, 0x28, 0x44, 0x00, 0x00, 0x00]),
    ("l", [0x00, 0x41, 0x7F, 0x40, 0x00, 0x00, 0x00]),
    ("m", [0x7C, 0x04, 0x18, 0x04, 0x78, 0x00, 0x00]),
    ("n", [0x7C, 0x08, 0x04, 0x04, 0x78, 0x00, 0x00]),
    ("o", [0x38, 0x44, 0x44, 0x44, 0x38, 0x00, 0x00]),
    ("p", [0x7C, 0x14, 0x14, 0x14, 0x08, 0x00, 0x00]),
    ("q", [0x08, 0x14, 0x14, 0x18, 0x7C, 0x00, 0x00]),
    ("r", [0x7C, 0x08, 0x04, 0x04, 0x08, 0x00, 0x00]),
    ("s", [0x48, 0x54, 0x54, 0x54, 0x20, 0x00, 0x00]),
    ("t", [0x04, 0x3F, 0x44, 0x40, 0x20, 0x00, 0x00]),
    ("u", [0x3C, 0x40, 0x40, 0x20, 0x7C, 0x00, 0x00]),
    ("v", [0x1C, 0x20, 0x40, 0x20, 0x1C, 0x00, 0x00]),
    ("w", [0x3C, 0x40, 0x30, 0x40, 0x3C, 0x00, 0x00]),
    ("x", [0x44, 0x28, 0x10, 0x28, 0x44, 0x00, 0x00]),
    ("y", [0x0C, 0x50, 0x50, 0x50, 0x3C, 0x00, 0x00]),
    ("z", [0x44, 0x64, 0x54, 0x4C, 0x44, 0x00, 0x00]),
];

fn glyph_rows(c: char) -> Option<[u8; 7]> {
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    for (ch, rows) in FONT_5X7 {
        if *ch == s {
            return Some(*rows);
        }
    }
    // Uppercase fallback for letters whose lowercase form is in the table.
    if c.is_ascii_uppercase() {
        let lc = c.to_ascii_lowercase();
        let mut buf = [0u8; 4];
        let s = lc.encode_utf8(&mut buf);
        for (ch, rows) in FONT_5X7 {
            if *ch == s {
                return Some(*rows);
            }
        }
    }
    None
}

fn put_pixel(buf: &mut [u8], x: usize, y: usize, c: Bgrx) {
    if x < W && y < H {
        let idx = y * W * BPP + x * BPP;
        if idx + 4 <= buf.len() {
            buf[idx..idx + 4].copy_from_slice(&c);
        }
    }
}

/// Draw text at (x0,y0) using the 5x7 font, 1px spacing between chars.
fn draw_text(buf: &mut [u8], text: &str, x0: usize, y0: usize, c: Bgrx) {
    let mut x = x0;
    for ch in text.chars() {
        if let Some(rows) = glyph_rows(ch) {
            for (ry, byte) in rows.iter().enumerate() {
                for col in 0..5 {
                    if (byte << col) & 0x80 != 0 {
                        put_pixel(buf, x + col, y0 + ry, c);
                    }
                }
            }
            x += 6;
        } else {
            x += 6;
        }
    }
}

/// Smaller text (skips odd rows) for tight spaces like the date line.
fn draw_text_small(buf: &mut [u8], text: &str, x0: usize, y0: usize, c: Bgrx) {
    let mut x = x0;
    for ch in text.chars() {
        if let Some(rows) = glyph_rows(ch) {
            for (ry, byte) in rows.iter().enumerate() {
                if ry % 2 == 0 {
                    for col in 0..5 {
                        if (byte << col) & 0x80 != 0 {
                            put_pixel(buf, x + col, y0 + ry / 2, c);
                        }
                    }
                }
            }
            x += 6;
        } else {
            x += 6;
        }
    }
}

/// Larger title text: 2x horizontal scale (10px-wide glyphs).
fn draw_text_big(buf: &mut [u8], text: &str, x0: usize, y0: usize, c: Bgrx) {
    let mut x = x0;
    for ch in text.chars() {
        if let Some(rows) = glyph_rows(ch) {
            for (ry, byte) in rows.iter().enumerate() {
                for col in 0..5 {
                    if (byte << col) & 0x80 != 0 {
                        put_pixel(buf, x + col * 2, y0 + ry, c);
                        put_pixel(buf, x + col * 2 + 1, y0 + ry, c);
                    }
                }
            }
            x += 12;
        } else {
            x += 12;
        }
    }
}

/// Single glyph for window control buttons (minimize/maximize/close).
fn draw_glyph(buf: &mut [u8], sym: char, x0: usize, y0: usize, c: Bgrx) {
    match sym {
        '_' => {
            for dx in 0..6 {
                put_pixel(buf, x0 + dx, y0 + 5, c);
            }
        }
        'O' => {
            for dx in 1..5 {
                put_pixel(buf, x0 + dx, y0, c);
                put_pixel(buf, x0 + dx, y0 + 5, c);
            }
            for dy in 1..5 {
                put_pixel(buf, x0, y0 + dy, c);
                put_pixel(buf, x0 + 5, y0 + dy, c);
            }
        }
        'X' => {
            for d in 0..6 {
                put_pixel(buf, x0 + d, y0 + d, c);
                put_pixel(buf, x0 + 5 - d, y0 + d, c);
            }
        }
        _ => {}
    }
}

/// Draw a fixed "12:00" clock using 3x-scaled digits for the taskbar clock.
fn draw_clock(buf: &mut [u8], x0: usize, y0: usize, c: Bgrx) {
    let digits = ['1', '2', ':', '0', '0'];
    let mut x = x0;
    for d in digits {
        if let Some(rows) = glyph_rows(d) {
            for (ry, byte) in rows.iter().enumerate() {
                for col in 0..5 {
                    if (byte << col) & 0x80 != 0 {
                        for dx in 0..2 {
                            for dy in 0..2 {
                                put_pixel(buf, x + col * 2 + dx, y0 + ry * 2 + dy, c);
                            }
                        }
                    }
                }
            }
            x += 12;
        } else {
            x += 12;
        }
    }
}
