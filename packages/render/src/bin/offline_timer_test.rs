// Offline regression test for async setTimeout.
//
// A <script> calls setTimeout(() => setText..., 10ms). We bind the runtime
// (which registers the timer), then sleep 50ms, then poll_timers and assert
// the callback fired and changed the DOM.
//
//   cargo run -p aris-render --features "desktop winit js" --bin offline_timer_test

use std::sync::Arc;

use aris_render::browser::{BrowserNavigationProvider, BrowserShellProvider, HttpNetProvider};

use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::Viewport;

fn main() {
    aris_render::init_logging();

    let html = r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><title>timer test</title>
<script>
  setTimeout(function() {
    document.getElementById('out').setText('timer fired');
  }, 10);
</script>
</head><body>
<div id="out">waiting</div>
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

    let mut rt = aris_render::js_runtime::JsRuntime::new();
    let scripts = aris_js::extract_scripts(html);
    rt.bind_and_run(&mut doc, &scripts.join("\n;\n"));

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
    println!("#out before poll: {:?}", before);
    assert_eq!(before, "waiting");

    // Wait for the timer to expire (10ms delay).
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Poll timers — should fire the setTimeout callback.
    let changed = rt.poll_timers(&mut doc);
    println!("poll_timers changed={}", changed);

    let after = doc
        .get_node(out_id)
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out after poll:  {:?}", after);

    if after != "timer fired" {
        println!("FAIL: expected 'timer fired', got {:?}", after);
        std::process::exit(2);
    }
    println!(
        "OK: setTimeout fired after delay and set #out to {:?}",
        after
    );
}
