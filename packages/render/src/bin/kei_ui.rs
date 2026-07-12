// kei_ui — full-screen aris-rendered UI for the kei OS.
fn main() {
    aris_render::init_logging();
    let html = r#"<!DOCTYPE html><html><head><style>
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
    background: #282C34;
    font-family: 'DejaVu Sans', sans-serif;
    color: #ABB2BF;
    height: 100vh;
    overflow: hidden;
}
.header {
    background: #61AFEF;
    height: 60px;
    display: flex;
    align-items: center;
    padding: 0 24px;
}
.header h1 { color: white; font-size: 24px; }
.content {
    padding: 24px;
}
.card {
    background: #21252B;
    border-radius: 8px;
    padding: 20px;
    margin-bottom: 16px;
}
.card h2 { color: #61AFEF; font-size: 20px; margin-bottom: 8px; }
.card p { color: #ABB2BF; font-size: 14px; line-height: 1.5; }
.stat {
    display: inline-block;
    background: #E06C75;
    color: white;
    padding: 8px 16px;
    border-radius: 4px;
    margin: 4px;
    font-weight: bold;
}
</style></head><body>
<div class="header"><h1>kei OS</h1></div>
<div class="content">
<div class="card">
<h2>System Status</h2>
<p>aris-render pipeline: Blitz DOM + Vello CPU rasterization</p>
</div>
<div class="card">
<h2>Resources</h2>
<span class="stat">CPU 12%</span>
<span class="stat">MEM 256MB</span>
<span class="stat">NET 1.2G</span>
</div>
<div class="card">
<h2>Display</h2>
<p>Framebuffer: /dev/fb0 (virtio-gpu)</p>
</div>
</div>
</body></html>"#;

    let config = aris_render::RenderConfig {
        width: 640,
        height: 480,
        scale: 1.0,
    };

    tracing::info!("rendering 640x480 UI...");
    let frame = match aris_render::render_html_with_font(html, &config) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("render error: {:?}", e);
            std::process::exit(1);
        }
    };

    let total = (frame.width as usize) * (frame.height as usize);
    let non_black = frame
        .rgba
        .chunks_exact(4)
        .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
        .count();
    tracing::info!(
        "rendered: {}/{} non-black pixels ({:.1}%)",
        non_black,
        total,
        100.0 * non_black as f64 / total as f64
    );

    let fb_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/fb0".to_string());
    #[cfg(unix)]
    {
        if std::path::Path::new(&fb_path).exists() {
            tracing::info!("opening {}...", fb_path);
            match aris_render::FbDevBackend::open(&fb_path) {
                Ok(mut fb) => {
                    let (fw, fh) = fb.resolution();
                    tracing::info!("fb: {}x{}", fw, fh);
                    match fb.present(&frame) {
                        Ok(()) => tracing::info!("presented to {} OK", fb_path),
                        Err(e) => tracing::error!("present error: {}", e),
                    }
                }
                Err(e) => tracing::error!("fb open error: {}", e),
            }
        } else {
            tracing::warn!("{} not found, skipping fbdev", fb_path);
        }
    }

    tracing::info!("UI active. Keeping process alive...");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
