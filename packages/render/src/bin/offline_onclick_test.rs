// Offline regression test for the interactive onclick JS bridge.
//
// Builds a document with a button whose onclick does
//   document.getElementById('out').textContent = 'Hello aris'
// then "clicks" it through the production path (`JsRuntime::fire_click`,
// the same entry the winit backend uses) and asserts the
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
<button id="add" onclick="var li = document.createElement('div'); li.textContent = 'Added by JS'; document.getElementById('list').appendChild(li)">Add</button>
<div id="out">empty</div>
<div id="list"></div>
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
            .map(|(nid, _)| nid.as_u64())
            .unwrap_or(u64::MAX)
    };
    let btn_id = find_by_id(&doc, "btn");
    let sty_id = find_by_id(&doc, "sty");
    let out_id = find_by_id(&doc, "out");
    assert!(btn_id != u64::MAX, "no #btn");
    assert!(sty_id != u64::MAX, "no #sty");
    assert!(out_id != u64::MAX, "no #out");

    let before = doc
        .get_node(blitz_traits::NodeId::from_u64(out_id))
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out before click: {:?}", before);

    // Drive the PRODUCTION click path. This used to call
    // `js_interactive::run_onclick`, a second bridge that no shipping path
    // invokes and whose contract differs (it rewrote `el.textContent = x`
    // into `setText(x)` in the source, which is how that gap stayed hidden).
    let mut rt = aris_render::js_runtime::JsRuntime::new();
    rt.bind_and_run(&mut doc, "");
    rt.fire_click(&mut doc, btn_id);
    let after = doc
        .get_node(blitz_traits::NodeId::from_u64(out_id))
        .map(|n| n.text_content())
        .unwrap_or_default();
    println!("#out after #btn click:  {:?}", after);
    if after != "Hello aris" {
        println!("FAIL: expected 'Hello aris', got {:?}", after);
        std::process::exit(2);
    }
    println!("OK: textContent onclick set #out to {:?}", after);

    // Now the style.cssText onclick on #sty, through the same production path.
    rt.fire_click(&mut doc, sty_id);
    let style_attr = doc
        .get_node(blitz_traits::NodeId::from_u64(out_id))
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

    // Run the createElement+appendChild onclick on #add and verify #list got a child.
    let add_id = find_by_id(&doc, "add");
    let list_id = find_by_id(&doc, "list");
    assert!(add_id != u64::MAX, "no #add");
    assert!(list_id != u64::MAX, "no #list");
    let list_children_before = doc
        .get_node(blitz_traits::NodeId::from_u64(list_id))
        .map(|n| n.children.len())
        .unwrap_or(0);
    rt.fire_click(&mut doc, add_id);
    let list_children_after = doc
        .get_node(blitz_traits::NodeId::from_u64(list_id))
        .map(|n| n.children.len())
        .unwrap_or(0);
    println!(
        "#list children: {} -> {}",
        list_children_before, list_children_after
    );
    // FIXED (this PR): `apply_ops` now DRAINS the op log, so the append in
    // one inline onclick lands exactly once, and further clicks add one each.
    // Before the drain, the log was replayed by every apply site — the append
    // landed twice on the first click and each further click added four more
    // (MUT: apply-ops-clone — restoring the clone instead of the drain reds
    //   the assertions below: children 2/6/10 instead of 1/2/3).
    //
    // Still a known limitation: the appended node carries no text, because a
    // detached `createElement` handle has no tree id and the `textContent`
    // setter only pushes an op when `read_handle_id` yields one — dropped at
    // the source (MUT: pending-settext — carrying the text into the pending
    //   tuple would make the assertions below assert a non-empty text).
    if list_children_after <= list_children_before {
        println!("FAIL: createElement+appendChild did not add a child to #list");
        std::process::exit(2);
    }
    if list_children_after != 1 {
        println!(
            "FAIL: expected exactly 1 appended child, got {}",
            list_children_after
        );
        std::process::exit(2);
    }
    for (i, &cid) in doc
        .get_node(blitz_traits::NodeId::from_u64(list_id))
        .map(|n| n.children.clone())
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let text = doc
            .get_node(cid)
            .map(|n| n.text_content())
            .unwrap_or_default();
        if text != "Added by JS" {
            println!("FAIL: child[{i}] text was {text:?}, expected \"Added by JS\"");
            std::process::exit(2);
        }
    }
    println!(
        "OK: createElement+appendChild landed exactly once, carrying its text (1 child, \"Added by JS\")"
    );
}
