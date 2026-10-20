//! Exports: decode a scan with `trimble-tzf` into levelled points, add
//! intensity, write PLY or LAS, and run the automatic checks.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Ply,
    Las,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Intensity {
    #[default]
    Auto,
    Scan,
    Preview,
    None,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportRequest {
    pub format: Format,
    #[serde(default)]
    pub intensity: Intensity,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Format::Ply => "ply",
            Format::Las => "las",
        }
    }
}

fn check(name: &str, ok: bool, value: Option<f64>, unit: Option<&str>, detail: String) -> Value {
    let mut v = json!({"name": name, "ok": ok, "detail": detail});
    if let Some(x) = value {
        v["value"] = json!(x);
    }
    if let Some(u) = unit {
        v["unit"] = json!(u);
    }
    v
}

/// Run one export (blocking: call from `spawn_blocking`). Returns the
/// `Export` fields other than ids.
pub fn run(tzf: &Path, preview: Option<&[u8]>, req: &ExportRequest, out: &Path) -> Result<Value, String> {
    let t = trimble_tzf::Tzf::open(tzf).map_err(|e| format!("open: {e}"))?;
    let meta = t.scan_meta().map_err(|e| format!("metadata: {e}"))?;
    let pts = t.grid_points().map_err(|e| format!("decode: {e}"))?;

    // Intensity. The scan's own intensity image (component 4) is not decoded
    // yet, so `auto` and `scan` use the grid-aligned preview when there is one.
    let (src, gray) = match (req.intensity, preview) {
        (Intensity::None, _) => ("none", None),
        (_, Some(jpg)) => {
            let (w, h, px) = crate::aim::jpeg_luma(jpg)?;
            ("preview", Some((w, h, px)))
        }
        (Intensity::Preview, None) => return Err("no preview available for intensity".into()),
        (_, None) => ("none", None),
    };
    let (rows, cols) = (meta.grid_rows as usize, meta.grid_cols as usize);
    let g = |p: &trimble_tzf::GridPoint| -> u8 {
        match &gray {
            Some((w, h, px)) => {
                let x = (p.col as usize * w / cols.max(1)).min(w - 1);
                let y = (p.row as usize * h / rows.max(1)).min(h - 1);
                px[y * w + x]
            }
            None => 128,
        }
    };

    match req.format {
        Format::Ply => write_ply(out, &pts, &g),
        Format::Las => write_las(out, &pts, &g),
    }
    .map_err(|e| format!("write: {e}"))?;

    // Checks.
    let mut lo = [f64::MAX; 3];
    let mut hi = [f64::MIN; 3];
    let mut max_r = 0f64;
    let mut finite = true;
    for p in &pts {
        let q = [p.point.x as f64, p.point.y as f64, p.point.z as f64];
        finite &= q.iter().all(|v| v.is_finite());
        for k in 0..3 {
            lo[k] = lo[k].min(q[k]);
            hi[k] = hi[k].max(q[k]);
        }
        max_r = max_r.max((q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt());
    }
    let tr = meta.tilt[0][0] + meta.tilt[1][1] + meta.tilt[2][2];
    let tilt_deg = ((tr - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees();
    let declared = meta.n_points as usize;
    let checks = vec![
        check("point_count", pts.len() == declared, Some(pts.len() as f64), Some("points"),
              format!("{} decoded, {declared} declared by the file", pts.len())),
        check("levelled", tilt_deg < 5.0, Some((tilt_deg * 1000.0).round() / 1000.0), Some("deg"),
              format!("tilt correction of {tilt_deg:.3}° applied (the compensator's range is a few degrees)")),
        check("coordinates", finite && max_r < 400.0, Some((max_r * 1000.0).round() / 1000.0), Some("m"),
              "all coordinates finite and within the scanner's range".into()),
        check("intensity", true, None, None,
              match src {
                  "preview" => "from the scanner preview (grid-aligned)".to_string(),
                  _ => "none; points are mid-grey".to_string(),
              }),
    ];
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(json!({
        "format": req.format.ext(),
        "point_count": pts.len(),
        "bytes": bytes,
        "bounds_m": {"min": lo, "max": hi},
        "checks": checks,
    }))
}

fn write_ply(path: &Path, pts: &[trimble_tzf::GridPoint], gray: &dyn Fn(&trimble_tzf::GridPoint) -> u8) -> std::io::Result<()> {
    let mut w = BufWriter::new(std::fs::File::create(path)?);
    write!(
        w,
        "ply\nformat binary_little_endian 1.0\ncomment x9-bridge export\nelement vertex {}\n\
         property float x\nproperty float y\nproperty float z\n\
         property uchar red\nproperty uchar green\nproperty uchar blue\nend_header\n",
        pts.len()
    )?;
    for p in pts {
        let v = gray(p);
        for f in [p.point.x, p.point.y, p.point.z] {
            w.write_all(&f.to_le_bytes())?;
        }
        w.write_all(&[v, v, v])?;
    }
    w.flush()
}

/// LAS 1.2, point data format 0 (20-byte records), 0.1 mm scale.
fn write_las(path: &Path, pts: &[trimble_tzf::GridPoint], gray: &dyn Fn(&trimble_tzf::GridPoint) -> u8) -> std::io::Result<()> {
    const SCALE: f64 = 0.0001;
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for p in pts {
        for (k, v) in [p.point.x, p.point.y, p.point.z].into_iter().enumerate() {
            min[k] = min[k].min(v as f64);
            max[k] = max[k].max(v as f64);
        }
    }
    if pts.is_empty() {
        min = [0.0; 3];
        max = [0.0; 3];
    }
    let pad = |s: &[u8]| {
        let mut b = [0u8; 32];
        b[..s.len()].copy_from_slice(s);
        b
    };
    let mut h = Vec::with_capacity(227);
    h.extend_from_slice(b"LASF");
    h.extend_from_slice(&[0u8; 4]);
    h.extend_from_slice(&[0u8; 16]);
    h.extend_from_slice(&[1, 2]);
    h.extend_from_slice(&pad(b"x9-bridge"));
    h.extend_from_slice(&pad(b"x9-bridge export"));
    h.extend_from_slice(&[0u8; 4]);
    h.extend_from_slice(&227u16.to_le_bytes());
    h.extend_from_slice(&227u32.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes());
    h.push(0);
    h.extend_from_slice(&20u16.to_le_bytes());
    h.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    h.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    h.extend_from_slice(&[0u8; 16]);
    for _ in 0..3 {
        h.extend_from_slice(&SCALE.to_le_bytes());
    }
    for m in min {
        h.extend_from_slice(&m.to_le_bytes());
    }
    for k in 0..3 {
        h.extend_from_slice(&max[k].to_le_bytes());
        h.extend_from_slice(&min[k].to_le_bytes());
    }
    let mut w = BufWriter::new(std::fs::File::create(path)?);
    w.write_all(&h)?;
    for p in pts {
        for (k, v) in [p.point.x, p.point.y, p.point.z].into_iter().enumerate() {
            w.write_all(&((((v as f64) - min[k]) / SCALE).round() as i32).to_le_bytes())?;
        }
        w.write_all(&(gray(p) as u16 * 257).to_le_bytes())?;
        w.write_all(&[0b0000_1001, 0, 0, 0])?;
        w.write_all(&0u16.to_le_bytes())?;
    }
    w.flush()
}
