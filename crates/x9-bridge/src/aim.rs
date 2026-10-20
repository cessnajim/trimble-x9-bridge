//! Aiming: a box drawn on a scan's preview → an area-scan window, using the
//! scan's own geometry (grid size, start angles, angular steps) from its file.

use crate::model::Window;
use std::f64::consts::{PI, TAU};
use std::path::Path;
use zune_jpeg::zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
use zune_jpeg::JpegDecoder;

/// The geometry of a scan's grid, in the instrument's own angles.
#[derive(Debug, Clone, Copy)]
pub struct Geo {
    pub cols: u32,
    pub rows: u32,
    /// Horizontal angle of the first column (instrument convention, [0, 2π)).
    pub start_ha: f64,
    /// Vertical angle of the first row, from the zenith.
    pub start_va: f64,
    pub step_ha: f64,
    pub step_va: f64,
}

pub fn geo(tzf: &Path) -> Result<Geo, String> {
    let t = trimble_tzf::Tzf::open(tzf).map_err(|e| e.to_string())?;
    let m = t.scan_meta().map_err(|e| e.to_string())?;
    Ok(Geo {
        cols: m.grid_cols,
        rows: m.grid_rows,
        start_ha: m.start_ha_rad,
        start_va: m.start_va_rad,
        step_ha: m.angle_step_rad[0],
        step_va: m.angle_step_rad[1],
    })
}

/// Width and height of a JPEG, from its headers.
pub fn jpeg_size(bytes: &[u8]) -> Result<(usize, usize), String> {
    let mut d = JpegDecoder::new(ZCursor::new(bytes));
    d.decode_headers().map_err(|e| format!("{e:?}"))?;
    let i = d.info().ok_or("no JPEG info")?;
    Ok((i.width as usize, i.height as usize))
}

/// Decode a JPEG to 8-bit grey: (width, height, pixels row-major).
pub fn jpeg_luma(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::Luma);
    let mut d = JpegDecoder::new_with_options(ZCursor::new(bytes), opts);
    let px = d.decode().map_err(|e| format!("{e:?}"))?;
    let i = d.info().ok_or("no JPEG info")?;
    let (w, h) = (i.width as usize, i.height as usize);
    if px.len() != w * h {
        return Err(format!("expected {} grey pixels, got {}", w * h, px.len()));
    }
    Ok((w, h, px))
}

/// Preview box (pixels, y down; `x1 < x0` wraps across the panorama seam)
/// → window, padded by `margin` radians on every side.
pub fn window_from_box(g: &Geo, pw: usize, ph: usize, b: [i64; 4], margin: f64) -> Result<Window, String> {
    let [x0, y0, x1, y1] = b;
    let (pw_i, ph_i) = (pw as i64, ph as i64);
    if !(0..=pw_i).contains(&x0) || !(0..=pw_i).contains(&x1) || !(0 <= y0 && y0 < y1 && y1 <= ph_i) {
        return Err(format!("box must lie within the {pw}x{ph} preview with y0 < y1"));
    }
    if x0 == x1 {
        return Err("box has zero width".into());
    }
    let (sx, sy) = (g.cols as f64 / pw as f64, g.rows as f64 / ph as f64);
    let margin = margin.max(0.0);
    let width_px = if x1 > x0 { x1 - x0 } else { x1 + pw_i - x0 };
    let start_ha = (g.start_ha + x0 as f64 * sx * g.step_ha - margin).rem_euclid(TAU);
    let delta_ha = (width_px as f64 * sx * g.step_ha + 2.0 * margin).min(TAU);
    let start_va = (g.start_va + y0 as f64 * sy * g.step_va - margin).max(0.0);
    let end_va = (g.start_va + y1 as f64 * sy * g.step_va + margin).min(PI);
    Ok(Window { start_ha, delta_ha, start_va, delta_va: end_va - start_va })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2026-10-19 full-dome scan (4165 × 1631, preview 2082 × 815) and
    /// the box that framed the window: the area scan aimed with these angles
    /// hit the window on the first try.
    #[test]
    fn reproduces_the_window_shot() {
        let g = Geo { cols: 4165, rows: 1631, start_ha: 3.578756332397461, start_va: 0.0,
                      step_ha: 0.0015086123021319509, step_va: 0.0015086123021319509 };
        let w = window_from_box(&g, 2082, 815, [523, 418, 674, 564], 0.0).unwrap();
        for (got, want) in [(w.start_ha, 5.157144), (w.delta_ha, 0.45571), (w.start_va, 1.261974), (w.delta_va, 0.440785)] {
            assert!((got - want).abs() < 2e-5, "{got} vs {want}");
        }
    }

    #[test]
    fn wraps_across_the_seam_and_rejects_bad_boxes() {
        let g = Geo { cols: 4000, rows: 1000, start_ha: 6.0, start_va: 0.0, step_ha: TAU / 4000.0, step_va: PI / 1000.0 };
        let w = window_from_box(&g, 4000, 1000, [3900, 400, 100, 500], 0.0).unwrap();
        assert!((w.delta_ha - 200.0 * TAU / 4000.0).abs() < 1e-9);
        assert!(w.start_ha < TAU);
        assert!(window_from_box(&g, 4000, 1000, [10, 500, 10, 600], 0.0).is_err());
        assert!(window_from_box(&g, 4000, 1000, [10, 600, 20, 500], 0.0).is_err());
        assert!(window_from_box(&g, 4000, 1000, [10, 0, 20, 1200], 0.0).is_err());
    }
}
