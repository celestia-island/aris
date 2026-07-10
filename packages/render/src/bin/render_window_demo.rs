// Open a desktop window rendering a known-good HTML test page with text.
//
// Unlike render_window (which loads the lagrange docs page whose CSS variables
// blitz doesn't fully resolve), this uses hardcoded-color HTML that blitz
// renders correctly — proving the winit + softbuffer window shows real text
// content, not just a background fill.

fn main() {
    let html = r#"<!DOCTYPE html><html><head><style>
body { margin:0; background:#282C34; font-family: sans-serif; }
h1 { color:#E06C75; font-size:48px; margin:20px; }
h2 { color:#61AFEF; font-size:32px; margin:16px 20px; }
p { color:#ABB2BF; font-size:20px; margin:12px 20px; }
.box { background:#61AFEF; width:300px; height:120px; margin:20px; border-radius:8px; }
.green { background:#98C379; color:#282C34; padding:12px; margin:20px; font-size:22px; }
</style></head><body>
<h1>aris-render</h1>
<h2>winit + softbuffer window</h2>
<p>This text is rendered by blitz-dom + Vello CPU, displayed in a native OS window.</p>
<div class="box"></div>
<div class="green">If you can read this, text rendering works.</div>
<p>HTML/CSS layout: Stylo cascade + Taffy flexbox/block.</p>
<p>Rasterization: anyrender_vello_cpu (pure Rust CPU).</p>
</body></html>"#;

    let config = aris_render::RenderConfig {
        width: 800,
        height: 600,
        scale: 1.0,
    };

    if let Err(e) = aris_render::winit_backend::run_window(html, &config) {
        eprintln!("Window error: {:?}", e);
        std::process::exit(1);
    }
}
