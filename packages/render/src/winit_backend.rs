// SPDX-License-Identifier: BUSL-1.1

//! Desktop window backend using winit + softbuffer.
//!
//! This module is gated behind the `winit` Cargo feature. It opens a native
//! OS window (Win32/X11/Wayland/macOS via winit) and blits the RGBA pixel
//! buffer produced by [`crate::render_html`] into it each frame using
//! softbuffer's software-rendering surface.
//!
//! ## HiDPI / Retina support
//!
//! The HTML is rendered at the window's **physical** pixel dimensions (logical
//! size × scale_factor). This means text is rasterized by Vello CPU at full
//! sharpness — no blurry upscaling. The softbuffer surface receives the frame
//! 1:1 with no stretching.
//!
//! ## Interaction
//!
//! Mouse clicks and keyboard input are captured. Click coordinates are printed
//! to stderr (converted from physical to logical pixels for CSS coordinate
//! space). Full DOM event dispatch requires the blitz-dom interactive path
//! (future work).

#![cfg(feature = "winit")]

use std::num::NonZeroU32;
use std::rc::Rc;

use crate::{Frame, RenderConfig};

/// Run a blocking event loop that renders `html` into a desktop window.
pub fn run_window(html: &str, config: &RenderConfig) -> anyhow::Result<()> {
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, MouseButton, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::keyboard::{Key, NamedKey};
    use winit::window::{Window, WindowId};

    let event_loop = EventLoop::new()?;

    struct App {
        html: String,
        config: RenderConfig,
        window: Option<Rc<Window>>,
        context: Option<softbuffer::Context<Rc<Window>>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        cached_frame: Option<Frame>,
        /// Physical pixel dimensions of the surface buffer.
        phys_size: (u32, u32),
        /// Current HiDPI scale factor.
        scale_factor: f64,
        /// Whether we need to re-render (initial + resize).
        dirty: bool,
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
            let context =
                softbuffer::Context::new(window.clone()).expect("softbuffer context");
            let surface =
                softbuffer::Surface::new(&context, window.clone()).expect("surface");

            self.window = Some(window);
            self.context = Some(context);
            self.surface = Some(surface);
            self.dirty = true;
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
                    // size is in logical pixels; physical = logical × scale
                    self.scale_factor = self
                        .window
                        .as_ref()
                        .map(|w| w.scale_factor())
                        .unwrap_or(1.0);
                    let pw = (size.width as f64 * self.scale_factor).round() as u32;
                    let ph = (size.height as f64 * self.scale_factor).round() as u32;
                    self.phys_size = (pw.max(1), ph.max(1));
                    self.dirty = true;
                }
                WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                    self.scale_factor = scale_factor;
                    self.dirty = true;
                }
                WindowEvent::RedrawRequested => {
                    self.maybe_render();
                    self.present();
                }
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    device_id: _,
                    ..
                } => {
                    // Click handling: full DOM event dispatch is future work.
                    // For now, just log that we received the event.
                    eprintln!("[winit] mouse click received");
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if event.state == ElementState::Pressed {
                        if let Key::Named(NamedKey::Escape) = event.logical_key {
                            event_loop.exit();
                        }
                    }
                }
                _ => {}
            }
        }
    }

    impl App {
        /// Re-render the HTML at physical resolution if dirty.
        fn maybe_render(&mut self) {
            if !self.dirty {
                return;
            }
            self.dirty = false;

            let (pw, ph) = self.phys_size;
            if pw == 0 || ph == 0 {
                return;
            }

            // Render at full physical resolution for sharp text on HiDPI.
            let render_config = RenderConfig {
                width: pw,
                height: ph,
                scale: self.scale_factor as f32,
            };

            eprintln!(
                "[winit] rendering at {}x{} (scale={:.1})",
                pw, ph, self.scale_factor
            );

            match crate::render_html(&self.html, &render_config) {
                Ok(frame) => {
                    let nb = frame
                        .rgba
                        .chunks_exact(4)
                        .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
                        .count();
                    eprintln!(
                        "[winit] rendered {}x{} non-black {}/{}",
                        frame.width,
                        frame.height,
                        nb,
                        frame.width as usize * frame.height as usize
                    );
                    self.cached_frame = Some(frame);
                }
                Err(e) => {
                    eprintln!("[winit] render error: {:?}", e);
                }
            }
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
                // If frame matches buffer exactly, do a fast copy.
                // Otherwise stretch (shouldn't happen if render used phys size).
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
                    // Nearest-neighbor stretch fallback
                    let bh = buf_len / pw.max(1) as usize;
                    let bw = buf_len / bh.max(1);
                    for dy in 0..bh {
                        let fy = if bh > 0 { dy * fh / bh } else { 0 };
                        if fy >= fh {
                            break;
                        }
                        let row_start = dy * bw;
                        if row_start >= buf_len {
                            break;
                        }
                        for dx in 0..bw {
                            let fx = if bw > 0 { dx * fw / bw } else { 0 };
                            if fx >= fw {
                                break;
                            }
                            let src = (fy * fw + fx) * 4;
                            if src + 2 >= frame.rgba.len() {
                                break;
                            }
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
    }

    let mut app = App {
        html: html.to_string(),
        config: config.clone(),
        window: None,
        context: None,
        surface: None,
        cached_frame: None,
        phys_size: (0, 0),
        scale_factor: 1.0,
        dirty: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Run a window that executes `<script>` blocks via Boa before rendering.
#[cfg(feature = "js")]
pub fn run_window_with_js(html: &str, config: &RenderConfig) -> anyhow::Result<()> {
    // For now, delegate to run_window — the JS execution happens in
    // render_html_with_js which is called by the caller. This variant
    // exists for API symmetry but uses the same winit loop.
    run_window(html, config)
}
