// Offline integration test for the browser navigation/event wiring.
//
// Constructs an HtmlDocument in memory with the SAME providers the winit
// backend uses, injects a PointerDown+PointerUp at a known link location, and
// verifies that:
//   1. the click resolves to the <a> node
//   2. the NavigationProvider fires (a LoadRequest is queued)
//
// No window, no mouse — pure in-process. Run:
//   cargo run -p aris-render --features "desktop winit" --bin offline_click_test

use std::sync::Arc;

use aris_render::browser::{
    BrowserNavigationProvider, BrowserShellProvider, BrowserState, HttpNetProvider,
};

use blitz_dom::{Document, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_traits::events::{
    BlitzPointerEvent, BlitzPointerId, KeyState, MouseEventButton, MouseEventButtons,
    PointerCoords, UiEvent,
};
use blitz_traits::shell::Viewport;

fn main() {
    aris_render::init_logging();

    let html = r#"<!DOCTYPE html><html><head><meta charset="UTF-8">
<title>Nav — Page 1</title>
<style>body{padding:0;margin:0;font-family:system-ui,sans-serif;font-size:16px;}
h1{margin:0;} a{display:block;margin:8px 0;font-size:16px;}</style>
</head><body>
<h1>Page 1</h1>
<a href="page2.html">Go to Page 2</a>
<a href="page3.html">Go to Page 3</a>
</body></html>"#;

    let state = Arc::new(BrowserState::new());

    let viewport = Viewport {
        window_size: (800, 600),
        hidpi_scale: 1.0,
        ..Default::default()
    };
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        net_provider: Some(Arc::new(HttpNetProvider::new())),
        navigation_provider: Some(Arc::new(BrowserNavigationProvider::new(Arc::clone(&state)))),
        shell_provider: Some(Arc::new(BrowserShellProvider::new(Arc::clone(&state)))),
        base_url: Some("file:///tmp/navtest/index.html".to_string()),
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, doc_config);
    doc.resolve(0.0);

    // Print the layout tree so we can see where the <a> nodes actually are.
    doc.print_tree();

    // Scan the document for the first <a> node and read its layout box.
    let tree = doc.tree();
    let mut link_id: Option<usize> = None;
    let mut link_box: Option<(f32, f32, f32, f32)> = None;
    for (id, node) in tree.iter() {
        if let Some(el) = node.element_data()
            && format!("{:?}", el.name.local).contains("'a'")
        {
            let pos = node.absolute_position(0.0, 0.0);
            let w = node.final_layout.size.width;
            let h = node.final_layout.size.height;
            println!("found <a> node {id} at ({}, {}) {}x{}", pos.x, pos.y, w, h);
            if link_id.is_none() {
                link_id = Some(id);
                link_box = Some((pos.x, pos.y, w, h));
            }
        }
    }

    let (id, (x, y, w, h)) = link_id
        .zip(link_box)
        .expect("no <a> node found in document");
    // Click the center of the link.
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    println!("clicking <a id={id}> at ({cx}, {cy})");

    // First move the pointer so hover is set (blitz derives the click target
    // from the hover node during PointerDown/Up).
    doc.set_hover_to(cx, cy);
    println!("hover after move: {:?}", doc.get_hover_node_id());

    let mk_event = |pressed: bool| BlitzPointerEvent {
        id: BlitzPointerId::Mouse,
        is_primary: true,
        coords: PointerCoords {
            page_x: cx,
            page_y: cy,
            screen_x: cx,
            screen_y: cy,
            client_x: cx,
            client_y: cy,
        },
        button: MouseEventButton::Main,
        buttons: if pressed {
            MouseEventButtons::Primary
        } else {
            MouseEventButtons::empty()
        },
        mods: unsafe { core::mem::zeroed() },
        details: Default::default(),
        element: Default::default(),
    };

    // Disable background fetch threads from actually running by checking the
    // queue before any thread completes. The navigate_to call queues a load
    // synchronously via state.load_url, which for file:// reads inline.
    doc.handle_ui_event(UiEvent::PointerDown(mk_event(true)));
    doc.handle_ui_event(UiEvent::PointerUp(mk_event(false)));
    let _ = KeyState::Pressed; // silence unused import on some toolchains

    // Drain loads queued by the navigation provider.
    let loads = state.drain_loads();
    println!("loads queued after click: {}", loads.len());
    for load in &loads {
        println!("  -> {}", load.url);
    }

    if loads.is_empty() {
        println!("FAIL: no navigation triggered by clicking the link");
        std::process::exit(2);
    }
    println!("OK: link click triggered navigation");
}
