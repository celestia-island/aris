// kei_ui — full-screen aris-rendered UI for the kei OS.
//
// Renders a desktop-like HTML interface at the native framebuffer resolution
// (1280×800 on QEMU virtio-gpu) and writes it to /dev/fb0 using an embedded
// font (no system_fonts/fontconfig dependency).
//
// This is the init process for kei's graphical boot — the entire QEMU screen
// becomes the aris-rendered UI.
//
// Usage: kei_ui [/dev/fb0]

fn main() {
    // Full-screen kei desktop UI — 1280×800 to match the virtio-gpu resolution.
    let html = r#"<!DOCTYPE html><html><head><style>
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
    background: linear-gradient(135deg, #1a1b26 0%, #16161e 50%, #1f2335 100%);
    font-family: 'DejaVu Sans', sans-serif;
    color: #a9b1d6;
    height: 100vh;
    overflow: hidden;
    display: flex;
    flex-direction: column;
}
/* Top bar */
.topbar {
    background: #1f2335;
    height: 48px;
    display: flex;
    align-items: center;
    padding: 0 24px;
    border-bottom: 1px solid #414868;
    gap: 16px;
}
.topbar .logo {
    color: #7aa2f7;
    font-size: 18px;
    font-weight: bold;
}
.topbar .clock {
    margin-left: auto;
    color: #9aa5ce;
    font-size: 15px;
}
/* Main area */
.main {
    flex: 1;
    display: flex;
    padding: 24px;
    gap: 24px;
}
/* Sidebar */
.sidebar {
    width: 220px;
    background: #1f2335;
    border-radius: 16px;
    padding: 20px;
    display: flex;
    flex-direction: column;
    gap: 8px;
}
.sidebar-title {
    color: #565f89;
    font-size: 12px;
    text-transform: uppercase;
    margin-bottom: 8px;
    letter-spacing: 1px;
}
.sidebar-item {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 12px;
    border-radius: 8px;
    color: #c0caf5;
    font-size: 15px;
}
.sidebar-item.active {
    background: #7aa2f7;
    color: #1a1b26;
    font-weight: bold;
}
.sidebar-icon {
    width: 28px;
    height: 28px;
    border-radius: 6px;
    background: #414868;
    flex-shrink: 0;
}
.sidebar-item.active .sidebar-icon {
    background: #1a1b26;
}
/* Content cards */
.content {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 16px;
}
.welcome-card {
    background: #1f2335;
    border-radius: 16px;
    padding: 32px;
}
.welcome-card h1 {
    color: #bb9af7;
    font-size: 32px;
    margin-bottom: 12px;
}
.welcome-card p {
    color: #9aa5ce;
    font-size: 16px;
    line-height: 1.6;
}
/* Stats grid */
.stats {
    display: flex;
    gap: 16px;
}
.stat-card {
    flex: 1;
    background: #1f2335;
    border-radius: 12px;
    padding: 20px;
    text-align: center;
}
.stat-value {
    font-size: 28px;
    font-weight: bold;
}
.stat-label {
    color: #565f89;
    font-size: 12px;
    text-transform: uppercase;
    margin-top: 4px;
}
.stat-card.cpu .stat-value { color: #f7768e; }
.stat-card.mem .stat-value { color: #9ece6a; }
.stat-card.net .stat-value { color: #7dcfff; }
.stat-card.uptime .stat-value { color: #e0af68; }
/* App grid */
.apps {
    display: flex;
    gap: 12px;
    flex-wrap: wrap;
}
.app {
    width: 88px;
    text-align: center;
}
.app-icon {
    width: 64px;
    height: 64px;
    border-radius: 14px;
    margin: 0 auto 8px;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 28px;
}
.app-name {
    color: #9aa5ce;
    font-size: 12px;
}
</style></head><body>

<div class="topbar">
    <span class="logo">kei</span>
    <span style="color:#565f89;font-size:14px;">/ Desktop</span>
    <span class="clock">14:32</span>
</div>

<div class="main">
    <div class="sidebar">
        <div class="sidebar-title">Navigation</div>
        <div class="sidebar-item active">
            <div class="sidebar-icon"></div>
            Home
        </div>
        <div class="sidebar-item">
            <div class="sidebar-icon"></div>
            Files
        </div>
        <div class="sidebar-item">
            <div class="sidebar-icon"></div>
            Terminal
        </div>
        <div class="sidebar-item">
            <div class="sidebar-icon"></div>
            Settings
        </div>
        <div class="sidebar-item">
            <div class="sidebar-icon"></div>
            Browser
        </div>
    </div>

    <div class="content">
        <div class="welcome-card">
            <h1>Welcome to kei</h1>
            <p>An aris-rendered desktop interface running on the kei operating system.
            This entire screen is rendered by the aris HTML/CSS engine (Blitz + Vello CPU)
            and written directly to /dev/fb0.</p>
        </div>

        <div class="stats">
            <div class="stat-card cpu">
                <div class="stat-value">12%</div>
                <div class="stat-label">CPU</div>
            </div>
            <div class="stat-card mem">
                <div class="stat-value">256M</div>
                <div class="stat-label">Memory</div>
            </div>
            <div class="stat-card net">
                <div class="stat-value">1.2G</div>
                <div class="stat-label">Network</div>
            </div>
            <div class="stat-card uptime">
                <div class="stat-value">3:42</div>
                <div class="stat-label">Uptime</div>
            </div>
        </div>

        <div class="welcome-card">
            <h1 style="font-size:22px;">Applications</h1>
            <div class="apps" style="margin-top:16px;">
                <div class="app">
                    <div class="app-icon" style="background:#7aa2f7;">T</div>
                    <div class="app-name">Terminal</div>
                </div>
                <div class="app">
                    <div class="app-icon" style="background:#9ece6a;">F</div>
                    <div class="app-name">Files</div>
                </div>
                <div class="app">
                    <div class="app-icon" style="background:#bb9af7;">B</div>
                    <div class="app-name">Browser</div>
                </div>
                <div class="app">
                    <div class="app-icon" style="background:#f7768e;">S</div>
                    <div class="app-name">Settings</div>
                </div>
                <div class="app">
                    <div class="app-icon" style="background:#7dcfff;">E</div>
                    <div class="app-name">Editor</div>
                </div>
                <div class="app">
                    <div class="app-icon" style="background:#e0af68;">M</div>
                    <div class="app-name">Music</div>
                </div>
            </div>
        </div>
    </div>
</div>

</body></html>"#;

    let config = aris_render::RenderConfig {
        width: 1280,
        height: 800,
        scale: 1.0,
    };

    eprintln!("[kei_ui] rendering 1280x800 UI...");
    let frame = match aris_render::render_html_with_font(html, &config) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[kei_ui] render error: {:?}", e);
            std::process::exit(1);
        }
    };

    let total = (frame.width as usize) * (frame.height as usize);
    let non_black = frame
        .rgba
        .chunks_exact(4)
        .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
        .count();
    eprintln!(
        "[kei_ui] rendered: {}/{} non-black pixels ({:.1}%)",
        non_black,
        total,
        100.0 * non_black as f64 / total as f64
    );

    // Write to /dev/fb0
    let fb_path = std::env::args().nth(1).unwrap_or_else(|| "/dev/fb0".to_string());
    #[cfg(unix)]
    {
        if std::path::Path::new(&fb_path).exists() {
            eprintln!("[kei_ui] opening {}...", fb_path);
            match aris_render::FbDevBackend::open(&fb_path) {
                Ok(mut fb) => {
                    let (fw, fh) = fb.resolution();
                    eprintln!("[kei_ui] fb: {}x{}", fw, fh);
                    match fb.present(&frame) {
                        Ok(()) => eprintln!("[kei_ui] presented to {} OK", fb_path),
                        Err(e) => eprintln!("[kei_ui] present error: {}", e),
                    }
                }
                Err(e) => eprintln!("[kei_ui] fb open error: {}", e),
            }
        } else {
            eprintln!("[kei_ui] {} not found, skipping fbdev", fb_path);
        }
    }

    // Keep the process alive so the framebuffer isn't overwritten
    eprintln!("[kei_ui] UI active. Keeping process alive...");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
