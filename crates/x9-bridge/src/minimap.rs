//! The minimap: a plain line drawing of the project so far, redrawn after
//! every station. Walls are lines (building walls bold, low walls thin,
//! vegetation faint green and dashed), stations are numbered dots with a tick
//! for the way each scanner faced (amber with "?" while not trusted), and
//! the mission's wall targets are drawn in their status colours. Stations
//! scanned but not yet placed are listed under the map. SVG for the web UI,
//! PNG for the chat.

use crate::register::{Face, Pose, Registration, Wall};

pub struct Minimap {
    pub walls: Vec<Wall>,
    /// Number, pose, counts for coverage.
    pub stations: Vec<(u16, Pose, bool)>,
    /// Target id, face, colour.
    pub faces: Vec<(String, Face, [u8; 3])>,
    /// Lines under the map: stations not placed, or still registering.
    pub notes: Vec<String>,
    pub title: String,
}

const BG: [u8; 3] = [17, 24, 39];
const GRID: [u8; 3] = [38, 46, 64];
const WALL: [u8; 3] = [226, 232, 240];
const TRUSTED: [u8; 3] = [255, 255, 255];
const UNTRUSTED: [u8; 3] = [251, 191, 36];
const TEXT: [u8; 3] = [203, 213, 225];
const LOW: [u8; 3] = [148, 163, 184];
const PLANT: [u8; 3] = [110, 200, 120];

/// How a wall run is drawn: colour, width, dashed.
fn style(kind: &str) -> ([u8; 3], f32, bool) {
    match kind {
        "low wall" => (LOW, 1.5, false),
        "vegetation" => (PLANT, 2.0, true),
        _ => (WALL, 3.0, false),
    }
}

impl Minimap {
    /// From a registration (or none yet), the mission's faces, and the
    /// project's stations that the registration doesn't hold yet.
    pub fn new(reg: Option<&Registration>, faces: Vec<(String, Face, [u8; 3])>, waiting: &[u16], registering: bool) -> Minimap {
        let mut notes = Vec::new();
        let (mut walls, mut stations) = (Vec::new(), Vec::new());
        if let Some(r) = reg {
            walls = r.walls.clone();
            for s in &r.stations {
                let a = s.audit.clone().unwrap_or_default();
                match s.pose {
                    Some(p) => stations.push((s.station, p, a.verdict.is_empty() || a.trusted())),
                    None => notes.push(format!(
                        "Station {}: not placed{}",
                        s.station,
                        a.reasons.first().map(|r| format!(" ({r})")).unwrap_or_default()
                    )),
                }
            }
        }
        for n in waiting {
            notes.push(format!("Station {n}: {}", if registering { "scanned, registering…" } else { "scanned, waiting to register" }));
        }
        let placed = stations.len();
        let count = |k: &str| walls.iter().filter(|w: &&Wall| style(&w.kind).0 == style(k).0).count();
        let title = match placed {
            0 => "No stations placed yet".to_string(),
            p => {
                let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
                let mut parts = vec![plural(p, "station", "stations"), plural(count("building"), "wall", "walls")];
                if count("low wall") > 0 {
                    parts.push(plural(count("low wall"), "low wall", "low walls"));
                }
                if count("vegetation") > 0 {
                    parts.push(plural(count("vegetation"), "run of vegetation", "runs of vegetation"));
                }
                format!("{} · station {}'s frame, not north", parts.join(" · "), reg.map(|r| r.frame_station()).unwrap_or(1))
            }
        };
        Minimap { walls, stations, faces, notes, title }
    }

    fn layout(&self) -> Layout {
        let mut pts: Vec<[f64; 2]> = Vec::new();
        for w in &self.walls {
            pts.push(w.from);
            pts.push(w.to);
        }
        for (_, p, _) in &self.stations {
            pts.push([p.xyz_m[0], p.xyz_m[1]]);
        }
        for (_, f, _) in &self.faces {
            pts.push(f.from);
            pts.push(f.to);
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in &pts {
            x0 = x0.min(p[0]);
            y0 = y0.min(p[1]);
            x1 = x1.max(p[0]);
            y1 = y1.max(p[1]);
        }
        if pts.is_empty() {
            (x0, y0, x1, y1) = (-5.0, -5.0, 5.0, 5.0);
        }
        // The margin is at least 1.5 m and at least MARGIN_PX, so a station's
        // number (drawn above and right of its dot) stays inside the picture
        // at any scale; on a large site 1.5 m is only a dozen pixels.
        let span = (x1 - x0).max(y1 - y0);
        let ppm = ((900.0 - 2.0 * MARGIN_PX) / span.max(1e-6)).clamp(8.0, 120.0);
        let pad = (1.5f64).max(MARGIN_PX / ppm);
        let (x0, y0, x1, y1) = (x0 - pad, y0 - pad, x1 + pad, y1 + pad);
        let grid = if (x1 - x0).max(y1 - y0) <= 25.0 { 1.0 } else { 5.0 };
        let top = 36.0;
        let w = ((x1 - x0) * ppm).ceil().max(420.0);
        let h = ((y1 - y0) * ppm).ceil() + top + 30.0 + 22.0 * self.notes.len() as f64;
        Layout { x0, y1, ppm, grid, top, w, map_h: (y1 - y0) * ppm, h }
    }

    pub fn svg(&self) -> String {
        let l = self.layout();
        let c = |v: [u8; 3]| format!("rgb({},{},{})", v[0], v[1], v[2]);
        let mut s = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:.0} {h:.0}" width="{w:.0}" height="{h:.0}" font-family="DejaVu Sans, Helvetica, Arial, sans-serif"><rect width="100%" height="100%" fill="{bg}"/>"#,
            w = l.w,
            h = l.h,
            bg = c(BG)
        );
        for (a, b) in l.grid_lines() {
            s += &format!(r#"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="{}" stroke-width="1"/>"#, a.0, a.1, b.0, b.1, c(GRID));
        }
        // Vegetation under walls, so a wall behind a hedge stays visible.
        let mut order: Vec<&Wall> = self.walls.iter().collect();
        order.sort_by_key(|w| match w.kind.as_str() { "vegetation" => 0, "low wall" => 1, _ => 2 });
        for wl in order {
            let (a, b) = (l.px(wl.from), l.px(wl.to));
            let (col, width, dashed) = style(&wl.kind);
            let dash = if dashed { r#" stroke-dasharray="4 4""# } else { "" };
            s += &format!(r#"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="{}" stroke-width="{width}" stroke-linecap="round"{dash}/>"#, a.0, a.1, b.0, b.1, c(col));
        }
        for (id, f, col) in &self.faces {
            let (a, b) = (l.px(f.from), l.px(f.to));
            s += &format!(r#"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="{}" stroke-width="6" stroke-linecap="round" opacity="0.85"/>"#, a.0, a.1, b.0, b.1, c(*col));
            s += &format!(r#"<text x="{:.1}" y="{:.1}" fill="{}" font-size="13" text-anchor="middle">{}</text>"#, (a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0 - 8.0, c(*col), esc(id));
        }
        for (n, p, ok) in &self.stations {
            let (x, y) = l.px([p.xyz_m[0], p.xyz_m[1]]);
            let col = c(if *ok { TRUSTED } else { UNTRUSTED });
            let t = p.turn_deg.to_radians();
            s += &format!(r#"<line x1="{x:.1}" y1="{y:.1}" x2="{:.1}" y2="{:.1}" stroke="{col}" stroke-width="2"/>"#, x + 15.0 * t.cos(), y - 15.0 * t.sin());
            s += &format!(r#"<circle cx="{x:.1}" cy="{y:.1}" r="6" fill="{col}"/>"#);
            s += &format!(r#"<text x="{:.1}" y="{:.1}" fill="{col}" font-size="15" font-weight="bold">{}{}</text>"#, x + 9.0, y - 9.0, n, if *ok { "" } else { "?" });
        }
        s += &format!(r#"<text x="12" y="23" fill="{}" font-size="14">{}</text>"#, c(TEXT), esc(&self.title));
        let scale = format!("grid {} m", l.grid);
        s += &format!(r#"<text x="12" y="{:.0}" fill="{}" font-size="12">{}</text>"#, l.top + l.map_h + 20.0, c(TEXT), scale);
        for (i, n) in self.notes.iter().enumerate() {
            s += &format!(r#"<text x="12" y="{:.0}" fill="{}" font-size="13">{}</text>"#, l.top + l.map_h + 42.0 + 22.0 * i as f64, c(UNTRUSTED), esc(n));
        }
        s + "</svg>"
    }

    pub fn png(&self) -> Vec<u8> {
        use crate::station::Canvas;
        let l = self.layout();
        let mut cv = Canvas::new(l.w as usize, l.h as usize, BG);
        let f = |p: (f64, f64)| (p.0 as f32, p.1 as f32);
        for (a, b) in l.grid_lines() {
            cv.line(f(a), f(b), GRID, 1.0);
        }
        let mut order: Vec<&Wall> = self.walls.iter().collect();
        order.sort_by_key(|w| match w.kind.as_str() { "vegetation" => 0, "low wall" => 1, _ => 2 });
        for wl in order {
            let (col, width, dashed) = style(&wl.kind);
            let (a, b) = (f(l.px(wl.from)), f(l.px(wl.to)));
            if dashed {
                let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
                let n = (len / 8.0).floor() as i32;
                for k in (0..n).step_by(2) {
                    let (t0, t1) = (k as f32 / n as f32, ((k + 1) as f32 / n as f32).min(1.0));
                    cv.line((a.0 + (b.0 - a.0) * t0, a.1 + (b.1 - a.1) * t0), (a.0 + (b.0 - a.0) * t1, a.1 + (b.1 - a.1) * t1), col, width);
                }
            } else {
                cv.line(a, b, col, width);
            }
        }
        let font = crate::station::font();
        for (id, fc, col) in &self.faces {
            let (a, b) = (f(l.px(fc.from)), f(l.px(fc.to)));
            cv.line(a, b, *col, 6.0);
            cv.text(&font, 14.0, ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0 - 24.0), 0.5, id, *col);
        }
        for (n, p, ok) in &self.stations {
            let (x, y) = f(l.px([p.xyz_m[0], p.xyz_m[1]]));
            let col = if *ok { TRUSTED } else { UNTRUSTED };
            let t = p.turn_deg.to_radians() as f32;
            cv.line((x, y), (x + 15.0 * t.cos(), y - 15.0 * t.sin()), col, 2.0);
            cv.ellipse((x, y), (6.0, 6.0), col, 1.0, true);
            cv.text(&font, 16.0, (x + 9.0, y - 28.0), 0.0, &format!("{n}{}", if *ok { "" } else { "?" }), col);
        }
        cv.text(&font, 15.0, (12.0, 8.0), 0.0, &self.title, TEXT);
        cv.text(&font, 13.0, (12.0, (l.top + l.map_h + 6.0) as f32), 0.0, &format!("grid {} m", l.grid), TEXT);
        for (i, n) in self.notes.iter().enumerate() {
            cv.text(&font, 14.0, (12.0, (l.top + l.map_h + 28.0 + 22.0 * i as f64) as f32), 0.0, n, UNTRUSTED);
        }
        cv.png()
    }
}

/// Room around the drawing for station numbers and face labels, in pixels.
const MARGIN_PX: f64 = 40.0;

struct Layout {
    x0: f64,
    y1: f64,
    ppm: f64,
    grid: f64,
    top: f64,
    w: f64,
    map_h: f64,
    h: f64,
}

impl Layout {
    /// Plan metres to picture pixels (y up becomes y down).
    fn px(&self, p: [f64; 2]) -> (f64, f64) {
        ((p[0] - self.x0) * self.ppm, self.top + (self.y1 - p[1]) * self.ppm)
    }

    fn grid_lines(&self) -> Vec<((f64, f64), (f64, f64))> {
        let mut out = Vec::new();
        let x1 = self.x0 + self.w / self.ppm;
        let y0 = self.y1 - self.map_h / self.ppm;
        let mut g = (self.x0 / self.grid).ceil() * self.grid;
        while g < x1 {
            let (px, _) = self.px([g, 0.0]);
            out.push(((px, self.top), (px, self.top + self.map_h)));
            g += self.grid;
        }
        let mut g = (y0 / self.grid).ceil() * self.grid;
        while g < self.y1 {
            let (_, py) = self.px([0.0, g]);
            out.push(((0.0, py), (self.w, py)));
            g += self.grid;
        }
        out
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::register::{Audit, StationPose};

    fn reg() -> Registration {
        let st = |n: u16, x: f64, verdict: &str, placed: bool| StationPose {
            scan_id: format!("s{n}"),
            station: n,
            pose: placed.then_some(Pose { turn_deg: 90.0, xyz_m: [x, 0.0, 0.0] }),
            audit: Some(Audit { verdict: verdict.into(), reasons: if placed { vec![] } else { vec!["it shares too little wall".into()] }, ..Default::default() }),
        };
        Registration {
            project_id: "p".into(),
            frame_scan_id: Some("s1".into()),
            stations: vec![st(1, 0.0, "confirmed", true), st(2, 3.0, "doubtful", true), st(3, 0.0, "unplaced", false)],
            pairs: vec![],
            walls: vec![
                Wall { wall_id: "w1".into(), from: [-1.0, 2.0], to: [4.0, 2.0], length_m: 5.0, z_m: [-1.5, 1.2], seen_by: vec![1, 2],
                       kind: "building".into(), flatness: 0.9, height_m: Some(2.7) },
                Wall { wall_id: "w2".into(), from: [-1.0, 1.5], to: [4.0, 1.5], length_m: 5.0, z_m: [-1.5, 0.0], seen_by: vec![1],
                       kind: "vegetation".into(), flatness: 0.2, height_m: Some(1.5) },
            ],
            summary: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn draws_walls_stations_and_what_is_not_placed() {
        let faces = vec![("north<wall>".to_string(), Face { from: [-1.0, 2.0], to: [4.0, 2.0] }, [74, 222, 128])];
        let m = Minimap::new(Some(&reg()), faces, &[4], true);
        assert_eq!(m.stations.len(), 2);
        assert!(m.stations[0].2 && !m.stations[1].2, "a doubtful station is drawn as not trusted");
        assert_eq!(m.notes, ["Station 3: not placed (it shares too little wall)", "Station 4: scanned, registering…"]);
        let svg = m.svg();
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(svg.contains(">2?</text>") && svg.contains(">1</text>"), "numbers, amber with ? when not trusted");
        assert!(svg.contains("north&lt;wall&gt;") && !svg.contains("north<wall>"), "text is escaped");
        assert!(svg.contains("grid 1 m"));
        assert!(svg.contains("1 wall · 1 run of vegetation"), "the title counts each kind");
        assert!(svg.contains(r#"stroke-dasharray="4 4""#), "vegetation is dashed");
        let png = m.png();
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn labels_stay_inside_on_a_large_site() {
        // 120 m across: at about 7 px/m a 1.5 m margin would be 10 px, and the
        // top station's number (28 px above its dot) would be cut off.
        let mut r = reg();
        for (s, xy) in r.stations.iter_mut().zip([[0.0, 0.0], [120.0, 120.0]]) {
            if let Some(p) = s.pose.as_mut() {
                p.xyz_m = [xy[0], xy[1], 0.0];
            }
        }
        let m = Minimap::new(Some(&r), vec![], &[], false);
        let l = m.layout();
        for (n, p, _) in &m.stations {
            let (x, y) = l.px([p.xyz_m[0], p.xyz_m[1]]);
            assert!(y - 30.0 >= l.top, "station {n}'s number is below the title band");
            assert!(x + 40.0 <= l.w, "station {n}'s number fits on the right");
            assert!(x - 16.0 >= 0.0 && y + 16.0 <= l.top + l.map_h, "station {n}'s dot and heading fit");
        }
    }

    #[test]
    fn an_empty_project_still_draws() {
        let m = Minimap::new(None, vec![], &[1], false);
        assert_eq!(m.title, "No stations placed yet");
        assert!(m.svg().contains("Station 1: scanned, waiting to register"));
        assert_eq!(&m.png()[1..4], b"PNG");
    }
}
