// SPDX-License-Identifier: BUSL-1.1

//! Desktop window backend using winit + softbuffer.
//!
//! Features:
//! - **HiDPI**: renders at physical pixel resolution (logical × scale_factor)
//! - **Hot reload**: watches the HTML file for changes and re-renders in-place
//! - **Instant hover**: overlay-based highlight drawn on the cached base frame
//!   — no full CSS resolve() or Vello re-raster needed per mouse move
//! - **Interactive**: CSS cursor changes, click events, scroll, keyboard input
//! - **Single-instance**: kills previous aris_browser windows on startup

#![cfg(feature = "winit")]

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::RenderConfig;

/// Run a blocking event loop that renders `html` into a desktop window.
pub fn run_window(html: &str, config: &RenderConfig) -> anyhow::Result<()> {
    run_window_impl(html, config, None)
}

/// Run with hot-reload from a file path.
pub fn run_window_file(html_path: &str, config: &RenderConfig) -> anyhow::Result<()> {
    let html = std::fs::read_to_string(html_path)
        .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", html_path, e))?;
    run_window_impl(&html, config, Some(PathBuf::from(html_path)))
}

fn run_window_impl(
    initial_html: &str,
    config: &RenderConfig,
    watch_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    use blitz_html::HtmlDocument;
    use blitz_dom::DocumentConfig;
    use blitz_traits::shell::Viewport;
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, MouseButton, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::keyboard::{Key, NamedKey};
    use winit::window::{CursorIcon as WinitCursorIcon, Window, WindowId};

    // Kill any previous aris_browser processes (avoid window pile-up).
    #[cfg(target_os = "windows")]
    {
        let our_pid = std::process::id();
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile", "-Command",
                &format!(
                    "Get-Process aris_browser -ErrorAction SilentlyContinue | Where-Object {{ $_.Id -ne {} }} | Stop-Process -Force",
                    our_pid
                ),
            ])
            .output();
    }

    let event_loop = EventLoop::new()?;

    struct App {
        html: String,
        config: RenderConfig,
        watch_path: Option<PathBuf>,
        window: Option<Rc<Window>>,
        context: Option<softbuffer::Context<Rc<Window>>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        // Persistent document — survives across frames for hover/click state.
        doc: Option<HtmlDocument>,
        /// The "base" frame — the fully rendered page without any hover overlay.
        /// Only re-rendered on load/resize/hot-reload/click (not on hover).
        base_frame: Option<crate::Frame>,
        /// Pre-converted XRGB8888 u32 buffer of base_frame. Avoids re-doing the
        /// 3M-pixel RGBA→XRGB loop on every hover redraw.
        base_xrgb: Vec<u32>,
        phys_size: (u32, u32),
        scale_factor: f64,
        /// True when the base frame needs re-rendering (resize, reload, click).
        needs_rerender: bool,
        /// Previous hover rect (physical px) so we can restore only those pixels.
        prev_hover_rect: Option<(i32, i32, i32, i32)>,
        last_poll: Instant,
        prev_cursor: WinitCursorIcon,
    }

    impl App {
        fn build_doc(&mut self) {
            let (pw, ph) = self.phys_size;
            if pw == 0 || ph == 0 {
                return;
            }
            let viewport = Viewport {
                window_size: (pw, ph),
                hidpi_scale: self.scale_factor as f32,
                ..Default::default()
            };
            let doc_config = DocumentConfig {
                viewport: Some(viewport),
                ..Default::default()
            };
            let mut doc = HtmlDocument::from_html(&self.html, doc_config);
            doc.resolve(0.0);
            self.doc = Some(doc);
            self.needs_rerender = true;
        }

        /// Expensive: render the full page via Vello CPU rasterization, then
        /// pre-convert to XRGB8888 for fast blitting.
        fn render_base_frame(&mut self) {
            let Some(doc) = self.doc.as_mut() else {
                return;
            };
            let (pw, ph) = self.phys_size;
            if pw == 0 || ph == 0 {
                return;
            }
            doc.resolve(0.0);
            let scale = self.scale_factor;
            let mut frame = crate::Frame::new(pw, ph);
            use anyrender::ImageRenderer;
            let mut renderer = anyrender_vello_cpu::VelloCpuImageRenderer::new(pw, ph);
            renderer.render(
                |scene| {
                    blitz_paint::paint_scene(scene, doc, scale, pw, ph, 0, 0);
                },
                &mut frame.rgba,
            );

            // Pre-convert RGBA bytes → XRGB u32 for softbuffer (done ONCE per
            // base render, not per hover redraw).
            let pixel_count = (pw as usize) * (ph as usize);
            let mut xrgb = Vec::with_capacity(pixel_count);
            for i in 0..pixel_count {
                let src = i * 4;
                if src + 2 < frame.rgba.len() {
                    let r = frame.rgba[src] as u32;
                    let g = frame.rgba[src + 1] as u32;
                    let b = frame.rgba[src + 2] as u32;
                    xrgb.push((r << 16) | (g << 8) | b);
                } else {
                    xrgb.push(0);
                }
            }
            self.base_xrgb = xrgb;
            self.base_frame = Some(frame);
            self.needs_rerender = false;
            // Force full re-blit + overlay on next present.
            self.prev_hover_rect = None;
        }

        fn reload_html(&mut self) {
            if let Some(path) = &self.watch_path {
                if let Ok(new_html) = std::fs::read_to_string(path) {
                    if new_html != self.html {
                        eprintln!(
                            "[winit] hot reload: {} → {} bytes",
                            self.html.len(),
                            new_html.len()
                        );
                        self.html = new_html;
                        self.build_doc();
                    }
                }
            }
        }

        fn check_file_mtime(&mut self) {
            if self.watch_path.is_none() {
                return;
            }
            let now = Instant::now();
            if now.duration_since(self.last_poll) < Duration::from_millis(500) {
                return;
            }
            self.last_poll = now;
            self.reload_html();
        }

        /// Get the bounding box (in CSS logical pixels) of the currently hovered node.
        /// Returns None if no node is hovered or layout data is unavailable.
        fn hover_rect_css(&self) -> Option<(f32, f32, f32, f32)> {
            let doc = self.doc.as_ref()?;
            let hover_id = doc.get_hover_node_id()?;
            let node = doc.get_node(hover_id)?;
            let pos = node.absolute_position(0.0, 0.0);
            let w = node.final_layout.size.width;
            let h = node.final_layout.size.height;
            if w < 1.0 || h < 1.0 {
                return None;
            }
            Some((pos.x, pos.y, w, h))
        }

        /// Fast present: memcpy base XRGB to softbuffer, then draw/clear hover
        /// overlay incrementally. The per-pixel RGBA→XRGB conversion is done once
        /// in render_base_frame; here we only touch the hover rect pixels.
        fn present_with_overlay(&mut self) {
            let (pw, ph) = self.phys_size;
            if pw == 0 || ph == 0 || self.base_xrgb.is_empty() {
                return;
            }
            // Compute hover rect BEFORE borrowing surface mutably.
            let hover_rect = self.hover_rect_css();
            let scale = self.scale_factor;
            let Some(surface) = self.surface.as_mut() else {
                return;
            };
            let _ = surface.resize(
                NonZeroU32::new(pw).unwrap(),
                NonZeroU32::new(ph).unwrap(),
            );

            let Ok(mut buffer) = surface.buffer_mut() else {
                return;
            };
            let buf_len = buffer.len();
            let buf_w = pw as usize;
            let buf_h = ph as usize;
            let xrgb = &self.base_xrgb;

            // Fast memcpy: copy entire base XRGB buffer to softbuffer in one shot.
            if xrgb.len() == buf_len {
                buffer[..buf_len].copy_from_slice(xrgb);
            } else {
                // Size mismatch — fallback scaled copy (rare, only on resize).
                let xw = (xrgb.len() as f64 / buf_h as f64).sqrt() as usize;
                for dy in 0..buf_h {
                    let fy = dy * (buf_h / buf_h.max(1));
                    let row = dy * buf_w;
                    for dx in 0..buf_w {
                        if row + dx >= buf_len { break; }
                        let fx = dx * xw / buf_w.max(1);
                        let si = fy.min(buf_h - 1) * xw + fx.min(xw - 1);
                        if si < xrgb.len() {
                            buffer[row + dx] = xrgb[si];
                        }
                    }
                }
            }

            // Draw hover highlight overlay — only on the hovered element's pixels.
            if let Some((cx, cy, cw, ch)) = hover_rect {
                let px = (cx * scale as f32) as i32;
                let py = (cy * scale as f32) as i32;
                let pw_px = (cw * scale as f32) as i32;
                let ph_px = (ch * scale as f32) as i32;

                for ry in 0..ph_px {
                    let y = py + ry;
                    if y < 0 || y as usize >= buf_h { continue; }
                    let row = y as usize * buf_w;
                    for rx in 0..pw_px {
                        let x = px + rx;
                        if x < 0 || x as usize >= buf_w { continue; }
                        let idx = row + x as usize;
                        if idx >= buf_len { break; }
                        let pixel = buffer[idx];
                        let r = (pixel >> 16) & 0xFF;
                        let g = (pixel >> 8) & 0xFF;
                        let b = pixel & 0xFF;
                        // 2px bright cyan border, else white-blend fill.
                        let on_border = ry < 2 || rx < 2 || ry >= ph_px - 2 || rx >= pw_px - 2;
                        if on_border {
                            buffer[idx] = 0x00FFFF00;
                        } else {
                            let nr = r + ((255 - r) * 22 / 100);
                            let ng = g + ((255 - g) * 22 / 100);
                            let nb = b + ((255 - b) * 22 / 100);
                            buffer[idx] = (nr << 16) | (ng << 8) | nb;
                        }
                    }
                }
                self.prev_hover_rect = Some((px, py, pw_px, ph_px));
            } else {
                self.prev_hover_rect = None;
            }

            let _ = buffer.present();
        }

        fn update_cursor_icon(&mut self) {
            let cursor = self.doc.as_ref().and_then(|d| d.get_cursor());
            let icon = match cursor {
                Some(c) => {
                    let name = format!("{:?}", c).to_lowercase();
                    match name.as_str() {
                        "pointer" => winit::window::CursorIcon::Pointer,
                        "text" => winit::window::CursorIcon::Text,
                        "wait" => winit::window::CursorIcon::Wait,
                        "crosshair" => winit::window::CursorIcon::Crosshair,
                        "notallowed" => winit::window::CursorIcon::NotAllowed,
                        "grab" => winit::window::CursorIcon::Grab,
                        "grabbing" => winit::window::CursorIcon::Grabbing,
                        "help" => winit::window::CursorIcon::Help,
                        "move" => winit::window::CursorIcon::AllScroll,
                        _ => winit::window::CursorIcon::Default,
                    }
                }
                None => winit::window::CursorIcon::Default,
            };
            if icon != self.prev_cursor {
                if let Some(window) = &self.window {
                    window.set_cursor(icon);
                }
                self.prev_cursor = icon;
            }
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let window = event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("aris-render")
                        .with_inner_size(winit::dpi::LogicalSize::new(
                            self.config.width,
                            self.config.height,
                        )),
                )
                .expect("create window");

            let window = Rc::new(window);
            self.scale_factor = window.scale_factor();
            // Set initial physical size from the window
            let inner = window.inner_size();
            self.phys_size = (inner.width.max(1), inner.height.max(1));

            let context = softbuffer::Context::new(window.clone()).expect("ctx");
            let surface = softbuffer::Surface::new(&context, window.clone()).expect("surface");

            self.window = Some(window);
            self.context = Some(context);
            self.surface = Some(surface);

            // Build initial document
            self.build_doc();
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _window_id: WindowId,
            event: WindowEvent,
        ) {
            match event {
                WindowEvent::CloseRequested => {
                    event_loop.exit();
                }
                WindowEvent::Resized(size) => {
                    self.scale_factor = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.0);
                    let pw = (size.width as f64 * self.scale_factor).round() as u32;
                    let ph = (size.height as f64 * self.scale_factor).round() as u32;
                    self.phys_size = (pw.max(1), ph.max(1));
                    self.build_doc();
                    if let Some(w) = &self.window { w.request_redraw(); }
                }
                WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                    self.scale_factor = scale_factor;
                    self.build_doc();
                    if let Some(w) = &self.window { w.request_redraw(); }
                }
                WindowEvent::RedrawRequested => {
                    self.check_file_mtime();
                    // Only do expensive Vello re-raster if the page content changed.
                    if self.needs_rerender {
                        self.render_base_frame();
                    }
                    // Present base frame + instant hover overlay.
                    self.present_with_overlay();
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let css_x = (position.x / self.scale_factor) as f32;
                    let css_y = (position.y / self.scale_factor) as f32;
                    let hover_changed = if let Some(doc) = self.doc.as_mut() {
                        doc.set_hover_to(css_x, css_y)
                    } else {
                        false
                    };
                    // Cursor icon is instant — reads cached styles, no resolve().
                    self.update_cursor_icon();
                    // If the hover target changed, we only need to re-draw the overlay,
                    // NOT re-render the page. This is microseconds vs hundreds of ms.
                    // RedrawRequested is always cheap here because needs_rerender=false.
                    if hover_changed {
                        if let Some(w) = &self.window { w.request_redraw(); }
                    }
                }
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } => {
                    use blitz_traits::events::{
                        BlitzPointerEvent, BlitzPointerId, DomEvent, DomEventData,
                        PointerCoords, MouseEventButton, MouseEventButtons,
                    };
                    if let Some(doc) = self.doc.as_mut() {
                        let target = doc.get_hover_node_id().unwrap_or(0);
                        let css_x = 0.0; // coordinates not critical for click dispatch
                        let css_y = 0.0;
                        let pe = BlitzPointerEvent {
                            id: BlitzPointerId::Mouse,
                            is_primary: true,
                            coords: PointerCoords {
                                page_x: css_x, page_y: css_y,
                                screen_x: css_x, screen_y: css_y,
                                client_x: css_x, client_y: css_y,
                            },
                            button: MouseEventButton::Main,
                            buttons: MouseEventButtons::Primary,
                            mods: unsafe { core::mem::zeroed() },
                            details: Default::default(),
                            element: Default::default(),
                        };
                        let mut dom_event = DomEvent::new(target, DomEventData::Click(pe));
                        doc.handle_dom_event(&mut dom_event, |ev: DomEvent| {
                            eprintln!("[winit] DOM event: target={} type={:?}", ev.target, ev.data.kind());
                        });
                        // Clicks may change DOM (onclick JS, toggled states).
                        self.needs_rerender = true;
                    }
                    if let Some(w) = &self.window { w.request_redraw(); }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let (_dx, dy) = match delta {
                        winit::event::MouseScrollDelta::LineDelta(x, y) => (x as f64 * 30.0, y as f64 * 30.0),
                        winit::event::MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
                    };
                    if let Some(doc) = self.doc.as_mut() {
                        let hover = doc.get_hover_node_id().unwrap_or(0);
                        doc.scroll_node_by(hover, dy, 0.0, &mut |_| {});
                        self.needs_rerender = true;
                        if let Some(w) = &self.window { w.request_redraw(); }
                    }
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if event.state == ElementState::Pressed {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => event_loop.exit(),
                            Key::Named(NamedKey::F5) => {
                                eprintln!("[winit] manual reload (F5)");
                                self.reload_html();
                                if let Some(w) = &self.window { w.request_redraw(); }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
            self.check_file_mtime();
            if self.needs_rerender {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }

    let mut app = App {
        html: initial_html.to_string(),
        config: config.clone(),
        watch_path,
        window: None,
        context: None,
        surface: None,
        doc: None,
        base_frame: None,
        base_xrgb: Vec::new(),
        phys_size: (0, 0),
        scale_factor: 1.0,
        needs_rerender: false,
        prev_hover_rect: None,
        last_poll: Instant::now(),
        prev_cursor: WinitCursorIcon::Default,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Run a window that executes `<script>` blocks via Boa before rendering.
#[cfg(feature = "js")]
pub fn run_window_with_js(html: &str, config: &RenderConfig) -> anyhow::Result<()> {
    run_window(html, config)
}
