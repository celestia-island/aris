// SPDX-License-Identifier: BUSL-1.1

//! Desktop browser window backend using winit + softbuffer.
//!
//! Features:
//! - **HiDPI**: renders at physical pixel resolution (logical × scale_factor)
//! - **Browser chrome**: a 44px toolbar with back/forward/reload buttons and an
//!   editable address bar, drawn directly into the softbuffer surface above the
//!   page content.
//! - **Navigation**: clicking `<a href>` and submitting forms works via a
//!   [`BrowserNavigationProvider`](crate::browser::BrowserNavigationProvider);
//!   the address bar accepts URLs, file paths, or search queries.
//! - **Networking**: an [`HttpNetProvider`](crate::browser::HttpNetProvider)
//!   fetches `<img>`, `<link rel=stylesheet>`, and `@font-face` over HTTP(S)
//!   or `file://`.
//! - **Text input**: typing into `<input>`/`<textarea>` is driven by blitz-dom's
//!   own editor via `UiEvent::KeyDown` — no hand-rolled editing.
//! - **Instant hover**: overlay-based highlight on the cached base frame.
//! - **Single-instance**: kills previous aris_browser windows on startup.

#![cfg(feature = "winit")]

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::RenderConfig;
use crate::browser::{
    BrowserNavigationProvider, BrowserShellProvider, BrowserState, HttpNetProvider,
};

use blitz_dom::Document;
use blitz_html::HtmlDocument;
use blitz_traits::events::{
    BlitzPointerEvent, BlitzPointerId, KeyState, MouseEventButton, MouseEventButtons,
    PointerCoords, UiEvent,
};
use blitz_traits::shell::Viewport;
use url::Url;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon as WinitCursorIcon, Window, WindowId};

/// Height of the browser chrome (address bar + buttons), in CSS logical px.
const CHROME_HEIGHT_CSS: f32 = 44.0;

/// Run a blocking event loop that renders `html` into a desktop window.
pub fn run_window(html: &str, config: &RenderConfig) -> anyhow::Result<()> {
    run_window_impl(html, None, false, config)
}

/// Load and render `url` (a URL or local path). Used by the CLI.
pub fn run_window_url(url: &str, config: &RenderConfig) -> anyhow::Result<()> {
    let normalized = crate::browser::normalize_input_to_url(url);
    let (html, final_url, fetch_remote) = match normalized.scheme() {
        "file" => {
            let path = normalized
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("invalid file path: {}", url))?;
            let html = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", path.display(), e))?;
            (html, normalized, false)
        }
        "about" | "data" => (
            crate::browser::about_or_data_html(&normalized),
            normalized,
            false,
        ),
        _ => (loading_page(normalized.as_ref()), normalized, true),
    };
    run_window_impl(&html, Some(final_url), fetch_remote, config)
}

fn run_window_impl(
    initial_html: &str,
    initial_url: Option<Url>,
    fetch_remote: bool,
    config: &RenderConfig,
) -> anyhow::Result<()> {
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
    let state = Arc::new(BrowserState::new());
    // For an http(s) initial URL, kick off the fetch immediately so the
    // loading page is replaced as soon as bytes arrive.
    if fetch_remote && let Some(url) = &initial_url {
        state.navigate_input(url.as_str());
    }
    let mut app = App {
        config: config.clone(),
        window: None,
        context: None,
        surface: None,
        doc: None,
        current_html: initial_html.to_string(),
        current_url: initial_url.clone(),
        base_xrgb: Vec::new(),
        phys_size: (0, 0),
        scale_factor: 1.0,
        needs_rerender: false,
        last_caret: Instant::now(),
        prev_cursor: WinitCursorIcon::Default,
        state: Arc::clone(&state),
        chrome: ChromeState::new(),
        last_mouse: (0.0, 0.0),
        modifiers: ModifiersState::default(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// The browser application state, held across the event loop.
struct App {
    config: RenderConfig,
    window: Option<Rc<Window>>,
    context: Option<softbuffer::Context<Rc<Window>>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    doc: Option<HtmlDocument>,
    /// Source HTML of the current document (kept so a resize rebuild keeps it).
    current_html: String,
    /// Canonical URL of the current document (base URL for relative resolution).
    current_url: Option<Url>,
    /// Pre-converted XRGB8888 of the rendered page (no chrome overlay).
    base_xrgb: Vec<u32>,
    phys_size: (u32, u32),
    scale_factor: f64,
    needs_rerender: bool,
    last_caret: Instant,
    prev_cursor: WinitCursorIcon,
    state: Arc<BrowserState>,
    chrome: ChromeState,
    last_mouse: (f32, f32),
    modifiers: ModifiersState,
}

impl App {
    /// Window size in CSS logical px.
    fn css_size(&self) -> (f32, f32) {
        (
            self.phys_size.0 as f32 / self.scale_factor as f32,
            self.phys_size.1 as f32 / self.scale_factor as f32,
        )
    }

    /// Build a fresh document from `current_html` / `current_url`.
    fn build_doc(&mut self) {
        let (pw, ph) = self.phys_size;
        if pw == 0 || ph == 0 {
            return;
        }
        let chrome_phys = (CHROME_HEIGHT_CSS * self.scale_factor as f32).round() as u32;
        let page_w = pw;
        let page_h = ph.saturating_sub(chrome_phys).max(1);
        let viewport = Viewport {
            window_size: (page_w, page_h),
            hidpi_scale: self.scale_factor as f32,
            ..Default::default()
        };

        let net = Arc::new(HttpNetProvider::new());
        let nav = Arc::new(BrowserNavigationProvider::new(Arc::clone(&self.state)));
        let shell = Arc::new(BrowserShellProvider::new(Arc::clone(&self.state)));

        let mut doc_config = blitz_dom::DocumentConfig {
            viewport: Some(viewport),
            net_provider: Some(net),
            navigation_provider: Some(nav),
            shell_provider: Some(shell),
            ..Default::default()
        };
        if let Some(url) = &self.current_url {
            doc_config.base_url = Some(url.to_string());
        }

        let mut doc = HtmlDocument::from_html(&self.current_html, doc_config);
        doc.resolve(0.0);
        self.doc = Some(doc);
        self.needs_rerender = true;
    }

    /// Rasterize the page to `base_xrgb` (XRGB8888 u32). Chrome drawn on top.
    fn render_base_frame(&mut self) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let (pw, ph) = self.phys_size;
        if pw == 0 || ph == 0 {
            return;
        }
        let chrome_phys = (CHROME_HEIGHT_CSS * self.scale_factor as f32).round() as u32;
        let page_h = ph.saturating_sub(chrome_phys).max(1);
        doc.resolve(0.0);
        let scale = self.scale_factor;
        let mut frame = crate::Frame::new(pw, page_h);
        use anyrender::ImageRenderer;
        let mut renderer = anyrender_vello_cpu::VelloCpuImageRenderer::new(pw, page_h);
        renderer.render(
            |scene| {
                blitz_paint::paint_scene(scene, doc, scale, pw, page_h, 0, 0);
            },
            &mut frame.rgba,
        );

        // RGBA → XRGB u32.
        let pixel_count = frame.width as usize * frame.height as usize;
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
        self.needs_rerender = false;
    }

    /// Hovered node rect in page CSS px.
    fn hover_rect_page_css(&self) -> Option<(f32, f32, f32, f32)> {
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

    /// Blit page frame, draw chrome, draw hover overlay.
    fn present(&mut self) {
        let (pw, ph) = self.phys_size;
        if pw == 0 || ph == 0 {
            return;
        }
        let scale = self.scale_factor as f32;
        let chrome_phys = (CHROME_HEIGHT_CSS * scale).round() as usize;
        let hover_rect = self.hover_rect_page_css();
        let css_w = self.css_size().0;
        let can_back = self.state.can_go_back();
        let can_fwd = self.state.can_go_forward();
        let url_display = if self.chrome.address_focused {
            self.chrome.address.clone()
        } else {
            self.current_url
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_default()
        };
        let focused = self.chrome.address_focused;
        let caret_on = self.chrome.caret_visible();
        let hover_region = self.chrome.hover;

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let _ = surface.resize(NonZeroU32::new(pw).unwrap(), NonZeroU32::new(ph).unwrap());
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        let buf_len = buffer.len();
        let buf_w = pw as usize;
        let buf_h = ph as usize;
        let xrgb = &self.base_xrgb;
        let page_h = buf_h.saturating_sub(chrome_phys);
        let xw = xrgb.len().checked_div(page_h).unwrap_or(0);

        // Copy page base frame below the chrome bar.
        if !xrgb.is_empty() && xw > 0 {
            let copy_n = buf_w.min(xw);
            for y in 0..page_h {
                let row = (y + chrome_phys) * buf_w;
                let srow = y * xw;
                if row + copy_n <= buf_len && srow + copy_n <= xrgb.len() {
                    buffer[row..row + copy_n].copy_from_slice(&xrgb[srow..srow + copy_n]);
                }
            }
        }

        // Chrome bar.
        draw_chrome(
            &mut buffer,
            buf_w,
            chrome_phys,
            css_w,
            scale,
            &url_display,
            focused,
            caret_on,
            can_back,
            can_fwd,
            hover_region,
        );

        // Hover highlight (page space, offset by chrome).
        if let Some((cx, cy, cw, ch)) = hover_rect {
            let px = (cx * scale) as i32;
            let py = (cy * scale) as i32 + chrome_phys as i32;
            let pw_px = (cw * scale) as i32;
            let ph_px = (ch * scale) as i32;
            for ry in 0..ph_px {
                let y = py + ry;
                if y < 0 || y as usize >= buf_h {
                    continue;
                }
                let row = y as usize * buf_w;
                for rx in 0..pw_px {
                    let x = px + rx;
                    if x < 0 || x as usize >= buf_w {
                        continue;
                    }
                    let idx = row + x as usize;
                    if idx >= buf_len {
                        break;
                    }
                    let pixel = buffer[idx];
                    let r = (pixel >> 16) & 0xFF;
                    let g = (pixel >> 8) & 0xFF;
                    let b = pixel & 0xFF;
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
        }

        let _ = buffer.present();
    }

    fn update_cursor_icon(&mut self) {
        let over_chrome = self.last_mouse.1 < CHROME_HEIGHT_CSS;
        let over_address = over_chrome
            && self
                .chrome
                .region_at(self.last_mouse.0, self.last_mouse.1, self.css_size().0)
                == Some(ChromeRegion::Address);
        let icon = if over_address || (!over_chrome && self.is_over_text()) {
            WinitCursorIcon::Text
        } else if over_chrome {
            let region =
                self.chrome
                    .region_at(self.last_mouse.0, self.last_mouse.1, self.css_size().0);
            match region {
                Some(ChromeRegion::Address) => WinitCursorIcon::Text,
                Some(ChromeRegion::Back | ChromeRegion::Forward | ChromeRegion::Reload) => {
                    WinitCursorIcon::Pointer
                }
                None => WinitCursorIcon::Default,
            }
        } else {
            let cursor = self.doc.as_ref().and_then(|d| d.get_cursor());
            match cursor {
                Some(c) => {
                    let name = format!("{:?}", c).to_lowercase();
                    match name.as_str() {
                        "pointer" => WinitCursorIcon::Pointer,
                        "text" => WinitCursorIcon::Text,
                        "wait" => WinitCursorIcon::Wait,
                        "crosshair" => WinitCursorIcon::Crosshair,
                        "notallowed" => WinitCursorIcon::NotAllowed,
                        "grab" => WinitCursorIcon::Grab,
                        "grabbing" => WinitCursorIcon::Grabbing,
                        "help" => WinitCursorIcon::Help,
                        "move" => WinitCursorIcon::AllScroll,
                        _ => WinitCursorIcon::Default,
                    }
                }
                None => WinitCursorIcon::Default,
            }
        };
        if icon != self.prev_cursor {
            if let Some(window) = &self.window {
                window.set_cursor(icon);
            }
            self.prev_cursor = icon;
        }
    }

    fn is_over_text(&self) -> bool {
        self.doc
            .as_ref()
            .and_then(|d| d.get_cursor())
            .map(|c| matches!(format!("{:?}", c).to_lowercase().as_str(), "text"))
            .unwrap_or(false)
    }

    /// Dispatch a pointer event to the document at page-space coords.
    fn dispatch_pointer(&mut self, css_x: f32, css_y: f32, pressed: bool) {
        let mods = winit_modifiers_to_blitz(self.modifiers);
        let pe = BlitzPointerEvent {
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
            buttons: if pressed {
                MouseEventButtons::Primary
            } else {
                MouseEventButtons::empty()
            },
            mods,
            details: unsafe { core::mem::zeroed() },
            element: Default::default(),
        };
        let ui = if pressed {
            UiEvent::PointerDown(pe)
        } else {
            UiEvent::PointerUp(pe)
        };
        if let Some(doc) = self.doc.as_mut() {
            doc.handle_ui_event(ui);
        }
    }

    /// Process queued page loads from providers/fetch threads.
    fn process_loads(&mut self) {
        let loads = self.state.drain_loads();
        if loads.is_empty() {
            return;
        }
        if let Some(load) = loads.into_iter().last() {
            tracing::info!("loading document: {}", load.url);
            self.current_html = load.html;
            self.current_url = Some(load.url.clone());
            self.chrome.set_url(load.url.as_ref());
            self.build_doc();
            self.state.commit_load(load.url);
        }
    }

    fn pump_messages(&mut self) {
        if let Some(doc) = self.doc.as_mut() {
            doc.handle_messages();
        }
    }

    fn apply_title(&mut self) {
        if let Some(title) = self.state.take_title()
            && let Some(window) = &self.window
        {
            let display = if title.trim().is_empty() {
                "aris".to_string()
            } else {
                format!("{} — aris", title)
            };
            window.set_title(&display);
        }
    }

    /// Handle a chrome-originated action.
    fn handle_chrome_action(&mut self, action: ChromeAction) {
        match action {
            ChromeAction::GoBack => {
                self.state.go_back();
            }
            ChromeAction::GoForward => {
                self.state.go_forward();
            }
            ChromeAction::Reload => {
                self.state.reload();
            }
            ChromeAction::Navigate(input) => {
                if !input.trim().is_empty() {
                    self.state.navigate_input(&input);
                }
            }
            ChromeAction::RedrawOnly => {}
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
                    .with_title("aris")
                    .with_inner_size(winit::dpi::LogicalSize::new(
                        self.config.width,
                        self.config.height,
                    )),
            )
            .expect("create window");
        let window = Rc::new(window);
        self.scale_factor = window.scale_factor();
        let inner = window.inner_size();
        self.phys_size = (inner.width.max(1), inner.height.max(1));

        let context = softbuffer::Context::new(window.clone()).expect("ctx");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("surface");
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);

        self.build_doc();
        if let Some(url) = &self.current_url {
            self.state.commit_load(url.clone());
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
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
                self.build_doc();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                self.build_doc();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.process_loads();
                self.pump_messages();
                self.apply_title();
                if self.needs_rerender {
                    self.render_base_frame();
                }
                self.present();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let css_x = (position.x / self.scale_factor) as f32;
                let css_y = (position.y / self.scale_factor) as f32;
                self.last_mouse = (css_x, css_y);
                let css_w = self.css_size().0;
                if css_y < CHROME_HEIGHT_CSS {
                    self.chrome.hover = self.chrome.region_at(css_x, css_y, css_w);
                    self.update_cursor_icon();
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                self.chrome.hover = None;
                let page_y = css_y - CHROME_HEIGHT_CSS;
                let hover_changed = self
                    .doc
                    .as_mut()
                    .map(|d| d.set_hover_to(css_x, page_y))
                    .unwrap_or(false);
                self.update_cursor_icon();
                if hover_changed && let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: mstate,
                button: MouseButton::Left,
                ..
            } => {
                if mstate != ElementState::Pressed {
                    return;
                }
                let (cx, cy) = self.last_mouse;
                let css_w = self.css_size().0;
                if cy < CHROME_HEIGHT_CSS {
                    if let Some(action) = self.chrome.click_at(cx, cy, css_w) {
                        self.handle_chrome_action(action);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                self.chrome.address_focused = false;
                let page_y = cy - CHROME_HEIGHT_CSS;
                self.dispatch_pointer(cx, page_y, true);
                self.dispatch_pointer(cx, page_y, false);
                self.needs_rerender = true;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (_dx, dy) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        (x as f64 * 30.0, y as f64 * 30.0)
                    }
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
                };
                if let Some(doc) = self.doc.as_mut() {
                    let hover = doc.get_hover_node_id().unwrap_or(0);
                    doc.scroll_node_by(hover, dy, 0.0, &mut |_| {});
                    self.needs_rerender = true;
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                // Address-bar input takes priority when focused.
                if self.chrome.address_focused {
                    if pressed && let Some(action) = self.chrome.handle_key(&event, self.modifiers)
                    {
                        self.handle_chrome_action(action);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                if pressed {
                    // Global shortcuts.
                    let ctrl = self.modifiers.control_key() || self.modifiers.super_key();
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            event_loop.exit();
                            return;
                        }
                        Key::Named(NamedKey::F5) => {
                            self.state.reload();
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        Key::Character(c) if ctrl && c.as_str().eq_ignore_ascii_case("r") => {
                            self.state.reload();
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        Key::Character(c) if ctrl && c.as_str().eq_ignore_ascii_case("l") => {
                            self.chrome.focus_address();
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                // Forward to the document for text input / page keys.
                if let Some(blitz_evt) = map_winit_key(&event, self.modifiers)
                    && let Some(doc) = self.doc.as_mut()
                {
                    let ui = if pressed {
                        UiEvent::KeyDown(blitz_evt)
                    } else {
                        UiEvent::KeyUp(blitz_evt)
                    };
                    doc.handle_ui_event(ui);
                    self.needs_rerender = true;
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Drain async loads / redraw requests from providers.
        if self.state.redraw_requested.swap(false, Ordering::Relaxed) {
            self.process_loads();
            self.pump_messages();
            self.apply_title();
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        // Blink the caret.
        let now = Instant::now();
        if now.duration_since(self.last_caret) >= Duration::from_millis(530) {
            self.last_caret = now;
            self.chrome.toggle_caret();
            if self.chrome.address_focused
                && let Some(window) = &self.window
            {
                window.request_redraw();
            }
        }
    }
}

// ── Chrome (address bar + nav buttons) ──────────────────────

/// Layout rectangles for the chrome bar, in CSS logical px.
struct ChromeLayout {
    back: (f32, f32, f32, f32),
    forward: (f32, f32, f32, f32),
    reload: (f32, f32, f32, f32),
    address: (f32, f32, f32, f32),
}

impl ChromeLayout {
    fn compute(width: f32) -> Self {
        let h = CHROME_HEIGHT_CSS;
        let pad = 8.0;
        let btn = 28.0;
        let mut x = pad;
        let back = (x, (h - btn) / 2.0, btn, btn);
        x += btn + 4.0;
        let forward = (x, (h - btn) / 2.0, btn, btn);
        x += btn + 4.0;
        let reload = (x, (h - btn) / 2.0, btn, btn);
        x += btn + 8.0;
        let addr_w = (width - x - pad).max(60.0);
        let address = (x, (h - 26.0) / 2.0, addr_w, 26.0);
        Self {
            back,
            forward,
            reload,
            address,
        }
    }
}

enum ChromeAction {
    GoBack,
    GoForward,
    Reload,
    Navigate(String),
    RedrawOnly,
}

#[derive(Clone, Copy, PartialEq)]
enum ChromeRegion {
    Back,
    Forward,
    Reload,
    Address,
}

struct ChromeState {
    address: String,
    address_focused: bool,
    hover: Option<ChromeRegion>,
    caret_blink: bool,
}

impl ChromeState {
    fn new() -> Self {
        Self {
            address: String::new(),
            address_focused: false,
            hover: None,
            caret_blink: true,
        }
    }

    fn set_url(&mut self, url: &str) {
        self.address = url.to_string();
    }

    fn toggle_caret(&mut self) {
        self.caret_blink = !self.caret_blink;
    }

    fn caret_visible(&self) -> bool {
        self.address_focused && self.caret_blink
    }

    fn focus_address(&mut self) {
        self.address_focused = true;
        self.caret_blink = true;
    }

    fn region_at(&self, x: f32, y: f32, width: f32) -> Option<ChromeRegion> {
        let l = ChromeLayout::compute(width);
        let in_rect =
            |r: (f32, f32, f32, f32)| x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3;
        if in_rect(l.back) {
            Some(ChromeRegion::Back)
        } else if in_rect(l.forward) {
            Some(ChromeRegion::Forward)
        } else if in_rect(l.reload) {
            Some(ChromeRegion::Reload)
        } else if in_rect(l.address) {
            Some(ChromeRegion::Address)
        } else {
            None
        }
    }

    fn click_at(&mut self, x: f32, y: f32, width: f32) -> Option<ChromeAction> {
        match self.region_at(x, y, width)? {
            ChromeRegion::Back => Some(ChromeAction::GoBack),
            ChromeRegion::Forward => Some(ChromeAction::GoForward),
            ChromeRegion::Reload => Some(ChromeAction::Reload),
            ChromeRegion::Address => {
                self.address_focused = true;
                self.caret_blink = true;
                Some(ChromeAction::RedrawOnly)
            }
        }
    }

    fn handle_key(
        &mut self,
        event: &winit::event::KeyEvent,
        modifiers: ModifiersState,
    ) -> Option<ChromeAction> {
        if event.state != ElementState::Pressed {
            return None;
        }
        let ctrl = modifiers.control_key() || modifiers.super_key();
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => {
                self.address_focused = false;
                Some(ChromeAction::Navigate(self.address.clone()))
            }
            Key::Named(NamedKey::Escape) => {
                self.address_focused = false;
                Some(ChromeAction::RedrawOnly)
            }
            Key::Named(NamedKey::Backspace) => {
                self.address.pop();
                self.caret_blink = true;
                Some(ChromeAction::RedrawOnly)
            }
            Key::Character(c) if !ctrl => {
                let s = c.as_str();
                if !s.is_empty() && s.chars().all(|ch| !ch.is_control()) {
                    self.address.push_str(s);
                    self.caret_blink = true;
                    Some(ChromeAction::RedrawOnly)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

// ── Key mapping ─────────────────────────────────────────────

/// Map a winit key event to a blitz `BlitzKeyEvent`.
fn map_winit_key(
    event: &winit::event::KeyEvent,
    modifiers: ModifiersState,
) -> Option<blitz_traits::events::BlitzKeyEvent> {
    let key = winit_key_to_kbd(&event.logical_key);
    let code = winit_physical_to_code(&event.physical_key);
    let state = if event.state == ElementState::Pressed {
        KeyState::Pressed
    } else {
        KeyState::Released
    };
    let text = if let winit::keyboard::Key::Character(c) = &event.logical_key {
        Some(c.to_string())
    } else {
        None
    };
    Some(blitz_traits::events::BlitzKeyEvent {
        key,
        code,
        modifiers: winit_modifiers_to_blitz(modifiers),
        location: keyboard_types::Location::Standard,
        is_auto_repeating: event.repeat,
        is_composing: false,
        state,
        text: text.map(Into::into),
    })
}

fn winit_key_to_kbd(key: &winit::keyboard::Key) -> keyboard_types::Key {
    use winit::keyboard::{Key, NamedKey};
    match key {
        Key::Named(n) => match n {
            NamedKey::Enter => keyboard_types::Key::Enter,
            NamedKey::Backspace => keyboard_types::Key::Backspace,
            NamedKey::Tab => keyboard_types::Key::Tab,
            NamedKey::Escape => keyboard_types::Key::Escape,
            // Space has no dedicated variant in keyboard-types 0.7; it is
            // surfaced as Character(" ").
            NamedKey::Space => keyboard_types::Key::Character(" ".to_string()),
            NamedKey::ArrowLeft => keyboard_types::Key::ArrowLeft,
            NamedKey::ArrowRight => keyboard_types::Key::ArrowRight,
            NamedKey::ArrowUp => keyboard_types::Key::ArrowUp,
            NamedKey::ArrowDown => keyboard_types::Key::ArrowDown,
            NamedKey::Home => keyboard_types::Key::Home,
            NamedKey::End => keyboard_types::Key::End,
            NamedKey::Delete => keyboard_types::Key::Delete,
            NamedKey::PageUp => keyboard_types::Key::PageUp,
            NamedKey::PageDown => keyboard_types::Key::PageDown,
            NamedKey::Shift => keyboard_types::Key::Shift,
            NamedKey::Control => keyboard_types::Key::Control,
            NamedKey::Alt => keyboard_types::Key::Alt,
            NamedKey::Super => keyboard_types::Key::Meta,
            NamedKey::CapsLock => keyboard_types::Key::CapsLock,
            _ => keyboard_types::Key::Unidentified,
        },
        Key::Character(c) => keyboard_types::Key::Character(c.to_string()),
        _ => keyboard_types::Key::Unidentified,
    }
}

fn winit_physical_to_code(key: &winit::keyboard::PhysicalKey) -> keyboard_types::Code {
    use winit::keyboard::PhysicalKey;
    match key {
        PhysicalKey::Unidentified(_) => keyboard_types::Code::Unidentified,
        PhysicalKey::Code(c) => {
            let name = format!("{:?}", c);
            name.parse().unwrap_or(keyboard_types::Code::Unidentified)
        }
    }
}

fn winit_modifiers_to_blitz(m: ModifiersState) -> keyboard_types::Modifiers {
    let mut out = keyboard_types::Modifiers::empty();
    if m.control_key() {
        out |= keyboard_types::Modifiers::CONTROL;
    }
    if m.alt_key() {
        out |= keyboard_types::Modifiers::ALT;
    }
    if m.shift_key() {
        out |= keyboard_types::Modifiers::SHIFT;
    }
    if m.super_key() {
        out |= keyboard_types::Modifiers::META;
    }
    out
}

// ── Chrome drawing ──────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_chrome(
    buffer: &mut [u32],
    buf_w: usize,
    chrome_h: usize,
    css_w: f32,
    scale: f32,
    display: &str,
    focused: bool,
    caret_on: bool,
    can_back: bool,
    can_fwd: bool,
    hover: Option<ChromeRegion>,
) {
    let bg = 0x2D2D3A;
    let btn_color = 0xC8C8D8;
    let btn_hover = 0xFFFFFF;
    let btn_disabled = 0x555566;
    let addr_bg = if focused { 0x1A1B26 } else { 0x24243A };
    let addr_border = if focused { 0x7AA2F7 } else { 0x3A3A4E };
    let accent = 0x7AA2F7;

    // Fill chrome background.
    for y in 0..chrome_h {
        let row = y * buf_w;
        for x in 0..buf_w {
            if row + x < buffer.len() {
                buffer[row + x] = bg;
            }
        }
    }

    let layout = ChromeLayout::compute(css_w);
    let to_phys = |v: f32| (v * scale) as usize;

    // Buttons.
    let draw_arrow = |buffer: &mut [u32],
                      rect: (f32, f32, f32, f32),
                      enabled: bool,
                      hovered: bool,
                      fwd: bool| {
        let color = if !enabled {
            btn_disabled
        } else if hovered {
            btn_hover
        } else {
            btn_color
        };
        let rx = to_phys(rect.0) as i32;
        let ry = to_phys(rect.1) as i32;
        let rw = to_phys(rect.2) as i32;
        let rh = to_phys(rect.3) as i32;
        let cx = rx + rw / 2;
        let cy = ry + rh / 2;
        let sz = rw.min(rh) / 3;
        for dy in 0..sz {
            let half = dy / 2;
            for dx in 0..(sz - half) {
                let (px, py1, py2) = if fwd {
                    (cx + sz / 2 - dx - 1, cy - sz / 2 + dy, cy + sz / 2 - dy - 1)
                } else {
                    (cx - sz / 2 + dx, cy - sz / 2 + dy, cy + sz / 2 - dy - 1)
                };
                plot(buffer, buf_w, px as usize, py1 as usize, color);
                plot(buffer, buf_w, px as usize, py2 as usize, color);
            }
        }
    };
    draw_arrow(
        buffer,
        layout.back,
        can_back,
        hover == Some(ChromeRegion::Back),
        false,
    );
    draw_arrow(
        buffer,
        layout.forward,
        can_fwd,
        hover == Some(ChromeRegion::Forward),
        true,
    );

    // Reload (circle arc).
    {
        let rect = layout.reload;
        let color = if hover == Some(ChromeRegion::Reload) {
            btn_hover
        } else {
            btn_color
        };
        let rx = to_phys(rect.0) as i32;
        let ry = to_phys(rect.1) as i32;
        let rw = to_phys(rect.2) as i32;
        let rh = to_phys(rect.3) as i32;
        let cx = rx + rw / 2;
        let cy = ry + rh / 2;
        let r = (rw.min(rh) as f32 / 3.0) as i32;
        let steps = 24;
        for i in 0..steps {
            let a = (i as f32) * std::f32::consts::TAU / steps as f32;
            let x = cx + (a.cos() * r as f32) as i32;
            let y = cy + (a.sin() * r as f32) as i32;
            plot(buffer, buf_w, x as usize, y as usize, color);
        }
    }

    // Address bar background + border.
    let ax = to_phys(layout.address.0);
    let ay = to_phys(layout.address.1);
    let aw = to_phys(layout.address.2);
    let ah = to_phys(layout.address.3);
    for y in 0..ah {
        for x in 0..aw {
            let bx = ax + x;
            let by = ay + y;
            if by < chrome_h {
                let idx = by * buf_w + bx;
                if idx < buffer.len() {
                    let on_border = x == 0 || x == aw - 1 || y == 0 || y == ah - 1;
                    buffer[idx] = if on_border { addr_border } else { addr_bg };
                }
            }
        }
    }

    // Render the address-bar text via the same Vello pipeline used for page
    // content, then alpha-blend it onto the chrome bar. This gives crisp,
    // real (not indicator-bar) text at the display's full resolution.
    let text_pad = to_phys(6.0);
    let avail_w = aw.saturating_sub(text_pad * 2);
    if !display.is_empty() && avail_w > 4 {
        // Render at physical resolution; the strip is (avail_w x ah).
        if let Some(glyphs) = render_text_strip(display, avail_w, ah, scale) {
            let gw = glyphs.width;
            let gh = glyphs.height;
            // Visible text width (clip to available).
            let vis_w = gw.min(avail_w);
            for ty in 0..gh.min(ah) {
                for tx in 0..vis_w {
                    let sidx = (ty * gw + tx) * 4;
                    if sidx + 3 >= glyphs.rgba.len() {
                        continue;
                    }
                    let a = glyphs.rgba[sidx + 3] as u32;
                    if a == 0 {
                        continue;
                    }
                    let bx = ax + text_pad + tx;
                    let by = ay + ty;
                    if by < chrome_h && bx < buf_w {
                        let idx = by * buf_w + bx;
                        if idx < buffer.len() {
                            let tr = glyphs.rgba[sidx] as u32;
                            let tg = glyphs.rgba[sidx + 1] as u32;
                            let tb = glyphs.rgba[sidx + 2] as u32;
                            let dst = buffer[idx];
                            let dr = (dst >> 16) & 0xFF;
                            let dg = (dst >> 8) & 0xFF;
                            let db = dst & 0xFF;
                            let nr = (tr * a + dr * (255 - a)) / 255;
                            let ng = (tg * a + dg * (255 - a)) / 255;
                            let nb = (tb * a + db * (255 - a)) / 255;
                            buffer[idx] = (nr << 16) | (ng << 8) | nb;
                        }
                    }
                }
            }
            // Caret just past the rendered text.
            if focused && caret_on {
                let cx = ax + text_pad + vis_w + 1;
                for y in 3..ah.saturating_sub(3) {
                    let by = ay + y;
                    if by < chrome_h && cx < buf_w {
                        let idx = by * buf_w + cx;
                        if idx < buffer.len() {
                            buffer[idx] = accent;
                        }
                    }
                }
            }
        }
    }
}

fn plot(buffer: &mut [u32], buf_w: usize, x: usize, y: usize, color: u32) {
    if (0..buf_w).contains(&x) {
        let idx = y * buf_w + x;
        if idx < buffer.len() {
            buffer[idx] = color;
        }
    }
}

/// A rendered text strip (RGBA) plus its dimensions.
struct TextStrip {
    rgba: Vec<u8>,
    width: usize,
    height: usize,
}

/// Render a single line of text into an RGBA buffer at the given size, using
/// the same blitz + Vello pipeline as page content. Returns `None` if the
/// rasterizer produces no content. The buffer is `width × height` RGBA8.
///
/// `scale` is the window scale factor; the strip is rendered at physical
/// resolution so text stays sharp on HiDPI displays.
fn render_text_strip(text: &str, width: usize, height: usize, scale: f32) -> Option<TextStrip> {
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::shell::Viewport;

    if width == 0 || height == 0 {
        return None;
    }
    // Render the text in a transparent document. We use a div sized to the
    // strip so the layout matches the chrome address bar.
    let html = format!(
        "<!DOCTYPE html><html><head><style>\
         html,body{{margin:0;padding:0;background:transparent;overflow:hidden;}}\
         .t{{font-family:system-ui,sans-serif;font-size:13px;color:#9aa5ce;\
         white-space:nowrap;padding:0;line-height:{h}px;}}\
         </style></head><body><div class=\"t\">{}</div></body></html>",
        crate::browser::escape_html(text),
        h = height
    );
    let viewport = Viewport {
        window_size: (width as u32, height as u32),
        hidpi_scale: scale,
        ..Default::default()
    };
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        ..Default::default()
    };
    let mut doc = HtmlDocument::from_html(&html, doc_config);
    doc.resolve(0.0);

    let mut frame = crate::Frame::new(width as u32, height as u32);
    use anyrender::ImageRenderer;
    let mut renderer = anyrender_vello_cpu::VelloCpuImageRenderer::new(width as u32, height as u32);
    renderer.render(
        |scene| {
            blitz_paint::paint_scene(
                scene,
                &mut doc,
                scale as f64,
                width as u32,
                height as u32,
                0,
                0,
            );
        },
        &mut frame.rgba,
    );

    // Vello renders onto a black background by default; we want transparency so
    // the strip alpha-blends over the chrome bar. Recover alpha: any pixel the
    // rasterizer lit (non-zero color) is treated as opaque text, everything
    // else transparent. This is a heuristic but works for solid-color text.
    for chunk in frame.rgba.chunks_exact_mut(4) {
        let lit = chunk[0] != 0 || chunk[1] != 0 || chunk[2] != 0;
        // If lit, set alpha to 255 (the text color is already there). Else 0.
        if !lit {
            // Clear to transparent (premultiplied black stays black, alpha 0).
            chunk[3] = 0;
        } else {
            chunk[3] = 255;
        }
    }

    Some(TextStrip {
        rgba: frame.rgba,
        width,
        height,
    })
}

// ── Built-in pages ──────────────────────────────────────────

fn loading_page(url: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><title>Loading…</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#1a1b26;color:#a9b1d6;padding:48px;text-align:center;}}\
         h1{{color:#7aa2f7;}}code{{color:#7dcfff;}}</style></head>\
         <body><h1>Loading…</h1><p><code>{}</code></p></body></html>",
        crate::browser::escape_html(url)
    )
}
