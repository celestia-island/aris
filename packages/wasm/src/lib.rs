//! aris-wasm — WASM Component Model host for the aris browser.
//!
//! This crate embeds [`tairitsu_ssr`] (built on Wasmtime) to execute
//! tairitsu/hikari WASM components (compiled to `wasm32-wasip2`) and extract
//! their rendered HTML. The HTML string is then handed to
//! [`aris_render::render_html`] for blitz-dom parsing + Vello CPU rasterization.
//!
//! ## Data flow
//!
//! ```text
//! hikari/tairitsu component (.wasm)
//!   → tairitsu_ssr::render_to_html()   [Wasmtime host, sync]
//!   → HTML string
//!   → aris_render::render_html()       [blitz + Vello CPU]
//!   → Frame (RGBA pixels)
//! ```
//!
//! This is the "boa + wasmtime → blitz" path described in the aris PLAN.md
//! §6.5 interaction mode (Phase 1B). Boa (JS glue) integration lives in the
//! `aris-js` crate; this crate handles the WASM/WASI side.

use anyhow::Result;

/// Render a tairitsu/hikari WASM component to an HTML string.
///
/// Loads `wasm_bytes` (a `wasm32-wasip2` Component) into Wasmtime, instantiates
/// it with the `tairitsu-browser:full` WIT world + WASI preview2, calls
/// `lifecycle::start`, and returns the rendered `<body>` HTML.
///
/// # Arguments
/// * `wasm_bytes` - The compiled WASM Component bytes (`.wasm` file contents)
/// * `viewport_width` - Simulated viewport width in CSS pixels
/// * `viewport_height` - Simulated viewport height in CSS pixels
pub fn render_component_to_html(
    wasm_bytes: &[u8],
    viewport_width: i32,
    viewport_height: i32,
) -> Result<String> {
    let config = tairitsu_ssr::SsrConfig::new(viewport_width, viewport_height);
    let html = tairitsu_ssr::render_to_html(wasm_bytes, config)?;
    tracing::info!(
        "tairitsu-ssr rendered {} bytes of HTML from {} bytes of WASM",
        html.len(),
        wasm_bytes.len()
    );
    Ok(html)
}

/// Render a tairitsu/hikari WASM component all the way to pixels.
///
/// Convenience wrapper: runs the component to HTML via Wasmtime, then renders
/// the HTML through the aris blitz pipeline to an RGBA [`aris_render::Frame`].
pub fn render_component(
    wasm_bytes: &[u8],
    config: &aris_render::RenderConfig,
) -> Result<aris_render::Frame> {
    let html = render_component_to_html(
        wasm_bytes,
        config.width as i32,
        config.height as i32,
    )?;
    aris_render::render_html(&html, config)
}
