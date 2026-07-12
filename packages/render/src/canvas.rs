// SPDX-License-Identifier: BUSL-1.1

//! Minimal Canvas 2D API backing store.
//!
//! Each `<canvas>` element gets a `Canvas2D` — an RGBA pixel buffer that JS
//! drawing commands (fillRect, fillText, clearRect, etc.) write into. The
//! rendered buffer is composited onto the page as an image overlay.
//!
//! This is NOT a full Canvas 2D implementation — it covers the most common
//! operations (fillRect, fillStyle, clearRect, simple fillText). Arcs, paths,
//  gradients, transforms, and pixel manipulation are future work.

#![cfg(feature = "js")]

/// A Canvas 2D backing store: RGBA pixels + current fill color.
#[derive(Clone)]
pub struct Canvas2D {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Current fill style as RGBA (default: black opaque).
    pub fill: [u8; 4],
}

impl Canvas2D {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; (width * height * 4) as usize],
            fill: [0, 0, 0, 255],
        }
    }

    /// Parse a CSS color string into RGBA. Supports #rgb, #rrggbb, and named
    /// colors (black, white, red, green, blue, etc.).
    pub fn parse_color(s: &str) -> [u8; 4] {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            match hex.len() {
                6 => {
                    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
                    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
                    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
                    [r, g, b, 255]
                }
                3 => {
                    let r = u8::from_str_radix(&format!("{}{}", &hex[0..1], &hex[0..1]), 16)
                        .unwrap_or(0);
                    let g = u8::from_str_radix(&format!("{}{}", &hex[1..2], &hex[1..2]), 16)
                        .unwrap_or(0);
                    let b = u8::from_str_radix(&format!("{}{}", &hex[2..3], &hex[2..3]), 16)
                        .unwrap_or(0);
                    [r, g, b, 255]
                }
                _ => [0, 0, 0, 255],
            }
        } else {
            match s.to_lowercase().as_str() {
                "black" => [0, 0, 0, 255],
                "white" => [255, 255, 255, 255],
                "red" => [255, 0, 0, 255],
                "green" => [0, 128, 0, 255],
                "blue" => [0, 0, 255, 255],
                "yellow" => [255, 255, 0, 255],
                "cyan" => [0, 255, 255, 255],
                "magenta" => [255, 0, 255, 255],
                "gray" | "grey" => [128, 128, 128, 255],
                "orange" => [255, 165, 0, 255],
                "purple" => [128, 0, 128, 255],
                "transparent" => [0, 0, 0, 0],
                _ => [0, 0, 0, 255],
            }
        }
    }

    /// Fill a rectangle with the current fill color.
    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let x0 = x.max(0.0) as u32;
        let y0 = y.max(0.0) as u32;
        let x1 = ((x + w).max(0.0) as u32).min(self.width);
        let y1 = ((y + h).max(0.0) as u32).min(self.height);
        for py in y0..y1 {
            for px in x0..x1 {
                let idx = ((py * self.width + px) * 4) as usize;
                if idx + 3 < self.rgba.len() {
                    let [r, g, b, a] = self.fill;
                    if a == 255 {
                        self.rgba[idx] = r;
                        self.rgba[idx + 1] = g;
                        self.rgba[idx + 2] = b;
                        self.rgba[idx + 3] = a;
                    } else if a > 0 {
                        // Alpha blend.
                        let dst = &self.rgba[idx..idx + 4];
                        let ar = a as u32;
                        let dr = dst[0] as u32 * (255 - ar) + r as u32 * ar;
                        let dg = dst[1] as u32 * (255 - ar) + g as u32 * ar;
                        let db = dst[2] as u32 * (255 - ar) + b as u32 * ar;
                        self.rgba[idx] = (dr / 255) as u8;
                        self.rgba[idx + 1] = (dg / 255) as u8;
                        self.rgba[idx + 2] = (db / 255) as u8;
                        self.rgba[idx + 3] = 255;
                    }
                }
            }
        }
    }

    /// Clear a rectangle to transparent.
    pub fn clear_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let x0 = x.max(0.0) as u32;
        let y0 = y.max(0.0) as u32;
        let x1 = ((x + w).max(0.0) as u32).min(self.width);
        let y1 = ((y + h).max(0.0) as u32).min(self.height);
        for py in y0..y1 {
            for px in x0..x1 {
                let idx = ((py * self.width + px) * 4) as usize;
                if idx + 3 < self.rgba.len() {
                    self.rgba[idx] = 0;
                    self.rgba[idx + 1] = 0;
                    self.rgba[idx + 2] = 0;
                    self.rgba[idx + 3] = 0;
                }
            }
        }
    }

    /// Set the fill style from a CSS color string.
    pub fn set_fill_style(&mut self, color: &str) {
        self.fill = Self::parse_color(color);
    }
}
