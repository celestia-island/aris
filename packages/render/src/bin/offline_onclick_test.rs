// Offline regression test for the interactive onclick JS bridge.
//
// Builds a document with a button whose onclick does
//   document.getElementById('out').textContent = 'Hello aris'
// then "clicks" it (runs run_onclick on the button node) and asserts the
// output element's text changed. No window, no mouse.
//
//   cargo run -p aris-render --features "desktop winit js" --bin offline_onclick_test

use std::sync::Arc;

use aris_render::browser::{BrowserNavigationProvider, BrowserShellProvider, HttpNetProvider};

use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::Viewport;

fn main() {
    aris_render::init_logging();

    let html = r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><title>onclick test</title>
<style>body{font-family:system-ui;}</style></head><body>
<button id="btn" onclick="document.getElementById('out').textContent = 'Hello aris'">Click</button>
<div id="out">empty</div>
</body></html>"#;

    let viewport = Viewport {
        window_size: (800, 600),
        hidpi_scale: 1.0,
        ..Default::default()
    };
    // Providers are wired but unused for this test.
    let state = std::sync::Arc::new(aris_render::browser::BrowserState::new());
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        net_provider: Some(Arc::new(HttpNetProvider::new())),
        navigation_provider: Some(Arc::new(BrowserNavigationProvider::new(Arc::clone(&state)))),
        shell_provider: Some(Arc::new(BrowserShellProvider::new(Arc::clone(&state)))),
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, doc_config);
    doc.resolve(0.0);

    // Find the button node.
    let btn_id = doc
        .tree()
        .iter()
        .find(|(_, n)| {
            n.element_data()
                .map(|e| format!("{:?}", e.name.local).contains("'button'"))
                .unwrap_or(false)
        })
        .map(|(id, _)| id)
        .expect("no <button> found");
    let out_id = doc
        .tree()
        .iter()
        .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
        .map(|(id, _)| id)
        .expect("no #out found");

    let before = doc
        .get_node(out_id)
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out before click: {:?}", before);

    // Run the onclick handler as if the button were clicked.
    let r = aris_render::js_interactive::run_onclick(&mut doc, btn_id);
    println!("onclick: executed={} mutated={}", r.executed, r.dom_mutated);
    for e in &r.errors {
        println!("  [js] {}", e);
    }

    let after = doc
        .get_node(out_id)
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out after click:  {:?}", after);

    if after != "Hello aris" {
        println!("FAIL: expected 'Hello aris', got {:?}", after);
        std::process::exit(2);
    }
    println!("OK: onclick updated #out to {:?}", after);
}
