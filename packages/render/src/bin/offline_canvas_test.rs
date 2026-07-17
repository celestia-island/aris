// Offline test for the Canvas2D Scene-recording model (no Boa, no window).
//
//   cargo run -p aris-render --features "js" --bin offline_canvas_test
//
// Canvas2D records into an anyrender::Scene instead of a raw pixel buffer.
// This test verifies scene commands and color parsing.

fn main() {
    let mut canvas = aris_render::canvas::Canvas2D::new(100, 100);

    assert!(
        !canvas.has_content(),
        "scene should be empty before any draw"
    );

    canvas.set_fill_style("red");
    canvas.fill_rect(10.0, 10.0, 50.0, 50.0);

    assert!(
        canvas.has_content(),
        "scene should have content after fill_rect"
    );

    let [r, g, b, a] = canvas.fill.components;
    println!(
        "fill color components = ({:.3}, {:.3}, {:.3}, {:.3})",
        r, g, b, a
    );
    assert!(r > 0.9 && g < 0.1 && b < 0.1, "expected red fill");

    canvas.clear_rect(0.0, 0.0, 100.0, 100.0);
    assert!(
        !canvas.has_content(),
        "scene should be empty after clearing full area"
    );

    // ── Color parsing ──
    let red = aris_render::canvas::Canvas2D::parse_color("#ff0000");
    let [rr, rg, rb, ra] = red.components;
    assert!((rr - 1.0).abs() < 0.01, "red component should be 1.0");
    assert!(rg < 0.01, "green component should be 0.0");
    assert!(rb < 0.01, "blue component should be 0.0");
    assert!((ra - 1.0).abs() < 0.01, "alpha should be 1.0");

    let red3 = aris_render::canvas::Canvas2D::parse_color("#f00");
    let [r3r, r3g, r3b, r3a] = red3.components;
    assert!((r3r - 1.0).abs() < 0.01);
    assert!(r3g < 0.01);
    assert!(r3b < 0.01);
    assert!((r3a - 1.0).abs() < 0.01);

    let blue = aris_render::canvas::Canvas2D::parse_color("blue");
    let [br, bg, bb, ba] = blue.components;
    assert!(br < 0.01);
    assert!((bg - 0.0).abs() < 0.01 || bg < 0.01);
    assert!((bb - 1.0).abs() < 0.01);
    assert!((ba - 1.0).abs() < 0.01);

    let white = aris_render::canvas::Canvas2D::parse_color("white");
    let [wr, wg, wb, wa] = white.components;
    assert!((wr - 1.0).abs() < 0.01);
    assert!((wg - 1.0).abs() < 0.01);
    assert!((wb - 1.0).abs() < 0.01);
    assert!((wa - 1.0).abs() < 0.01);

    println!("OK: Canvas2D scene recording and color parsing all work");
}
