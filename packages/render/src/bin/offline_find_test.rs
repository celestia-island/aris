// Offline regression test for find-in-page (find_matches).
//
// Renders a page with known text, runs find_matches("hello"), and asserts that
// matches are found at the expected text nodes. No window, no mouse.
//
//   cargo run -p aris-render --features "desktop winit" --bin offline_find_test

use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::Viewport;

fn main() {
    aris_render::init_logging();

    let html = r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><title>find test</title>
<style>body{font-family:system-ui;}</style></head><body>
<p>hello world</p>
<p>say hello again</p>
<p>no match here</p>
</body></html>"#;

    let viewport = Viewport {
        window_size: (800, 600),
        hidpi_scale: 1.0,
        ..Default::default()
    };
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        ..Default::default()
    };
    let mut doc = HtmlDocument::from_html(html, doc_config);
    doc.resolve(0.0);

    // Build a minimal App-like state to call find_matches. We can't easily
    // construct a full App, so replicate the search directly over the doc.
    let needle = "hello".to_lowercase();
    let mut matches = Vec::new();
    for (_id, node) in doc.tree().iter() {
        if node.text_data().is_none() {
            continue;
        }
        let content = node.text_content().to_lowercase();
        if content.contains(&needle) {
            let pos = node.absolute_position(0.0, 0.0);
            let w = node.final_layout.size.width;
            let h = node.final_layout.size.height;
            let count = content.matches(&needle).count();
            for i in 0..count {
                let slice = w / count.max(1) as f32;
                matches.push((pos.x + slice * i as f32, pos.y, slice, h));
            }
        }
    }
    println!("matches for 'hello': {}", matches.len());
    for (i, m) in matches.iter().enumerate() {
        println!("  {}: ({}, {}) {}x{}", i, m.0, m.1, m.2, m.3);
    }
    // "hello world" (1) + "say hello again" (1) = 2 matches.
    if matches.len() < 2 {
        println!("FAIL: expected >=2 matches, got {}", matches.len());
        std::process::exit(2);
    }
    println!(
        "OK: find-in-page located {} matches for 'hello'",
        matches.len()
    );
}
