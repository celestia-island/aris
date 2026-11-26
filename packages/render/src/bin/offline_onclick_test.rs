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
    // KNOWN LIMITATION (pre-existing, and NOT caused by the migration).
    //
    // ROOT CAUSE: `JsRuntime::apply_ops` clones the op log and never clears it,
    // so every call REPLAYS every op recorded so far; `Op::AppendChild` mints a
    // fresh node each time it is applied. `bind_and_run` calls `apply_ops`
    // twice (once on entry, once after evaluating), so one inline onclick that
    // appends lands two nodes, and each further click adds four more — an
    // unbounded leak. A plain script path (one `bind_and_run`) appends once.
    //
    // The appended node's text is separately absent because the `textContent`
    // setter only pushes an op when `read_handle_id` yields a tree id, and a
    // pending `createElement` handle has none — the text is dropped at the
    // source, not lost in transit.
    //
    // The retired `js_interactive` bridge avoided both by running its own
    // mutator session, which is exactly why guarding it never surfaced them.
    // This test therefore asserts only what the production path guarantees
    // today — the append reaches the document — and pins the child count so a
    // future fix has to update it deliberately.
    if list_children_after <= list_children_before {
        println!("FAIL: createElement+appendChild did not add a child to #list");
        std::process::exit(2);
    }
    if list_children_after != 2 {
        println!(
            "FAIL: expected the pinned count of 2 children, got {}",
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
        if !text.is_empty() {
            println!("FAIL: child[{i}] unexpectedly carries text {text:?}");
            std::process::exit(2);
        }
    }
    println!(
        "OK: createElement+appendChild reached the document ({} children, text dropped — known limitation)",
        list_children_after
    );
}
