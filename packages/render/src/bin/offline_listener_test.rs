// Offline regression test for addEventListener via the persistent JsRuntime.
//
// A page has a <script> that registers a click listener on #btn which sets
// #out textContent. We bind the runtime (running the script), then fire a click
// on #btn and assert #out changed. No window, no mouse.
//
//   cargo run -p aris-render --features "desktop winit js" --bin offline_listener_test

use std::sync::Arc;

use aris_render::browser::{BrowserNavigationProvider, BrowserShellProvider, HttpNetProvider};

use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::Viewport;

fn main() {
    aris_render::init_logging();

    let html = r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><title>listener test</title>
<style>body{font-family:system-ui;}</style>
<script>
  document.getElementById('btn').addEventListener('click', function() {
    document.getElementById('out').setText('clicked via listener');
  });
</script>
</head><body>
<button id="btn">Click</button>
<div id="out">empty</div>
</body></html>"#;

    let viewport = Viewport {
        window_size: (800, 600),
        hidpi_scale: 1.0,
        ..Default::default()
    };
    let state = Arc::new(aris_render::browser::BrowserState::new());
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        net_provider: Some(Arc::new(HttpNetProvider::new())),
        navigation_provider: Some(Arc::new(BrowserNavigationProvider::new(Arc::clone(&state)))),
        shell_provider: Some(Arc::new(BrowserShellProvider::new(Arc::clone(&state)))),
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, doc_config);
    doc.resolve(0.0);

    // Bind the runtime + run the <script> (registers the listener).
    let mut rt = aris_render::js_runtime::JsRuntime::new();
    let scripts = aris_js::extract_scripts(html);
    rt.bind_and_run(&mut doc, &scripts.join("\n;\n"));

    // Resolve ids.
    let btn_id = doc
        .tree()
        .iter()
        .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
        .map(|(id, _)| id)
        .expect("no #btn");
    let out_id = doc
        .tree()
        .iter()
        .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
        .map(|(id, _)| id)
        .expect("no #out");

    let before = doc
        .get_node(out_id)
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out before click: {:?}", before);

    // Fire the click listener.
    rt.fire_click(&mut doc, btn_id as u32);

    let after = doc
        .get_node(out_id)
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out after click:  {:?}", after);
    if after != "clicked via listener" {
        println!("FAIL: expected 'clicked via listener', got {:?}", after);
        std::process::exit(2);
    }
    println!(
        "OK: addEventListener('click') fired and set #out to {:?}",
        after
    );
}
