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
            // Resize the softbuffer surface to the window size.
            let _ = surface.resize(
                NonZeroU32::new(sw).unwrap(),
                NonZeroU32::new(sh).unwrap(),
            );
            // softbuffer expects XRGB8888 u32 pixels (0x00RRGGBB). Our Frame is
            // RGBA bytes ([R, G, B, A]). Convert per-pixel, mapping frame
            // coordinates onto the (possibly differently-sized) surface with
            // top-left origin.
            if let Ok(mut buffer) = surface.buffer_mut() {
                let fw = frame.width as usize;
                let fh = frame.height as usize;
                let buf_len = buffer.len();
                let pixels_per_row = buf_len / sh as usize;
                for y in 0..(sh as usize).min(fh) {
                    for x in 0..(sw as usize).min(fw) {
                        let src = (y * fw + x) * 4;
                        if src + 2 >= frame.rgba.len() {
                            break;
                        }
                        let r = frame.rgba[src] as u32;
                        let g = frame.rgba[src + 1] as u32;
                        let b = frame.rgba[src + 2] as u32;
                        // XRGB8888: blue in low byte
                        let idx = y * pixels_per_row + x;
                        if idx < buf_len {
                            buffer[idx] = (r << 16) | (g << 8) | b;
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
        surface_size: (0, 0),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
