// SPDX-License-Identifier: BUSL-1.1

//! Canvas 2D API backing store using anyrender::Scene.
//!
//! Each `<canvas>` element gets a `Canvas2D` that records drawing commands into
//! an [`anyrender::Scene`]. The scene is then composited onto the page at the
//! canvas element's layout position during rendering.
//!
//! Supported operations: fillRect, clearRect, fillStyle (CSS color), strokeRect,
//! beginPath/arc/closePath/fill (path-based fills), and basic transforms
//! (translate/scale/rotate/save/restore).
//!
//! This replaces the earlier hand-written pixel buffer with the same vello-based
//! vector rendering pipeline blitz uses for page content.

#![cfg(feature = "js")]

use anyrender::{PaintScene, Scene};
use kurbo::{Affine, BezPath, Circle, Rect, Shape};
use peniko::{Color, Fill};

/// A Canvas 2D backing store: records into an anyrender::Scene.
pub struct Canvas2D {
    pub width: u32,
    pub height: u32,
    /// The recorded scene (drawing commands).
    scene: Scene,
    /// Current fill color.
    pub fill: Color,
    /// Current stroke color.
    pub stroke: Color,
    /// Current transform stack.
    transforms: Vec<Affine>,
    /// Current path being built (for beginPath/arc/fill).
    path: BezPath,
}

impl Canvas2D {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            scene: Scene::new(),
            fill: peniko::color::Rgba8 {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            }
            .into(),
            stroke: peniko::color::Rgba8 {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            }
            .into(),
            transforms: vec![Affine::IDENTITY],
            path: BezPath::new(),
        }
    }

    /// Get the current transform.
    fn current_transform(&self) -> Affine {
        *self.transforms.last().unwrap_or(&Affine::IDENTITY)
    }

    /// Parse a CSS color string into a peniko Color.
    pub fn parse_color(s: &str) -> Color {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            match hex.len() {
                6 => {
                    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
                    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
                    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
                    peniko::color::Rgba8 { r, g, b, a: 255 }.into()
                }
                3 => {
                    let r = u8::from_str_radix(&format!("{}{}", &hex[0..1], &hex[0..1]), 16)
                        .unwrap_or(0);
                    let g = u8::from_str_radix(&format!("{}{}", &hex[1..2], &hex[1..2]), 16)
                        .unwrap_or(0);
                    let b = u8::from_str_radix(&format!("{}{}", &hex[2..3], &hex[2..3]), 16)
                        .unwrap_or(0);
                    peniko::color::Rgba8 { r, g, b, a: 255 }.into()
                }
                _ => peniko::color::Rgba8 {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                }
                .into(),
            }
        } else {
            match s.to_lowercase().as_str() {
                "black" => peniko::color::Rgba8 {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                }
                .into(),
                "white" => peniko::color::Rgba8 {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                }
                .into(),
                "red" => peniko::color::Rgba8 {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                }
                .into(),
                "green" => peniko::color::Rgba8 {
                    r: 0,
                    g: 128,
                    b: 0,
                    a: 255,
                }
                .into(),
                "blue" => peniko::color::Rgba8 {
                    r: 0,
                    g: 0,
                    b: 255,
                    a: 255,
                }
                .into(),
                "yellow" => peniko::color::Rgba8 {
                    r: 255,
                    g: 255,
                    b: 0,
                    a: 255,
                }
                .into(),
                "cyan" => peniko::color::Rgba8 {
                    r: 0,
                    g: 255,
                    b: 255,
                    a: 255,
                }
                .into(),
                "magenta" => peniko::color::Rgba8 {
                    r: 255,
                    g: 0,
                    b: 255,
                    a: 255,
                }
                .into(),
                "gray" | "grey" => peniko::color::Rgba8 {
                    r: 128,
                    g: 128,
                    b: 128,
                    a: 255,
                }
                .into(),
                "orange" => peniko::color::Rgba8 {
                    r: 255,
                    g: 165,
                    b: 0,
                    a: 255,
                }
                .into(),
                "purple" => peniko::color::Rgba8 {
                    r: 128,
                    g: 0,
                    b: 128,
                    a: 255,
                }
                .into(),
                "transparent" => peniko::color::Rgba8 {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                }
                .into(),
                _ => peniko::color::Rgba8 {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                }
                .into(),
            }
        }
    }

    /// Fill a rectangle with the current fill color.
    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let rect = Rect::new(x, y, x + w, y + h);
        let tf = self.current_transform();
        self.scene.fill(Fill::EvenOdd, tf, self.fill, None, &rect);
    }

    /// Stroke a rectangle outline with the current stroke color.
    pub fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let rect = Rect::new(x, y, x + w, y + h);
        let tf = self.current_transform();
        let stroke = kurbo::Stroke::new(1.0);
        self.scene.stroke(&stroke, tf, self.stroke, None, &rect);
    }

    /// Clear a rectangle to transparent (resets the clip region).
    pub fn clear_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        // Canvas clearRect removes content in the region. With scene recording,
        // we simulate by pushing a clip layer excluding the cleared region, but
        // that's complex. For now, clear the entire scene (acceptable for
        // scripts that clearRect the whole canvas before redraw).
        let area = Rect::new(0.0, 0.0, self.width as f64, self.height as f64);
        if Rect::new(x, y, x + w, y + h).contains_rect(area) {
            self.scene.reset();
        }
    }

    /// Set the fill style from a CSS color string.
    pub fn set_fill_style(&mut self, color: &str) {
        self.fill = Self::parse_color(color);
    }

    /// Set the stroke style from a CSS color string.
    pub fn set_stroke_style(&mut self, color: &str) {
        self.stroke = Self::parse_color(color);
    }

    // ── Path operations ──

    pub fn begin_path(&mut self) {
        self.path = BezPath::new();
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        self.path.move_to((x, y));
    }

    pub fn line_to(&mut self, x: f64, y: f64) {
        self.path.line_to((x, y));
    }

    pub fn arc(&mut self, cx: f64, cy: f64, r: f64, _start: f64, _end: f64) {
        let circle = Circle::new((cx, cy), r);
        let path = circle.path_elements(0.1);
        // Approximate arc segment by adding circle path elements.
        for el in path {
            self.path.push(el);
        }
    }

    pub fn close_path(&mut self) {
        self.path.close_path();
    }

    pub fn fill_path(&mut self) {
        let tf = self.current_transform();
        self.scene
            .fill(Fill::EvenOdd, tf, self.fill, None, &self.path);
    }

    pub fn stroke_path(&mut self) {
        let tf = self.current_transform();
        let stroke = kurbo::Stroke::new(1.0);
        self.scene
            .stroke(&stroke, tf, self.stroke, None, &self.path);
    }

    // ── Transforms ──

    pub fn translate(&mut self, tx: f64, ty: f64) {
        let tf = self.current_transform();
        let new_tf = tf * Affine::translate((tx, ty));
        if let Some(last) = self.transforms.last_mut() {
            *last = new_tf;
        }
    }

    pub fn scale(&mut self, sx: f64, sy: f64) {
        let tf = self.current_transform();
        let new_tf = tf * Affine::scale_non_uniform(sx, sy);
        if let Some(last) = self.transforms.last_mut() {
            *last = new_tf;
        }
    }

    pub fn rotate(&mut self, angle: f64) {
        let tf = self.current_transform();
        let new_tf = tf * Affine::rotate(angle);
        if let Some(last) = self.transforms.last_mut() {
            *last = new_tf;
        }
    }

    pub fn save(&mut self) {
        self.transforms.push(self.current_transform());
    }

    pub fn restore(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    // ── Scene access ──

    /// Get the recorded scene for compositing.
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// Count filled pixels (for testing — approximated by checking if the
    /// scene has any Fill commands).
    pub fn has_content(&self) -> bool {
        !self.scene.commands.is_empty()
    }
}
