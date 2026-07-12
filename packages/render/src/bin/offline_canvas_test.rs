// Offline test for the Canvas2D backing store (no Boa, no window).
//
//   cargo run -p aris-render --features "js" --bin offline_canvas_test

fn main() {
    let mut canvas = aris_render::canvas::Canvas2D::new(100, 100);

    // Fill with red.
    canvas.set_fill_style("red");
    canvas.fill_rect(10.0, 10.0, 50.0, 50.0);

    // Check pixel at (20, 20) — should be red.
    let idx = ((20 * 100 + 20) * 4) as usize;
    let r = canvas.rgba[idx];
    let g = canvas.rgba[idx + 1];
    let b = canvas.rgba[idx + 2];
    println!("pixel(20,20) = RGB({},{},{})", r, g, b);
    assert!(
        r > 200 && g < 50 && b < 50,
        "expected red, got RGB({},{},{})",
        r,
        g,
        b
    );

    // Check pixel at (80, 80) — should be transparent (0,0,0,0).
    let idx2 = ((80 * 100 + 80) * 4) as usize;
    let a = canvas.rgba[idx2 + 3];
    println!("pixel(80,80) alpha = {}", a);
    assert_eq!(a, 0, "expected transparent");

    // Clear a sub-rect.
    canvas.clear_rect(15.0, 15.0, 10.0, 10.0);
    let idx3 = ((17 * 100 + 17) * 4) as usize;
    let a3 = canvas.rgba[idx3 + 3];
    println!("pixel(17,17) after clear alpha = {}", a3);
    assert_eq!(a3, 0, "expected transparent after clearRect");

    // Test color parsing.
    assert_eq!(
        aris_render::canvas::Canvas2D::parse_color("#ff0000"),
        [255, 0, 0, 255]
    );
    assert_eq!(
        aris_render::canvas::Canvas2D::parse_color("#f00"),
        [255, 0, 0, 255]
    );
    assert_eq!(
        aris_render::canvas::Canvas2D::parse_color("blue"),
        [0, 0, 255, 255]
    );
    assert_eq!(
        aris_render::canvas::Canvas2D::parse_color("white"),
        [255, 255, 255, 255]
    );

    println!("OK: Canvas2D fillRect, clearRect, and color parsing all work");
}
