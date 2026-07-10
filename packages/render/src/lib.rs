// SPDX-License-Identifier: BUSL-1.1

//! aris-render — HTML/CSS rendering pipeline for the aris HMI.

#![allow(unsafe_code)]
#![allow(dead_code)]

use alloc::vec::Vec;

extern crate alloc;

#[cfg(unix)]
pub mod fbdev;
#[cfg(unix)]
pub use fbdev::FbDevBackend;

#[cfg(feature = "winit")]
pub mod winit_backend;

use anyrender::ImageRenderer;

/// Configuration for the rendering pipeline.
#[derive(Debug, Clone)]
pub struct RenderConfig {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self { width: 1280, height: 800, scale: 1.0 }
    }
}

/// A rendered frame as an RGBA pixel buffer.
#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, rgba: vec![0; (width * height * 4) as usize] }
    }

    pub fn save_ppm(&self, path: &str) -> anyhow::Result<()> {
        use std::fs::File;
        use std::io::Write;
        let mut file = File::create(path)?;
        writeln!(file, "P6")?;
        writeln!(file, "{} {}", self.width, self.height)?;
        writeln!(file, "255")?;
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
        for chunk in self.rgba.chunks_exact(4) {
            rgb.push(chunk[0]);
            rgb.push(chunk[1]);
            rgb.push(chunk[2]);
        }
        file.write_all(&rgb)?;
        Ok(())
    }
}

/// Renders an HTML string into a pixel buffer using Blitz + Vello CPU.
pub fn render_html(html: &str, config: &RenderConfig) -> anyhow::Result<Frame> {
    let width = config.width;
    let height = config.height;
    let scale = config.scale as f64;

    // Use blitz-html's HtmlDocument to parse HTML properly
    use blitz_html::HtmlDocument;
    use blitz_dom::DocumentConfig;
    use blitz_traits::shell::Viewport;

    let viewport = Viewport {
        window_size: (width, height),
        hidpi_scale: config.scale,
        ..Default::default()
    };

    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        ..Default::default()
    };

    // HtmlDocument::from_html handles full HTML parsing (html5ever) + DOM construction
    let mut doc = HtmlDocument::from_html(html, doc_config);

    // Resolve styles (Stylo CSS cascade) and compute layout (Taffy)
    doc.resolve(0.0);

    // Paint to anyrender scene, then rasterize via Vello CPU
    let mut frame = Frame::new(width, height);
    let mut renderer = anyrender_vello_cpu::VelloCpuImageRenderer::new(width, height);
    renderer.render(
        |scene| {
            blitz_paint::paint_scene(scene, &mut doc, scale, width, height, 0, 0);
        },
        &mut frame.rgba,
    );

    Ok(frame)
}
