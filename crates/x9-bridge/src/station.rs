//! Next-station proposals: where the agent asks the person to set up next,
//! drawn on the current station's preview so they recognise the spot. A
//! marker ("set up here") on the ground, labels and boxes for what was seen,
//! what is hidden and the landmarks the agent's prose refers to. The bridge
//! draws the picture itself, so the chat, the web UI and a report all show the
//! same one. Coordinates are preview pixels, as for aiming.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use serde::Deserialize;
use serde_json::{json, Value};

static FONT: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Covered by this station (green).
    Seen,
    /// Not covered: what the next station is for (red).
    Hidden,
    /// Something to walk by (white).
    #[default]
    Landmark,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Marker {
    pub x: i64,
    pub y: i64,
    /// Under "SET UP HERE", e.g. "on the path, ~3 m past the corner".
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub x: i64,
    pub y: i64,
    pub text: String,
    #[serde(default)]
    pub kind: Kind,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Zone {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub kind: Kind,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Proposal {
    pub marker: Marker,
    /// The agent's directions in prose, by landmarks (shown with the picture).
    pub note: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub boxes: Vec<Zone>,
}

const MAX_ITEMS: usize = 12;
const MAX_TEXT: usize = 80;

impl Proposal {
    /// Everything inside a `w × h` preview, short texts, a note.
    pub fn check(&self, w: usize, h: usize) -> Result<(), String> {
        let inside = |x: i64, y: i64| (0..w as i64).contains(&x) && (0..h as i64).contains(&y);
        if !inside(self.marker.x, self.marker.y) {
            return Err(format!("the marker must lie within the {w}x{h} preview"));
        }
        if self.note.trim().is_empty() || self.note.chars().count() > 600 {
            return Err("note: the directions in prose, 1–600 characters".into());
        }
        if self.labels.len() + self.boxes.len() > MAX_ITEMS {
            return Err(format!("at most {MAX_ITEMS} labels and boxes together"));
        }
        if self.marker.label.chars().count() > MAX_TEXT {
            return Err(format!("marker.label: at most {MAX_TEXT} characters"));
        }
        for l in &self.labels {
            if !inside(l.x, l.y) || l.text.trim().is_empty() || l.text.chars().count() > MAX_TEXT {
                return Err(format!("label {:?}: inside the preview, 1–{MAX_TEXT} characters", l.text));
            }
        }
        for b in &self.boxes {
            if !inside(b.x0, b.y0) || !inside(b.x1, b.y1) || b.x0 >= b.x1 || b.y0 >= b.y1 || b.text.chars().count() > MAX_TEXT {
                return Err(format!("box {:?}: x0 < x1 and y0 < y1 inside the preview, text up to {MAX_TEXT} characters", b.text));
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        let kind = |k: Kind| match k {
            Kind::Seen => "seen",
            Kind::Hidden => "hidden",
            Kind::Landmark => "landmark",
        };
        json!({
            "marker": {"x": self.marker.x, "y": self.marker.y, "label": self.marker.label},
            "note": self.note,
            "labels": self.labels.iter().map(|l| json!({"x": l.x, "y": l.y, "text": l.text, "kind": kind(l.kind)})).collect::<Vec<_>>(),
            "boxes": self.boxes.iter().map(|b| json!({"x0": b.x0, "y0": b.y0, "x1": b.x1, "y1": b.y1, "text": b.text, "kind": kind(b.kind)})).collect::<Vec<_>>(),
        })
    }
}

const GREEN: [u8; 3] = [74, 222, 128];
const RED: [u8; 3] = [248, 113, 113];
const AMBER: [u8; 3] = [251, 191, 36];
const WHITE: [u8; 3] = [241, 245, 249];
const PANEL: [u8; 3] = [15, 23, 42];

fn colour(k: Kind) -> [u8; 3] {
    match k {
        Kind::Seen => GREEN,
        Kind::Hidden => RED,
        Kind::Landmark => WHITE,
    }
}

/// An RGB canvas with the few shapes the picture needs.
pub struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

/// The embedded label font.
pub fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT).expect("embedded font")
}

impl Canvas {
    pub fn new(w: usize, h: usize, bg: [u8; 3]) -> Canvas {
        Canvas { w, h, px: bg.iter().copied().cycle().take(w * h * 3).collect() }
    }
    pub fn png(&self) -> Vec<u8> {
        crate::measure::png_rgb(self.w, self.h, &self.px)
    }
    pub fn put(&mut self, x: i64, y: i64, c: [u8; 3], a: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 3;
        for (k, ck) in c.iter().enumerate() {
            self.px[i + k] = (self.px[i + k] as f32 * (1.0 - a) + *ck as f32 * a).round() as u8;
        }
    }
    fn fill(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, c: [u8; 3]) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.put(x, y, c, 1.0);
            }
        }
    }
    pub fn line(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32), c: [u8; 3], width: f32) {
        let n = ((x1 - x0).abs().max((y1 - y0).abs()) as usize).max(1);
        let r = (width / 2.0).max(0.5);
        for i in 0..=n {
            let t = i as f32 / n as f32;
            let (cx, cy) = (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
            for dy in -(r.ceil() as i64)..=(r.ceil() as i64) {
                for dx in -(r.ceil() as i64)..=(r.ceil() as i64) {
                    if ((dx * dx + dy * dy) as f32) <= r * r {
                        self.put(cx.round() as i64 + dx, cy.round() as i64 + dy, c, 1.0);
                    }
                }
            }
        }
    }
    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3], width: f32) {
        self.line((x0, y0), (x1, y0), c, width);
        self.line((x1, y0), (x1, y1), c, width);
        self.line((x1, y1), (x0, y1), c, width);
        self.line((x0, y1), (x0, y0), c, width);
    }
    pub fn ellipse(&mut self, (cx, cy): (f32, f32), (rx, ry): (f32, f32), c: [u8; 3], width: f32, filled: bool) {
        let steps = 180;
        let mut prev = (cx + rx, cy);
        for i in 1..=steps {
            let a = i as f32 / steps as f32 * std::f32::consts::TAU;
            let p = (cx + rx * a.cos(), cy + ry * a.sin());
            self.line(prev, p, c, width);
            prev = p;
        }
        if filled {
            for y in (cy - ry) as i64..=(cy + ry) as i64 {
                for x in (cx - rx) as i64..=(cx + rx) as i64 {
                    let (u, v) = ((x as f32 - cx) / rx, (y as f32 - cy) / ry);
                    if u * u + v * v <= 1.0 {
                        self.put(x, y, c, 1.0);
                    }
                }
            }
        }
    }
    /// Text on a dark panel; `anchor_x`: 0 left, 0.5 middle, 1 right of `x`.
    pub fn text(&mut self, font: &FontRef, size: f32, (x, y): (f32, f32), anchor_x: f32, s: &str, c: [u8; 3]) {
        let sf = font.as_scaled(PxScale::from(size));
        let width: f32 = s.chars().map(|ch| sf.h_advance(font.glyph_id(ch))).sum();
        let (ascent, descent) = (sf.ascent(), sf.descent());
        let pad = (size * 0.3).round();
        // Slide inwards rather than run off an edge.
        let left = (x - width * anchor_x).min(self.w as f32 - width - pad - 2.0).max(pad + 2.0);
        self.fill((left - pad) as i64, (y - pad * 0.6) as i64, (left + width + pad) as i64, (y + ascent - descent + pad * 0.6) as i64, PANEL);
        let mut pen = left;
        for ch in s.chars() {
            let id = font.glyph_id(ch);
            let g = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(pen, y + ascent));
            if let Some(o) = font.outline_glyph(g) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| self.put(b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64, c, cov));
            }
            pen += sf.h_advance(id);
        }
    }
}

/// The preview (grey, `pw × ph`) with the proposal drawn on it: cropped to the
/// annotated part (padded) and enlarged so text reads well. PNG bytes.
pub fn render(grey: &[u8], pw: usize, ph: usize, p: &Proposal) -> Vec<u8> {
    let font = FontRef::try_from_slice(FONT).expect("embedded font");
    // The region the proposal is about.
    let (mut x0, mut y0, mut x1, mut y1) = (p.marker.x, p.marker.y - 60, p.marker.x, p.marker.y + 40);
    for l in &p.labels {
        x0 = x0.min(l.x);
        x1 = x1.max(l.x);
        y0 = y0.min(l.y);
        y1 = y1.max(l.y);
    }
    for b in &p.boxes {
        x0 = x0.min(b.x0);
        x1 = x1.max(b.x1);
        y0 = y0.min(b.y0);
        y1 = y1.max(b.y1);
    }
    let pad = 120;
    let (x0, x1) = ((x0 - pad).max(0), (x1 + pad).min(pw as i64 - 1));
    let (y0, y1) = ((y0 - pad).max(0), (y1 + pad).min(ph as i64 - 1));
    let (cw, ch) = ((x1 - x0 + 1) as usize, (y1 - y0 + 1) as usize);
    let s = (2000.0 / cw as f32).clamp(1.0, 2.0);
    let (w, h) = ((cw as f32 * s) as usize, (ch as f32 * s) as usize);
    // Nearest-neighbour enlargement of the grey crop.
    let mut c = Canvas { w, h, px: vec![0; w * h * 3] };
    for y in 0..h {
        let sy = (y0 as usize + (y as f32 / s) as usize).min(ph - 1);
        for x in 0..w {
            let sx = (x0 as usize + (x as f32 / s) as usize).min(pw - 1);
            let g = grey[sy * pw + sx];
            let i = (y * w + x) * 3;
            c.px[i..i + 3].copy_from_slice(&[g, g, g]);
        }
    }
    let at = |x: i64, y: i64| ((x - x0) as f32 * s, (y - y0) as f32 * s);
    let size = (14.0 * s).clamp(16.0, 28.0);
    let stroke = (2.0 * s).max(2.0);
    for b in &p.boxes {
        let ((ax, ay), (bx, by)) = (at(b.x0, b.y0), at(b.x1, b.y1));
        c.rect(ax, ay, bx, by, colour(b.kind), stroke);
        if !b.text.is_empty() {
            c.text(&font, size, (ax, ay - size * 1.6), 0.0, &b.text, colour(b.kind));
        }
    }
    for l in &p.labels {
        let (lx, ly) = at(l.x, l.y);
        c.ellipse((lx, ly), (stroke * 1.5, stroke * 1.5), colour(l.kind), 1.0, true);
        c.text(&font, size, (lx + stroke * 3.0, ly - size * 0.6), 0.0, &l.text, colour(l.kind));
    }
    // The marker: a ring on the ground and a small tripod standing on it.
    let (mx, my) = at(p.marker.x, p.marker.y);
    let r = 20.0 * s.max(1.2);
    c.ellipse((mx, my), (r, r * 0.42), AMBER, stroke * 1.4, false);
    for dx in [-0.65f32, 0.0, 0.65] {
        c.line((mx, my - r * 1.75), (mx + dx * r, my - 1.0), AMBER, stroke * 1.2);
    }
    c.ellipse((mx, my - r * 2.05), (r * 0.28, r * 0.28), AMBER, 1.0, true);
    c.text(&font, size * 1.25, (mx, my + r * 0.75), 0.5, "SET UP HERE", AMBER);
    if !p.marker.label.is_empty() {
        c.text(&font, size, (mx, my + r * 0.75 + size * 2.3), 0.5, &p.marker.label, WHITE);
    }
    crate::measure::png_rgb(w, h, &c.px)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal() -> Proposal {
        serde_json::from_value(json!({
            "marker": {"x": 300, "y": 200, "label": "on the path, ~3 m past the corner"},
            "note": "Carry the tripod past the house corner, onto the path.",
            "labels": [{"x": 100, "y": 120, "text": "Shed door", "kind": "landmark"}],
            "boxes": [{"x0": 150, "y0": 80, "x1": 220, "y1": 140, "text": "Seen: the two windows", "kind": "seen"}]
        }))
        .unwrap()
    }

    #[test]
    fn checks_keep_it_inside_and_short() {
        let p = proposal();
        assert!(p.check(400, 300).is_ok());
        assert!(p.check(250, 300).unwrap_err().contains("marker"));
        let mut q = proposal();
        q.note = " ".into();
        assert!(q.check(400, 300).unwrap_err().contains("note"));
        let mut q = proposal();
        q.boxes[0].x1 = 140;
        assert!(q.check(400, 300).unwrap_err().contains("x0 < x1"));
    }

    #[test]
    fn draws_a_png_with_the_marker_on_it() {
        let (pw, ph) = (400usize, 300usize);
        let grey = vec![90u8; pw * ph];
        let png = render(&grey, pw, ph, &proposal());
        assert_eq!(&png[1..4], b"PNG");
        // A label at the far right edge still draws (slid inwards), without panicking.
        let mut edge = proposal();
        edge.labels.push(Label { x: 399, y: 10, text: "Hidden: everything past this corner".into(), kind: Kind::Hidden });
        assert_eq!(&render(&grey, pw, ph, &edge)[1..4], b"PNG");
        let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
        assert!(w >= 400, "{w}");
    }
}
