// SPDX-License-Identifier: BUSL-1.1

//! Browser shell services — networking, navigation, and history.
//!
//! This module wires the three `blitz_traits` providers that turn a static
//! HTML renderer into a working browser:
//!
//! - [`HttpNetProvider`] — fetches `<img>`, `<link rel=stylesheet>`, `@font-face`
//!   and other subresources over HTTP(S) or `file://`. Without it, blitz-dom's
//!   mutator issues `fetch` calls that go nowhere.
//! - [`BrowserNavigationProvider`] — receives link clicks and form submits from
//!   blitz-dom and triggers a top-level page load via [`BrowserState`].
//! - [`BrowserShellProvider`] — bridges `request_redraw`, window title, IME, and
//!   clipboard requests from blitz-dom back into the render loop.
//!
//! [`BrowserState`] is shared (`Arc`) between all three providers and the winit
//! event loop. Page loads produced by providers or the address bar are delivered
//! to the render loop through a `crossbeam`-style channel so the loop can swap
//! the active `HtmlDocument` on the main thread.

#![cfg(feature = "winit")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use bytes::Bytes;
use tracing::{debug, warn};
use url::Url;

use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::net::{NetHandler, NetProvider, Request};
use blitz_traits::shell::ShellProvider;

/// A request from a provider to the render loop to load a new document.
#[derive(Debug, Clone)]
pub struct LoadRequest {
    /// The HTML (or other text) to render.
    pub html: String,
    /// The canonical URL of the document — used as the base URL so relative
    /// links/resources resolve correctly, and shown in the address bar.
    pub url: Url,
}

/// Shared browser state: navigation history, the current URL, and a channel for
/// providers/threads to ask the render loop to load a new page.
///
/// The render loop holds one half of the channel (it polls
/// [`pending_loads`](Self::pending_loads) each frame); providers and fetch
/// threads push [`LoadRequest`]s through the other half.
pub struct BrowserState {
    /// Current document URL (last successfully loaded). Set by the render loop.
    current_url: Mutex<Option<Url>>,
    /// Full navigation history (URLs only).
    history: Mutex<Vec<Url>>,
    /// Index into `history` of the current entry.
    history_idx: Mutex<usize>,
    /// Outstanding page-load requests delivered from providers/threads.
    /// MPSC-style: pushers `lock().push()`, the loop `lock().drain()`.
    pending_loads: Mutex<Vec<LoadRequest>>,
    /// Set by [`BrowserShellProvider::request_redraw`] to wake the loop.
    pub redraw_requested: AtomicBool,
    /// True while a navigation is in flight (set on load_url, cleared on commit).
    pub navigating: AtomicBool,
    /// Latest window-title string pushed by the shell provider.
    pending_title: Mutex<Option<String>>,
    /// Latest IME state pushed by the shell provider.
    pending_ime: Mutex<Option<(bool, f32, f32, f32, f32)>>,
}

impl BrowserState {
    pub fn new() -> Self {
        Self {
            current_url: Mutex::new(None),
            history: Mutex::new(Vec::new()),
            history_idx: Mutex::new(0),
            pending_loads: Mutex::new(Vec::new()),
            redraw_requested: AtomicBool::new(false),
            navigating: AtomicBool::new(false),
            pending_title: Mutex::new(None),
            pending_ime: Mutex::new(None),
        }
    }

    /// The URL of the currently displayed document, if any.
    pub fn current_url(&self) -> Option<Url> {
        self.current_url.lock().unwrap().clone()
    }

    /// Drain any page-load requests queued by providers/threads. Called by the
    /// render loop each frame.
    pub fn drain_loads(&self) -> Vec<LoadRequest> {
        std::mem::take(&mut *self.pending_loads.lock().unwrap())
    }

    /// Take and clear the latest pending window title, if any.
    pub fn take_title(&self) -> Option<String> {
        self.pending_title.lock().unwrap().take()
    }

    /// Take and clear the latest pending IME area request.
    pub fn take_ime(&self) -> Option<(bool, f32, f32, f32, f32)> {
        self.pending_ime.lock().unwrap().take()
    }

    /// Record that a document was loaded at `url` (called by the render loop
    /// after it swaps in the new `HtmlDocument`).
    ///
    /// For a *forward* navigation (new entry) this truncates any forward-history
    /// and appends. For a *history* navigation (back/forward/reload), where
    /// `go_back`/`go_forward` already moved the index and the URL matches the
    /// entry at that index, we only update `current_url` without truncating —
    /// otherwise we'd wipe the forward history we just stepped back from.
    pub fn commit_load(&self, url: Url) {
        // Navigation completed — clear the loading indicator.
        self.navigating.store(false, Ordering::Relaxed);
        let mut history = self.history.lock().unwrap();
        let mut idx = self.history_idx.lock().unwrap();
        // History navigation: the index was moved by go_back/go_forward and the
        // URL matches the entry there. Don't truncate — just record current_url.
        if history.get(*idx).is_some_and(|entry| *entry == url) {
            *self.current_url.lock().unwrap() = Some(url);
            return;
        }
        // Forward navigation: drop any forward entries and append.
        history.truncate(*idx + 1);
        history.push(url.clone());
        *idx = history.len() - 1;
        *self.current_url.lock().unwrap() = Some(url);
    }

    /// Can we go back?
    pub fn can_go_back(&self) -> bool {
        let idx = *self.history_idx.lock().unwrap();
        idx > 0 && !self.history.lock().unwrap().is_empty()
    }

    /// Can we go forward?
    pub fn can_go_forward(&self) -> bool {
        let idx = *self.history_idx.lock().unwrap();
        let len = self.history.lock().unwrap().len();
        idx + 1 < len
    }

    /// Navigate back in history. Returns `true` if a load was queued.
    ///
    /// Takes `self: &Arc<Self>` so the load can hand a clone to a fetch thread.
    pub fn go_back(self: &Arc<Self>) -> bool {
        let history = self.history.lock().unwrap();
        let mut idx = self.history_idx.lock().unwrap();
        if *idx == 0 || history.is_empty() {
            return false;
        }
        *idx -= 1;
        let url = history[*idx].clone();
        drop(history);
        drop(idx);
        self.load_url(&url);
        true
    }

    /// Navigate forward in history. Returns `true` if a load was queued.
    pub fn go_forward(self: &Arc<Self>) -> bool {
        let history = self.history.lock().unwrap();
        let mut idx = self.history_idx.lock().unwrap();
        if *idx + 1 >= history.len() {
            return false;
        }
        *idx += 1;
        let url = history[*idx].clone();
        drop(history);
        drop(idx);
        self.load_url(&url);
        true
    }

    /// Reload the current URL. Returns `true` if a load was queued.
    pub fn reload(self: &Arc<Self>) -> bool {
        let Some(url) = self.current_url() else {
            return false;
        };
        self.load_url(&url);
        true
    }

    /// Begin loading `input`, interpreting it as a URL, file path, or search
    /// query. Called from the address bar (Enter) and external callers.
    pub fn navigate_input(self: &Arc<Self>, input: &str) {
        let url = normalize_input_to_url(input);
        self.load_url(&url);
    }

    /// Fetch a URL (possibly off-thread) and queue a [`LoadRequest`] for the
    /// render loop when the bytes arrive. History is committed by the render
    /// loop in [`commit_load`](Self::commit_load); for back/forward/reload the
    /// caller has already moved the history index.
    fn load_url(self: &Arc<Self>, url: &Url) {
        let scheme = url.scheme();
        // Mark navigation in flight for the loading indicator.
        self.navigating.store(true, Ordering::Relaxed);
        match scheme {
            "http" | "https" => {
                let url = url.clone();
                let state = Arc::clone(self);
                // Spawn a fetch thread so we never block the render loop.
                thread::spawn(move || match fetch_text(&url) {
                    Ok(html) => state.queue_load(html, url),
                    Err(e) => {
                        warn!("fetch {}: {}", url, e);
                        let err_html = error_page(url.as_ref(), &e);
                        state.queue_load(err_html, url);
                    }
                });
            }
            "file" => {
                let path = url
                    .to_file_path()
                    .unwrap_or_else(|_| PathBuf::from(url.path()));
                let url = url.clone();
                match std::fs::read_to_string(&path) {
                    Ok(html) => self.queue_load(html, url),
                    Err(e) => {
                        let msg = e.to_string();
                        warn!("file {}: {}", path.display(), msg);
                        let err_html = error_page(&path.display().to_string(), &msg);
                        self.queue_load(err_html, url);
                    }
                }
            }
            "about" => {
                let html = about_page(url.as_str());
                self.queue_load(html, url.clone());
            }
            "data" => {
                // Best-effort: strip the data: prefix and treat the payload as HTML.
                let body = url.as_str().trim_start_matches("data:");
                let html = body
                    .split_once(',')
                    .map(|(_mime, data)| data.to_string())
                    .unwrap_or_default();
                self.queue_load(html, url.clone());
            }
            _ => {
                warn!("unsupported scheme '{}': {}", scheme, url);
                let err_html = error_page(url.as_ref(), &format!("unsupported scheme '{scheme}'"));
                self.queue_load(err_html, url.clone());
            }
        }
    }

    /// Push a completed page load onto the channel for the render loop.
    fn queue_load(&self, html: String, url: Url) {
        self.pending_loads
            .lock()
            .unwrap()
            .push(LoadRequest { html, url });
        self.redraw_requested.store(true, Ordering::Relaxed);
    }
}

impl Default for BrowserState {
    fn default() -> Self {
        Self::new()
    }
}

// [`BrowserState`] is always held inside an `Arc<BrowserState>` shared between
// the render loop and the providers. The `go_back` / `go_forward` / `reload` /
// `navigate_input` / `load_url` methods therefore take `self: &Arc<Self>` so a
// clone can be handed to a spawned fetch thread.

/// HTTP + `file://` networking for blitz-dom subresources.
///
/// Each `fetch` spawns a thread that performs a blocking GET and delivers the
/// body to the handler. This keeps the render loop non-blocking without pulling
/// an async runtime into the renderer. Responses are cached in-memory keyed by
/// URL so that repeated subresource requests (reload, duplicate images) resolve
/// instantly, and cookies are persisted across requests via the shared client's
/// cookie store.
pub struct HttpNetProvider {
    /// Shared client so connection pooling + cookie storage kick in.
    client: reqwest::blocking::Client,
    /// In-memory response cache: URL -> body bytes. Held in an Arc so the
    /// spawned fetch threads can populate it.
    cache: Arc<Mutex<Vec<(String, Bytes)>>>,
}

impl HttpNetProvider {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .user_agent("aris/0.1 (like Gecko)")
            .cookie_store(true)
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            client,
            cache: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Cap the in-memory cache so it can't grow without bound.
    const CACHE_MAX: usize = 128;

    fn cache_get(cache: &Mutex<Vec<(String, Bytes)>>, url: &str) -> Option<Bytes> {
        cache
            .lock()
            .unwrap()
            .iter()
            .find(|(u, _)| u == url)
            .map(|(_, b)| b.clone())
    }

    fn cache_put(cache: &Mutex<Vec<(String, Bytes)>>, url: String, bytes: Bytes) {
        let mut c = cache.lock().unwrap();
        if c.iter().any(|(u, _)| u == &url) {
            return;
        }
        if c.len() >= Self::CACHE_MAX {
            c.remove(0);
        }
        c.push((url, bytes));
    }
}

impl Default for HttpNetProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl NetProvider for HttpNetProvider {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url.clone();
        // Cache hit: deliver immediately without a network round-trip.
        if let Some(cached) = Self::cache_get(&self.cache, url.as_str()) {
            debug!("net cache hit: {}", url);
            handler.bytes(url.to_string(), cached);
            return;
        }
        let client = self.client.clone();
        let cache = Arc::clone(&self.cache);
        // `handler` is `Send + Sync`, so we can move it into a thread.
        thread::spawn(move || {
            let bytes_result = load_url_bytes(&client, &url);
            match bytes_result {
                Ok(bytes) => {
                    Self::cache_put(&cache, url.to_string(), bytes.clone());
                    handler.bytes(url.to_string(), bytes);
                }
                Err(e) => {
                    debug!("net fetch {}: {}", url, e);
                    // blitz-dom has no error callback; we simply don't call
                    // `handler.bytes`, which leaves the resource unloaded.
                }
            }
        });
    }
}

/// Fetch the body of a URL as `Bytes`. Handles `http(s)` and `file://`.
fn load_url_bytes(client: &reqwest::blocking::Client, url: &Url) -> Result<Bytes, String> {
    match url.scheme() {
        "http" | "https" => {
            let resp = client.get(url.as_str()).send().map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("HTTP {}", resp.status()));
            }
            let bytes = resp.bytes().map_err(|e| e.to_string())?;
            Ok(bytes)
        }
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| "invalid file path".to_string())?;
            let data = std::fs::read(&path).map_err(|e| e.to_string())?;
            Ok(Bytes::from(data))
        }
        _ => Err(format!("unsupported scheme: {}", url.scheme())),
    }
}

/// Fetch a URL as UTF-8 text (for top-level document loads).
fn fetch_text(url: &Url) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("aris/0.1 (like Gecko)")
        .cookie_store(true)
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());
    match url.scheme() {
        "http" | "https" => {
            let resp = client.get(url.as_str()).send().map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("HTTP {}", resp.status()));
            }
            let text = resp.text().map_err(|e| e.to_string())?;
            Ok(text)
        }
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| "invalid file path".to_string())?;
            std::fs::read_to_string(&path).map_err(|e| e.to_string())
        }
        _ => Err(format!("unsupported scheme: {}", url.scheme())),
    }
}

/// Navigation provider: link clicks and form submits arrive here.
pub struct BrowserNavigationProvider {
    state: Arc<BrowserState>,
}

impl BrowserNavigationProvider {
    pub fn new(state: Arc<BrowserState>) -> Self {
        Self { state }
    }
}

impl NavigationProvider for BrowserNavigationProvider {
    fn navigate_to(&self, options: NavigationOptions) {
        debug!("navigate_to: {}", options.url);
        // Link clicks / form submits always push a new history entry.
        self.state.load_url(&options.url);
    }
}

/// Shell provider: bridges blitz-dom shell requests back to the render loop.
pub struct BrowserShellProvider {
    state: Arc<BrowserState>,
}

impl BrowserShellProvider {
    pub fn new(state: Arc<BrowserState>) -> Self {
        Self { state }
    }
}

impl ShellProvider for BrowserShellProvider {
    fn request_redraw(&self) {
        self.state.redraw_requested.store(true, Ordering::Relaxed);
    }

    fn set_window_title(&self, title: String) {
        *self.state.pending_title.lock().unwrap() = Some(title);
        self.state.redraw_requested.store(true, Ordering::Relaxed);
    }

    fn set_ime_enabled(&self, is_enabled: bool) {
        // Record (enabled, 0,0,0,0); the loop forwards to the winit window.
        self.state
            .pending_ime
            .lock()
            .unwrap()
            .replace((is_enabled, 0.0, 0.0, 0.0, 0.0));
    }

    fn set_ime_cursor_area(&self, x: f32, y: f32, width: f32, height: f32) {
        // Assume enabled when an area is set.
        self.state
            .pending_ime
            .lock()
            .unwrap()
            .replace((true, x, y, width, height));
    }
}

// ── URL normalization ───────────────────────────────────────

/// Turn arbitrary user input (from the address bar or CLI) into a `Url`.
///
/// - `http://` / `https://` / `file://` / `about:` / `data:` → as-is.
/// - A bare path that exists on disk → `file://`.
/// - Otherwise → treat as a web host if it contains a dot (add `https://`),
///   or a search query (Google).
pub fn normalize_input_to_url(input: &str) -> Url {
    let trimmed = input.trim();

    // Explicit scheme.
    if let Ok(url) = Url::parse(trimmed)
        && matches!(
            url.scheme(),
            "http" | "https" | "file" | "about" | "data" | "ftp"
        )
    {
        return url;
    }

    // Existing local file path.
    let p = Path::new(trimmed);
    if p.exists()
        && let Ok(url) = Url::from_file_path(canonicalize_path(p))
    {
        return url;
    }

    // Looks like a host (contains a dot, no spaces) → https://.
    let looks_like_host = !trimmed.contains(' ')
        && trimmed.contains('.')
        && !trimmed.starts_with('/')
        && !trimmed.contains(char::is_whitespace);
    if looks_like_host && let Ok(url) = Url::parse(&format!("https://{}", trimmed)) {
        return url;
    }

    // Fallback: a web search.
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", trimmed)
        .finish();
    Url::parse(&format!("https://www.google.com/search?{}", q))
        .unwrap_or_else(|_| Url::parse("about:blank").unwrap())
}

/// Resolve `.`/`..` in a path for a stable `file://` URL.
fn canonicalize_path(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ── Built-in pages ──────────────────────────────────────────

/// Escape a string for safe interpolation into HTML text content.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render an `about:` or `data:` URL to inline HTML (synchronously, no fetch).
pub fn about_or_data_html(url: &Url) -> String {
    match url.scheme() {
        "data" => {
            let body = url.as_str().trim_start_matches("data:");
            body.split_once(',')
                .map(|(_mime, data)| data.to_string())
                .unwrap_or_default()
        }
        _ => about_page(url.as_str()),
    }
}

fn error_page(url: &str, err: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Load error</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#1a1b26;color:#a9b1d6;padding:48px;}}\
         h1{{color:#f7768e;}}code{{background:#24283b;padding:2px 6px;border-radius:4px;color:#7dcfff;}}</style></head>\
         <body><h1>Unable to load page</h1>\
         <p>aris could not load <code>{}</code>.</p>\
         <p><code>{}</code></p>\
         <p>Check the URL or your network connection and try again.</p></body></html>",
        escape_html(url),
        escape_html(err)
    )
}

fn about_page(url: &str) -> String {
    if url == "about:blank" {
        return "<!DOCTYPE html><html><head><title>about:blank</title></head><body></body></html>"
            .to_string();
    }
    format!(
        "<!DOCTYPE html><html><head><title>aris — about</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#1a1b26;color:#a9b1d6;padding:48px;}}\
         h1{{color:#7aa2f7;}}code{{background:#24283b;padding:2px 6px;border-radius:4px;color:#7dcfff;}}</style></head>\
         <body><h1>{}</h1><p>This is the aris browser engine.</p></body></html>",
        escape_html(url)
    )
}
