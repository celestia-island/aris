// SPDX-License-Identifier: BUSL-1.1

//! aris-render — HTML/CSS rendering pipeline for the aris HMI.
//!
//! Provides a pure-Rust rendering stack using Blitz (Stylo + Taffy + Parley)
//! and Vello CPU for software rasterization to a pixel buffer. The buffer
//! can be pushed to `/dev/fb0` on kei/Linux or saved as a file for testing.
//!
//! ## Architecture
//!
//! ```text
//! HTML string → blitz-dom (parse + CSS cascade + layout)
//!            → blitz-renderer-vello (Vello CPU rasterize to RGBA buffer)
//!            → fbdev backend (mmap /dev/fb0) or file output
//! ```

#![forbid(unsafe_code)]

use alloc::string::String;
use alloc::vec::Vec;

extern crate alloc;

pub mod fbdev;

pub use fbdev::FbDevBackend;

/// Configuration for the rendering pipeline.
#[derive(Debug, Clone)]
pub struct RenderConfig {
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Scale factor (1.0 = no scaling).
    pub scale: f32,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
            scale: 1.0,
        }
    }
}

/// A rendered frame as an RGBA pixel buffer.
#[derive(Debug)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel data in RGBA format (4 bytes per pixel, row-major).
    pub rgba: Vec<u8>,
}

impl Frame {
    /// Creates a new empty (black) frame.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; (width * height * 4) as usize],
        }
    }

    /// Returns the raw pixel data as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.rgba
    }

    /// Returns the raw pixel data as a mutable byte slice.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.rgba
    }

    /// Saves the frame as a PPM (P6) file for testing/debugging.
    pub fn save_ppm(&self, path: &str) -> anyhow::Result<()> {
        use std::fs::File;
        use std::io::Write;

        let mut file = File::create(path)?;
        writeln!(file, "P6")?;
        writeln!(file, "{} {}", self.width, self.height)?;
        writeln!(file, "255")?;

        // Convert RGBA → RGB for PPM
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
        for chunk in self.rgba.chunks_exact(4) {
            rgb.push(chunk[0]); // R
            rgb.push(chunk[1]); // G
            rgb.push(chunk[2]); // B
        }
        file.write_all(&rgb)?;
        Ok(())
    }
}

/// Renders an HTML string into a pixel buffer using the Blitz + Vello CPU pipeline.
///
/// This is the high-level entry point: parse HTML, apply CSS cascade via Stylo,
/// compute layout via Taffy, rasterize via Vello CPU — all in pure Rust with
/// no GPU/DRM dependency.
///
/// # Arguments
///
/// * `html` — The HTML document to render.
/// * `config` — Rendering configuration (viewport size, scale).
///
/// # Returns
///
/// A `Frame` containing the rendered RGBA pixel data.
///
/// # Note
///
/// This function is a placeholder that produces a solid-color test pattern.
/// The full Blitz integration (DOM parse → Stylo cascade → Taffy layout →
/// Vello CPU rasterize) requires the blitz-dom and blitz-renderer-vello crates
/// to be published on crates.io with compatible APIs. Until then, the test
/// pattern verifies the fbdev output path end-to-end.
pub fn render_html(html: &str, config: &RenderConfig) -> anyhow::Result<Frame> {
    // TODO: Full Blitz pipeline integration:
    // 1. blitz_dom::Document::from_html(html)
    // 2. Apply CSS cascade (Stylo)
    // 3. Compute layout (Taffy)
    // 4. Rasterize via blitz_renderer_vello::render_to_buffer(&mut frame.rgba)
    //
    // For now, generate a test pattern that visualizes the viewport:
    let mut frame = Frame::new(config.width, config.height);
    generate_test_pattern(&mut frame, html);
    Ok(frame)
}

/// Generates a diagnostic test pattern when full HTML rendering is not yet wired.
///
/// The pattern shows:
/// - A colored border (green) to confirm pixel output works
/// - A diagonal gradient to show the full viewport is covered
fn generate_test_pattern(frame: &mut Frame, _html: &str) {
    let w = frame.width;
    let h = frame.height;

    for y in 0..h {
        for x in 0..w {
            let idx = ((y * w + x) * 4) as usize;
            let on_border = x < 8 || y < 8 || x >= w - 8 || y >= h - 8;

            if on_border {
                // Green border (matches kei's GPU test pattern)
                frame.rgba[idx] = 0; // R
                frame.rgba[idx + 1] = 255; // G
                frame.rgba[idx + 2] = 0; // B
                frame.rgba[idx + 3] = 255; // A
            } else {
                // Blue gradient with red diagonal (One Half Dark inspired)
                let blue = ((x ^ y) & 0xFF) as u8;
                let red = (((x + y) >> 1) & 0x7F) as u8;
                frame.rgba[idx] = red;
                frame.rgba[idx + 1] = 0x2C;
                frame.rgba[idx + 2] = blue;
                frame.rgba[idx + 3] = 255;
            }
        }
    }
}
