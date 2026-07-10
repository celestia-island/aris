// SPDX-License-Identifier: BUSL-1.1

//! Desktop window backend using winit + softbuffer.
//!
//! This module is gated behind the `winit` Cargo feature. It opens a native
//! OS window (Win32/X11/Wayland/macOS via winit) and blits the RGBA pixel
//! buffer produced by [`crate::render_html`] into it each frame using
//! softbuffer's software-rendering surface.
//!
//! The data flow is identical to the fbdev path — `render_html` produces a
//! `Frame` (RGBA bytes), and this backend converts + presents it — only the
//! output target differs (an OS window vs `/dev/fb0`).

#![cfg(feature = "winit")]

use std::num::NonZeroU32;
use std::rc::Rc;

use crate::{Frame, RenderConfig};

/// Run a blocking event loop that renders `html` into a desktop window.
///
/// The window is sized to `config.width × config.height`. Press the window's
/// close button or Escape to quit.
pub fn run_window(html: &str, config: &RenderConfig) -> anyhow::Result<()> {
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::window::{Window, WindowId};

    let event_loop = EventLoop::new()?;

    struct App {
        html: String,
        config: RenderConfig,
        window: Option<Rc<Window>>,
        context: Option<softbuffer::Context<Rc<Window>>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        cached_frame: Option<Frame>,
        surface_size: (u32, u32),
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
            let context =
                softbuffer::Context::new(window.clone()).expect("softbuffer context");
            let surface =
                softbuffer::Surface::new(&context, window.clone()).expect("surface");

            // Render the HTML once on resume.
            match crate::render_html(&self.html, &self.config) {
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

            self.surface_size = (self.config.width, self.config.height);
            self.window = Some(window);
            self.context = Some(context);
            self.surface = Some(surface);
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
                    let (w, h) = (size.width, size.height);
                    self.surface_size = (w, h);
                    self.present();
                }
                WindowEvent::RedrawRequested => {
                    self.present();
                }
                _ => {}
            }
        }
    }

    impl App {
        fn present(&mut self) {
            let (sw, sh) = self.surface_size;
            if sw == 0 || sh == 0 {
                return;
            }
            let Some(frame) = self.cached_frame.as_ref() else {
                return;
            };
            let Some(surface) = self.surface.as_mut() else {
                return;
            };
            // softbuffer's surface dimensions are in physical pixels. The
            // window reports logical sizes via Resized, so account for HiDPI
            // by scaling up. We request the physical pixel grid from the
            // window's scale factor.
            let scale = self
                .window
                .as_ref()
                .map(|w| w.scale_factor())
                .unwrap_or(1.0);
            let phys_w = ((sw as f64) * scale).round() as u32;
            let phys_h = ((sh as f64) * scale).round() as u32;
            if phys_w == 0 || phys_h == 0 {
                return;
            }
            let _ = surface.resize(
                NonZeroU32::new(phys_w).unwrap(),
                NonZeroU32::new(phys_h).unwrap(),
            );
            // softbuffer expects XRGB8888 u32 pixels (0x00RRGGBB). Our Frame is
            // RGBA bytes ([R, G, B, A]). The surface buffer has phys_w * phys_h
            // entries (one u32 each). We stretch the frame (fw×fh) to fill the
            // physical surface so the full window is covered regardless of
            // HiDPI scaling.
            if let Ok(mut buffer) = surface.buffer_mut() {
                let fw = frame.width as usize;
                let fh = frame.height as usize;
                let pw = phys_w as usize;
                let ph = phys_h as usize;
                let buf_len = buffer.len();
                for dy in 0..ph {
                    // Map physical y → frame y (nearest-neighbor stretch).
                    let fy = if ph > 0 { dy * fh / ph } else { 0 };
                    if fy >= fh {
                        break;
                    }
                    let row_start = dy * pw;
                    if row_start >= buf_len {
                        break;
                    }
                    for dx in 0..pw {
                        let fx = if pw > 0 { dx * fw / pw } else { 0 };
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
                        // XRGB8888: blue in low byte
                        buffer[row_start + dx] = (r << 16) | (g << 8) | b;
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
        surface_size: (0, 0),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
