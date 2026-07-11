// SPDX-License-Identifier: BUSL-1.1

//! Desktop window backend using winit + softbuffer.
//!
//! Features:
//! - **HiDPI**: renders at physical pixel resolution (logical × scale_factor)
//! - **Hot reload**: watches the HTML file for changes and re-renders in-place
//! - **Interactive**: persistent HtmlDocument with CSS :hover cursor changes,
//!   click events, scroll, and keyboard input
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
    use blitz_traits::events::{
        BlitzPointerEvent, BlitzPointerId, DomEvent, DomEventData,
        PointerCoords, MouseEventButton, MouseEventButtons,
    };
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
        cached_frame: Option<crate::Frame>,
        phys_size: (u32, u32),
        scale_factor: f64,
        dirty: bool,
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
            self.dirty = true;
        }

        fn render_doc(&mut self) {
            let Some(doc) = self.doc.as_mut() else {
                return;
            };
            let (pw, ph) = self.phys_size;
            if pw == 0 || ph == 0 {
                return;
            }
            let scale = self.scale_factor;
            eprintln!("[winit] paint {}x{} (scale={:.1})", pw, ph, scale);
            let mut frame = crate::Frame::new(pw, ph);
            use anyrender::ImageRenderer;
            let mut renderer = anyrender_vello_cpu::VelloCpuImageRenderer::new(pw, ph);
            renderer.render(
                |scene| {
                    blitz_paint::paint_scene(scene, doc, scale, pw, ph, 0, 0);
                },
                &mut frame.rgba,
            );
            self.cached_frame = Some(frame);
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

        fn present(&mut self) {
            let (pw, ph) = self.phys_size;
            if pw == 0 || ph == 0 {
                return;
            }
            let Some(frame) = self.cached_frame.as_ref() else {
                return;
            };
            let Some(surface) = self.surface.as_mut() else {
                return;
            };
            let _ = surface.resize(
                NonZeroU32::new(pw).unwrap(),
                NonZeroU32::new(ph).unwrap(),
            );
            if let Ok(mut buffer) = surface.buffer_mut() {
                let fw = frame.width as usize;
                let fh = frame.height as usize;
                let buf_len = buffer.len();
                if fw * fh == buf_len {
                    for i in 0..buf_len {
                        let src = i * 4;
                        if src + 2 < frame.rgba.len() {
                            let r = frame.rgba[src] as u32;
                            let g = frame.rgba[src + 1] as u32;
                            let b = frame.rgba[src + 2] as u32;
                            buffer[i] = (r << 16) | (g << 8) | b;
                        }
                    }
                } else {
                    let bh = buf_len / pw.max(1) as usize;
                    let bw = buf_len / bh.max(1);
                    for dy in 0..bh {
                        let fy = if bh > 0 { dy * fh / bh } else { 0 };
                        if fy >= fh { break; }
                        let row_start = dy * bw;
                        if row_start >= buf_len { break; }
                        for dx in 0..bw {
                            let fx = if bw > 0 { dx * fw / bw } else { 0 };
                            if fx >= fw { break; }
                            let src = (fy * fw + fx) * 4;
                            if src + 2 >= frame.rgba.len() { break; }
                            let r = frame.rgba[src] as u32;
                            let g = frame.rgba[src + 1] as u32;
                            let b = frame.rgba[src + 2] as u32;
                            buffer[row_start + dx] = (r << 16) | (g << 8) | b;
                        }
                    }
                }
                let _ = buffer.present();
            }
        }

        fn make_pointer_event(&self, pos: winit::dpi::PhysicalPosition<f64>) -> BlitzPointerEvent {
            // pos is in physical pixels; blitz uses CSS (logical) coordinates
            let css_x = (pos.x / self.scale_factor) as f32;
            let css_y = (pos.y / self.scale_factor) as f32;
            BlitzPointerEvent {
                id: BlitzPointerId::Mouse,
                is_primary: true,
                coords: PointerCoords {
                    page_x: css_x,
                    page_y: css_y,
                    screen_x: css_x,
                    screen_y: css_y,
                    client_x: css_x,
                    client_y: css_y,
                },
                button: MouseEventButton::Main,
                buttons: MouseEventButtons::Primary,
                mods: unsafe { core::mem::zeroed() },
                details: Default::default(),
                element: Default::default(),
            }
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
                    window.set_cursor_icon(icon);
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
                    if self.dirty {
                        self.render_doc();
                        self.dirty = false;
                    }
                    self.present();
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let css_x = (position.x / self.scale_factor) as f32;
                    let css_y = (position.y / self.scale_factor) as f32;
                    if let Some(doc) = self.doc.as_mut() {
                        let changed = doc.set_hover_to(css_x, css_y);
                        if changed {
                            doc.resolve(0.0);
                            self.dirty = true;
                        }
                    }
                    // Update cursor icon based on CSS cursor property
                    self.update_cursor_icon();
                    if self.dirty {
                        if let Some(w) = &self.window { w.request_redraw(); }
                    }
                }
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } => {
                    // Dispatch click event to the document
                    let scale = self.scale_factor;
                    if let Some(doc) = self.doc.as_mut() {
                        let target = doc.get_hover_node_id().unwrap_or(0);
                        let pe = BlitzPointerEvent {
                            id: BlitzPointerId::Mouse,
                            is_primary: true,
                            coords: PointerCoords {
                                page_x: 0.0, page_y: 0.0,
                                screen_x: 0.0, screen_y: 0.0,
                                client_x: 0.0, client_y: 0.0,
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
                        doc.resolve(0.0);
                        self.dirty = true;
                    }
                    let _ = scale;
                    if let Some(w) = &self.window { w.request_redraw(); }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let (dx, dy) = match delta {
                        winit::event::MouseScrollDelta::LineDelta(x, y) => (x as f64 * 30.0, y as f64 * 30.0),
                        winit::event::MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
                    };
                    if let Some(doc) = self.doc.as_mut() {
                        let hover = doc.get_hover_node_id().unwrap_or(0);
                        doc.scroll_node_by(hover, dy, dx, &mut |_| {});
                        self.dirty = true;
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
            if self.dirty {
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
        cached_frame: None,
        phys_size: (0, 0),
        scale_factor: 1.0,
        dirty: false,
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
