// Demonstrate Boa JS execution + blitz rendering in a winit window.
//
// This HTML contains a <script> that uses document.write to inject dynamic
// content. With the `js` feature, aris-render executes the script via Boa
// before rendering, so the JS-generated content appears in the window.

fn main() {
    let html = r#"<!DOCTYPE html><html><head><style>
body { margin:0; background:#1e1e2e; font-family: sans-serif; }
h1 { color:#89b4fa; font-size:40px; margin:20px; }
#js-output { color:#a6e3a1; font-size:24px; margin:20px; padding:16px; background:#313244; border-radius:8px; }
</style></head><body>
<h1>aris-render + Boa JS</h1>
<div id="js-output">Loading...</div>
<script>
document.write("<p style='color:#f9e2af;font-size:20px;margin:20px'>This text was injected by Boa JS via document.write!</p>");
</script>
</body></html>"#;

    let config = aris_render::RenderConfig {
        width: 800,
        height: 600,
        scale: 1.0,
    };

    if let Err(e) = aris_render::winit_backend::run_window_with_js(html, &config) {
        eprintln!("Window error: {:?}", e);
        std::process::exit(1);
    }
}
