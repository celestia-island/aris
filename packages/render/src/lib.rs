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

#[cfg(feature = "winit")]
pub mod browser;

#[cfg(feature = "js")]
pub mod js_interactive;

#[cfg(feature = "js")]
pub mod js_runtime;

#[cfg(feature = "js")]
pub mod canvas;

#[cfg(feature = "render")]
use anyrender::ImageRenderer;

/// Embedded fallback font for headless/fbdev builds where `system_fonts` is off.
/// DejaVu Sans (latin-400) — SIL-compatible open-source license.
#[cfg(feature = "render")]
const EMBEDDED_FONT: &[u8] = include_bytes!("../assets/font.ttf");

/// Initialize structured logging with timestamps and levels.
///
/// Call this at the top of every binary's `main()`:
/// ```no_run
/// aris_render::init_logging();
/// ```
/// Output format: `2026-07-11T10:30:45.123Z  INFO aris_render::fbdev: message`
///
/// Control verbosity with `RUST_LOG=debug`, `RUST_LOG=aris_render=trace`, etc.
pub fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .init();
}

/// Configuration for the rendering pipeline.
#[derive(Debug, Clone)]
pub struct RenderConfig {
    pub width: u32,
    pub height: u32,
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
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; (width * height * 4) as usize],
        }
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
#[cfg(feature = "render")]
pub fn render_html(html: &str, config: &RenderConfig) -> anyhow::Result<Frame> {
    let width = config.width;
    let height = config.height;
    let scale = config.scale as f64;

    // Check if we should skip DOM/Vello entirely (kei fontique NULL workaround).
    // This must be checked BEFORE any fontique/parley/skrifa code runs.
    let skip_dom = std::env::var("KEI_NO_DOM").is_ok();
    if skip_dom {
        let mut frame = Frame::new(width, height);
        fill_fallback(&mut frame.rgba, width, height);
        return Ok(frame);
    }

    // Use blitz-html's HtmlDocument to parse HTML properly
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::shell::Viewport;

    // Create a FontContext with the embedded font registered.
    // Use Blob::from_vec to avoid Arc<dyn AsRef<[u8]>> vtable dispatch
    // (which produces NULL on kei's VM).
    
    use parley::FontContext;
    use parley::fontique::{Collection, CollectionOptions, SourceCache};

    let mut font_ctx = FontContext {
        source_cache: SourceCache::new_shared(),
        collection: Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        }),
    };
    let font_blob: parley::fontique::Blob<u8> =
        parley::fontique::Blob::new(std::sync::Arc::new(EMBEDDED_FONT.to_vec()));
    font_ctx.collection.register_fonts(font_blob, None);

    let viewport: Viewport = Viewport {
        window_size: (width, height),
        hidpi_scale: config.scale,
        ..Default::default()
    };

    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        font_ctx: Some(font_ctx),
        ..Default::default()
    };

    let doc: Option<HtmlDocument> = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Some(HtmlDocument::from_html(html, doc_config))
    }))
    .unwrap_or(None);

    // Resolve styles (Stylo CSS cascade) and compute layout (Taffy).
    // On kei, this triggers fontique/skrifa font metrics init which NULL-derefs.
    // Skip resolve and paint with raw DOM (no CSS cascade, no font metrics).
    // doc.resolve(0.0);
    // Instead, manually set basic layout on the root node:
    // We skip resolve entirely and let Vello paint whatever the DOM has.
    // Without resolve, elements have no computed style, so Vello paints
    // transparent backgrounds. We handle the painting manually below.

    // Paint to anyrender scene, then rasterize via Vello CPU.
    // Try calling resolve first — if it panics (fontique NULL on kei),
    // catch the panic and paint with raw DOM.
    // Actually, we skipped resolve above. Let's try paint_scene directly.
    // paint_scene may still call font code, but for simple divs without text
    // it should just paint colored rectangles.
    let _frame = Frame::new(width, height);
    let _renderer = anyrender_vello_cpu::VelloCpuImageRenderer::new(width, height);
    // On kei, doc.resolve() triggers fontique/skrifa font metrics init
    // which enters an infinite loop. Skip resolve and use fallback if
    // DOM creation also failed.
    let _resolve_ok = doc.is_some();

    // Paint to anyrender scene, then rasterize via Vello CPU
    let mut frame = Frame::new(width, height);
    let mut renderer = anyrender_vello_cpu::VelloCpuImageRenderer::new(width, height);
    if let Some(mut doc) = doc {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            doc.resolve(0.0);
        }))
        .is_ok()
        {
            renderer.render(
                |scene| {
                    blitz_paint::paint_scene(scene, &mut doc, scale, width, height, 0, 0);
                },
                &mut frame.rgba,
            );
        } else {
            fill_fallback(&mut frame.rgba, width, height);
        }
    } else {
        // DOM creation failed (fontique NULL on kei). Use fallback.
        fill_fallback(&mut frame.rgba, width, height);
    }

    Ok(frame)
}

/// Fallback pixel-fill when Blitz DOM/Vello rendering fails (e.g., on kei
/// where fontique/skrifa font metrics init NULL-derefs). Draws a simple
/// browser-style UI (header bar + content cards) directly into the RGBA buffer.
#[cfg(feature = "render")]
fn fill_fallback(rgba: &mut [u8], width: u32, height: u32) {
    for y in 0..height as usize {
        for x in 0..width as usize {
            let idx = (y * width as usize + x) * 4;
            let (r, g, b) = if y < 60 {
                (0x61, 0xAF, 0xEF) // blue header
            } else if (80..160).contains(&y) {
                (0x21, 0x25, 0x2B) // card 1
            } else if (180..260).contains(&y) {
                (0x21, 0x25, 0x2B) // card 2
            } else if (280..360).contains(&y) {
                (0x21, 0x25, 0x2B) // card 3
            } else {
                (0x28, 0x2C, 0x34) // dark bg
            };
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 0xFF;
        }
    }
}

/// Renders HTML with an embedded font, bypassing `system_fonts`/fontconfig.
///
/// This is for headless targets (aarch64-musl, kei fbdev) where fontconfig
/// cannot be linked. The embedded DejaVu Sans is registered into a custom
/// `FontContext` so text renders without system font discovery.
#[cfg(feature = "render")]
pub fn render_html_with_font(html: &str, config: &RenderConfig) -> anyhow::Result<Frame> {
    let width = config.width;
    let height = config.height;
    let scale = config.scale as f64;

    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::shell::Viewport;
    use linebender_resource_handle::Blob;
    use parley::FontContext;
    use parley::fontique::{Collection, CollectionOptions, SourceCache};
    use std::sync::Arc;

    // Build a FontContext with the embedded font — no system_fonts needed.
    let mut font_ctx = FontContext {
        source_cache: SourceCache::new_shared(),
        collection: Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        }),
    };
    font_ctx
        .collection
        .register_fonts(Blob::new(Arc::new(EMBEDDED_FONT) as _), None);

    let viewport = Viewport {
        window_size: (width, height),
        hidpi_scale: config.scale,
        ..Default::default()
    };

    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        font_ctx: Some(font_ctx),
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, doc_config);
    doc.resolve(0.0);

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

/// Execute `<script>` blocks in the HTML via Boa, then render.
///
/// When the `js` feature is enabled, this runs any inline `<script>` tags
/// through the Boa JS engine before feeding the (possibly modified) HTML to
/// [`render_html`]. JS side effects that write to `document.body` or
/// `document.write` are injected into the HTML so the rendered output reflects
/// the script's output.
///
/// Without the `js` feature, this is equivalent to [`render_html`].
#[cfg(feature = "js")]
#[cfg(feature = "render")]
pub fn render_html_with_js(html: &str, config: &RenderConfig) -> anyhow::Result<Frame> {
    let result = aris_js::execute_scripts(html);
    if !result.errors.is_empty() {
        for e in &result.errors {
            tracing::warn!("[js] {}", e);
        }
    }
    // If the script wrote body content via document.write, inject it before
    // </body>. This is a minimal integration — full DOM mutation would need
    // the tairitsu WIT host (Phase 4).
    let final_html = if let Some(body) = result.document_props.get("body") {
        if !body.is_empty() {
            html.replace("</body>", &format!("{}\n</body>", body))
        } else {
            html.to_string()
        }
    } else {
        html.to_string()
    };
    render_html(&final_html, config)
}
