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
<button id="sty" onclick="document.getElementById('out').style.cssText = 'color:#ff0000'">Style</button>
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

    // Find #btn and #sty and #out by id.
    let find_by_id = |doc: &HtmlDocument, id: &str| {
        doc.tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
            .map(|(nid, _)| nid)
            .unwrap_or(usize::MAX)
    };
    let btn_id = find_by_id(&doc, "btn");
    let sty_id = find_by_id(&doc, "sty");
    let out_id = find_by_id(&doc, "out");
    assert!(btn_id != usize::MAX, "no #btn");
    assert!(sty_id != usize::MAX, "no #sty");
    assert!(out_id != usize::MAX, "no #out");

    let before = doc
        .get_node(out_id)
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out before click: {:?}", before);

    // Run the textContent onclick on #btn.
    let r = aris_render::js_interactive::run_onclick(&mut doc, btn_id);
    println!(
        "btn onclick: executed={} mutated={}",
        r.executed, r.dom_mutated
    );
    for e in &r.errors {
        println!("  [js] {}", e);
    }
    let after = doc
        .get_node(out_id)
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out after #btn click:  {:?}", after);
    if after != "Hello aris" {
        println!("FAIL: expected 'Hello aris', got {:?}", after);
        std::process::exit(2);
    }
    println!("OK: textContent onclick set #out to {:?}", after);

    // Now run the style.cssText onclick on #sty and verify #out got a style.
    let r2 = aris_render::js_interactive::run_onclick(&mut doc, sty_id);
    println!(
        "sty onclick: executed={} mutated={}",
        r2.executed, r2.dom_mutated
    );
    for e in &r2.errors {
        println!("  [js] {}", e);
    }
    let style_attr = doc
        .get_node(out_id)
        .and_then(|n| n.attr(blitz_dom::local_name!("style")))
        .map(|s| s.to_string())
        .unwrap_or_default();
    println!("#out style after #sty click: {:?}", style_attr);
    if !style_attr.contains("color") {
        println!("FAIL: expected a style with 'color', got {:?}", style_attr);
        std::process::exit(2);
    }
    println!(
        "OK: style.cssText onclick set #out style to {:?}",
        style_attr
    );
}
