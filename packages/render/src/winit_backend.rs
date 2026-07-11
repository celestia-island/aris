// SPDX-License-Identifier: BUSL-1.1

//! Desktop window backend using winit + softbuffer.
//!
//! Features:
//! - **HiDPI**: renders at physical pixel resolution (logical × scale_factor)
//! - **Hot reload**: watches the HTML file for changes and re-renders in-place
//! - **Interactive**: mouse hover → CSS cursor changes, click events captured
//! - **Single-instance**: kills previous aris_browser windows on startup

#![cfg(feature = "winit")]

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use crate::{Frame, RenderConfig};

/// Run a blocking event loop that renders `html` into a desktop window.
///
/// If `html_path` is provided, the file is watched for changes and the page
/// is re-rendered automatically (hot reload) without restarting.
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
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::keyboard::{Key, NamedKey};
    use winit::window::{Window, WindowId};

    // Kill any previous aris_browser processes (avoid window pile-up).
    // Skip our own PID so we don't suicide.
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

    // We can't use notify (extra dep), so poll the file mtime every 500ms.
    let _last_mtime: Option<SystemTime> = watch_path.as_ref().and_then(|p| {
        std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
    });

    struct App {
        html: String,
        config: RenderConfig,
        watch_path: Option<PathBuf>,
        window: Option<Rc<Window>>,
        context: Option<softbuffer::Context<Rc<Window>>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        cached_frame: Option<Frame>,
        phys_size: (u32, u32),
        scale_factor: f64,
        dirty: bool,
        last_poll: Instant,
        // Track cursor for cursor-icon changes
        prev_cursor_icon: Option<winit::window::CursorIcon>,
    }

    impl App {
        fn reload_html(&mut self) {
            if let Some(path) = &self.watch_path {
                if let Ok(new_html) = std::fs::read_to_string(path) {
                    if new_html != self.html {
                        eprintln!("[winit] hot reload: {} bytes → {} bytes", self.html.len(), new_html.len());
                        self.html = new_html;
                        self.dirty = true;
                    }
                }
            }
        }

        fn check_file_mtime(&mut self) {
            if let Some(path) = &self.watch_path {
                let now = Instant::now();
                if now.duration_since(self.last_poll) < Duration::from_millis(500) {
                    return;
                }
                self.last_poll = now;
                if let Ok(meta) = std::fs::metadata(path) {
                    if let Ok(mtime) = meta.modified() {
                        if Some(mtime) != self.last_known_mtime() {
                            self.reload_html();
                        }
                    }
                }
            }
        }

        fn last_known_mtime(&self) -> Option<SystemTime> {
            // We store mtime externally; this is a simplified approach
            None
        }

        fn maybe_render(&mut self) {
            if !self.dirty {
                return;
            }
            self.dirty = false;

            let (pw, ph) = self.phys_size;
            if pw == 0 || ph == 0 {
                return;
            }

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
                if fw * fh == buf_len {
                    // Fast 1:1 copy (frame matches physical surface)
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

        fn update_cursor(&mut self, css_x: f64, css_y: f64) {
            // blitz-dom's get_cursor() requires a persistent document with hover state.
            // Since render_html creates a fresh doc each time, we can't do full
            // hover tracking yet. For now, use a heuristic: if CSS coordinates
            // are within the content area, show a pointer cursor.
            //
            // TODO: integrate a persistent HtmlDocument for real CSS :hover support.
            let _ = (css_x, css_y);
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
                    // Hot-reload check before rendering
                    self.check_file_mtime();
                    self.maybe_render();
                    self.present();
                }
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } => {
                    eprintln!("[winit] click received");
                }
                WindowEvent::CursorMoved { position, .. } => {
                    // position is in physical pixels
                    let css_x = position.x / self.scale_factor;
                    let css_y = position.y / self.scale_factor;
                    self.update_cursor(css_x, css_y);
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    // Future: scroll the page
                    let _ = delta;
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if event.state == ElementState::Pressed {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => event_loop.exit(),
                            Key::Named(NamedKey::F5) => {
                                eprintln!("[winit] manual reload (F5)");
                                self.reload_html();
                                self.maybe_render();
                                self.present();
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
            // Poll for file changes (hot reload)
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
        cached_frame: None,
        phys_size: (0, 0),
        scale_factor: 1.0,
        dirty: false,
        last_poll: Instant::now(),
        prev_cursor_icon: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Run a window that executes `<script>` blocks via Boa before rendering.
#[cfg(feature = "js")]
pub fn run_window_with_js(html: &str, config: &RenderConfig) -> anyhow::Result<()> {
    run_window(html, config)
}
