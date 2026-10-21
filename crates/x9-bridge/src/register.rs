//! Registration: a project's stations in one frame. The X9 levels every scan,
//! so a station's pose is a turn about vertical and a shift (x, y, z): four
//! numbers, not six. Stations are matched in pairs, then adjusted together:
//!
//! 1. Coarse: wall-like points (in a height band around the scanner, locally
//!    flat, with a horizontal normal; ground and trees drop out) as a plan
//!    image; every turn in 1° steps, the shift by FFT cross-correlation; the
//!    best few turns at least 10° apart, refined to 0.05°.
//! 2. Fine: point-to-plane ICP on 10 cm-thinned points from each guess,
//!    turn and shift together with a shrinking match distance; the guess that
//!    fits best wins. A pair counts when its residual is under 15 mm over at
//!    least 8000 matched flat points.
//! 3. Global: every pair that counts is a constraint, and all stations are
//!    adjusted together (the first fixed) with a robust (Cauchy) loss, so one
//!    wrong pair can't drag the rest and loops close.
//!
//! The project frame is the first station's: x and y as that scanner happened
//! to face (not north), z up, the origin at its centre. From the registered
//! stations come the walls (straight runs in the merged plan) and, for any
//! wall face, how much of its length some station saw and how finely.

use rustfft::{num_complex::Complex, FftPlanner};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use trimble_tzf::GridPoint;

/// Station clouds: one point per 5 cm cube (the centroid) within 30 m.
pub const VOXEL: f32 = 0.05;
pub const RANGE: f32 = 30.0;
/// Matching works on 10 cm-thinned points.
const ICP_VOXEL: f32 = 0.10;
/// Height band of the coarse plan slice, relative to the scanner.
const SLICE: (f32, f32) = (-1.0, 1.5);
const COARSE_CELL: f32 = 0.20;
const HYPOTHESES: usize = 5;
/// Points of the second station ICP iterates on (all are used for the final fit).
const ICP_SAMPLE: usize = 40_000;
pub const GOOD_RMS_M: f64 = 0.015;
pub const GOOD_MATCHES: usize = 8000;
/// A pair the global fit leaves further off than this is an outlier.
const OUTLIER_M: f64 = 0.10;
const OUTLIER_DEG: f64 = 0.3;

// ------------------------------------------------------------------ clouds ---

/// A station's points (scanner frame, levelled), one per 5 cm cube, with the
/// nearest range seen in each (for point spacing) and the scan's angular step.
pub struct Cloud {
    pub step_rad: f32,
    pub pts: Vec<[f32; 3]>,
    pub range: Vec<f32>,
}

const CLOUD_MAGIC: &[u8; 4] = b"X9C1";

fn key(p: [f32; 3], v: f32) -> u64 {
    let k = |x: f32| ((x / v).floor() as i64 + (1 << 20)) as u64 & 0x1F_FFFF;
    (k(p[0]) << 42) | (k(p[1]) << 21) | k(p[2])
}

impl Cloud {
    pub fn from_grid(pts: &[GridPoint], step_rad: f64) -> Cloud {
        let mut acc: HashMap<u64, ([f64; 3], u32, f32)> = HashMap::with_capacity(pts.len() / 8);
        for g in pts {
            let p = [g.point.x, g.point.y, g.point.z];
            if !p.iter().all(|v| v.is_finite()) {
                continue;
            }
            let r = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
            if r < 0.5 || p[0].hypot(p[1]) > RANGE {
                continue;
            }
            let e = acc.entry(key(p, VOXEL)).or_insert(([0.0; 3], 0, f32::MAX));
            for (sum, v) in e.0.iter_mut().zip(p) {
                *sum += v as f64;
            }
            e.1 += 1;
            e.2 = e.2.min(r);
        }
        let mut v: Vec<(u64, [f32; 3], f32)> =
            acc.into_iter().map(|(k, (s, n, r))| (k, [(s[0] / n as f64) as f32, (s[1] / n as f64) as f32, (s[2] / n as f64) as f32], r)).collect();
        v.sort_unstable_by_key(|x| x.0);
        Cloud { step_rad: step_rad as f32, pts: v.iter().map(|x| x.1).collect(), range: v.iter().map(|x| x.2).collect() }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(12 + self.pts.len() * 16);
        b.extend_from_slice(CLOUD_MAGIC);
        b.extend_from_slice(&(self.pts.len() as u32).to_le_bytes());
        b.extend_from_slice(&self.step_rad.to_le_bytes());
        for p in &self.pts {
            for v in p {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        for r in &self.range {
            b.extend_from_slice(&r.to_le_bytes());
        }
        b
    }

    pub fn from_bytes(b: &[u8]) -> Result<Cloud, String> {
        let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        if b.len() < 12 || &b[..4] != CLOUD_MAGIC {
            return Err("not a station cloud".into());
        }
        let n = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        if b.len() != 12 + n * 16 {
            return Err("station cloud is truncated".into());
        }
        let pts = (0..n).map(|i| [f(12 + i * 12), f(16 + i * 12), f(20 + i * 12)]).collect();
        let range = (0..n).map(|i| f(12 + n * 12 + i * 4)).collect();
        Ok(Cloud { step_rad: f(8), pts, range })
    }
}

// --------------------------------------------------------------- kd-tree ---

/// A static 3-D kd-tree (implicit: each range's middle element splits it).
pub struct Tree {
    pts: Vec<[f32; 3]>,
    idx: Vec<u32>,
}

fn d2(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

impl Tree {
    pub fn new(pts: &[[f32; 3]]) -> Tree {
        let mut idx: Vec<u32> = (0..pts.len() as u32).collect();
        fn build(pts: &[[f32; 3]], idx: &mut [u32], depth: usize) {
            if idx.len() <= 1 {
                return;
            }
            let mid = idx.len() / 2;
            let d = depth % 3;
            idx.select_nth_unstable_by(mid, |a, b| pts[*a as usize][d].total_cmp(&pts[*b as usize][d]));
            let (lo, hi) = idx.split_at_mut(mid);
            build(pts, lo, depth + 1);
            build(pts, &mut hi[1..], depth + 1);
        }
        build(pts, &mut idx, 0);
        let sorted = idx.iter().map(|&i| pts[i as usize]).collect();
        Tree { pts: sorted, idx }
    }

    /// The nearest point within `r` (index into the original slice, distance²).
    pub fn nearest(&self, q: [f32; 3], r: f32) -> Option<(usize, f32)> {
        let mut best = (u32::MAX, r * r);
        self.nn(0, self.pts.len(), 0, q, &mut best);
        (best.0 != u32::MAX).then_some((best.0 as usize, best.1))
    }

    fn nn(&self, lo: usize, hi: usize, depth: usize, q: [f32; 3], best: &mut (u32, f32)) {
        if lo >= hi {
            return;
        }
        let mid = (lo + hi) / 2;
        let p = self.pts[mid];
        let dd = d2(p, q);
        if dd < best.1 {
            *best = (self.idx[mid], dd);
        }
        let diff = q[depth % 3] - p[depth % 3];
        let (first, second) = if diff < 0.0 { ((lo, mid), (mid + 1, hi)) } else { ((mid + 1, hi), (lo, mid)) };
        self.nn(first.0, first.1, depth + 1, q, best);
        if diff * diff < best.1 {
            self.nn(second.0, second.1, depth + 1, q, best);
        }
    }

    /// The `K` nearest points (fewer if the tree is smaller): (distance², index).
    pub fn knn<const K: usize>(&self, q: [f32; 3]) -> ([(f32, u32); K], usize) {
        let mut out = [(f32::MAX, 0u32); K];
        let mut n = 0;
        self.kn(0, self.pts.len(), 0, q, &mut out, &mut n);
        (out, n)
    }

    fn kn<const K: usize>(&self, lo: usize, hi: usize, depth: usize, q: [f32; 3], out: &mut [(f32, u32); K], n: &mut usize) {
        if lo >= hi {
            return;
        }
        let mid = (lo + hi) / 2;
        let p = self.pts[mid];
        let dd = d2(p, q);
        if *n < K || dd < out[K - 1].0 {
            let mut i = (*n).min(K - 1);
            while i > 0 && out[i - 1].0 > dd {
                out[i] = out[i - 1];
                i -= 1;
            }
            out[i] = (dd, self.idx[mid]);
            *n = (*n + 1).min(K);
        }
        let diff = q[depth % 3] - p[depth % 3];
        let (first, second) = if diff < 0.0 { ((lo, mid), (mid + 1, hi)) } else { ((mid + 1, hi), (lo, mid)) };
        self.kn(first.0, first.1, depth + 1, q, out, n);
        if *n < K || diff * diff < out[K - 1].0 {
            self.kn(second.0, second.1, depth + 1, q, out, n);
        }
    }
}

/// Eigen-decomposition of a symmetric 3×3 matrix (Jacobi): the eigenvector
/// of the smallest eigenvalue, and that eigenvalue over their sum.
fn smallest_eigen(mut a: [[f64; 3]; 3]) -> ([f64; 3], f64) {
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..12 {
        let off = a[0][1].abs() + a[0][2].abs() + a[1][2].abs();
        if off < 1e-14 {
            break;
        }
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            if a[p][q].abs() < 1e-18 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = theta.signum().max(0.0) * 2.0 - 1.0;
            let t = t / (theta.abs() + (theta * theta + 1.0).sqrt());
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            for row in a.iter_mut() {
                let (akp, akq) = (row[p], row[q]);
                row[p] = c * akp - s * akq;
                row[q] = s * akp + c * akq;
            }
            let (rp, rq) = (a[p], a[q]);
            a[p] = std::array::from_fn(|k| c * rp[k] - s * rq[k]);
            a[q] = std::array::from_fn(|k| s * rp[k] + c * rq[k]);
            for row in v.iter_mut() {
                let (vp, vq) = (row[p], row[q]);
                row[p] = c * vp - s * vq;
                row[q] = s * vp + c * vq;
            }
        }
    }
    let ev = [a[0][0], a[1][1], a[2][2]];
    let i = (0..3).min_by(|&x, &y| ev[x].total_cmp(&ev[y])).unwrap();
    let sum = ev.iter().map(|e| e.max(0.0)).sum::<f64>();
    ([v[0][i], v[1][i], v[2][i]], ev[i].max(0.0) / (sum + 1e-12))
}

/// Run `f` over `0..n` on all cores, collecting results in order.
fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let threads = std::thread::available_parallelism().map(|t| t.get()).unwrap_or(1).min(n.max(1));
    let chunk = n.div_ceil(threads.max(1)).max(1);
    let f = &f;
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..n).step_by(chunk).map(|lo| s.spawn(move || (lo..(lo + chunk).min(n)).map(f).collect::<Vec<T>>())).collect();
        hs.into_iter().flat_map(|h| h.join().expect("worker thread")).collect()
    })
}

// --------------------------------------------------------------- matching ---

/// A station ready for matching: 10 cm-thinned points, their normals and
/// flatness, a kd-tree, and its wall slice (x, y).
pub struct Prepared {
    pub pts: Vec<[f32; 3]>,
    pub range: Vec<f32>,
    pub normal: Vec<[f32; 3]>,
    pub planar: Vec<bool>,
    pub wall: Vec<bool>,
    tree: Tree,
    slice: Vec<[f32; 2]>,
}

pub fn prepare(c: &Cloud) -> Prepared {
    let mut seen = HashSet::with_capacity(c.pts.len() / 4);
    let (mut pts, mut range) = (Vec::new(), Vec::new());
    for (p, r) in c.pts.iter().zip(&c.range) {
        if seen.insert(key(*p, ICP_VOXEL)) {
            pts.push(*p);
            range.push(*r);
        }
    }
    let tree = Tree::new(&pts);
    let nc: Vec<([f32; 3], f32)> = par_map(pts.len(), |i| {
        let (nb, n) = tree.knn::<12>(pts[i]);
        if n < 6 {
            return ([0.0, 0.0, 1.0], 1.0);
        }
        let mut m = [0f64; 3];
        for &(_, j) in &nb[..n] {
            for k in 0..3 {
                m[k] += pts[j as usize][k] as f64 / n as f64;
            }
        }
        let mut cov = [[0f64; 3]; 3];
        for &(_, j) in &nb[..n] {
            let d = [pts[j as usize][0] as f64 - m[0], pts[j as usize][1] as f64 - m[1], pts[j as usize][2] as f64 - m[2]];
            for a in 0..3 {
                for b in 0..3 {
                    cov[a][b] += d[a] * d[b];
                }
            }
        }
        let (v, curv) = smallest_eigen(cov);
        ([v[0] as f32, v[1] as f32, v[2] as f32], curv as f32)
    });
    let normal: Vec<[f32; 3]> = nc.iter().map(|x| x.0).collect();
    let planar = nc.iter().map(|x| x.1 < 0.05).collect();
    let wall: Vec<bool> = nc.iter().map(|x| x.0[2].abs() < 0.25 && x.1 < 0.03).collect();
    let slice = pts
        .iter()
        .zip(&wall)
        .filter(|(p, w)| **w && p[2] > SLICE.0 && p[2] < SLICE.1)
        .map(|(p, _)| [p[0], p[1]])
        .collect();
    Prepared { pts, range, normal, planar, wall, tree, slice }
}

/// How B sits in A: `B_in_A = R(turn) b + shift`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pair {
    pub a: String,
    pub b: String,
    pub turn_deg: f64,
    pub shift_m: [f64; 3],
    /// None when ICP found too few matches.
    pub rms_mm: Option<f64>,
    pub matches: usize,
    pub overlap: f64,
    pub coarse_score: f64,
    /// Fits well enough to be a constraint.
    pub good: bool,
    /// After the global fit: how far this pair is left from its own fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_deg: Option<f64>,
    #[serde(default)]
    pub outlier: bool,
}

struct Coarse {
    n: usize,
    m: usize,
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    ifft: std::sync::Arc<dyn rustfft::Fft<f32>>,
}

impl Coarse {
    fn new() -> Coarse {
        let n = (2.0 * RANGE / COARSE_CELL) as usize;
        let m = 2 * n;
        let mut planner = FftPlanner::new();
        Coarse { n, m, fft: planner.plan_fft_forward(m), ifft: planner.plan_fft_inverse(m) }
    }

    /// The slice turned by `theta` as a blurred plan image (`n × n`).
    fn raster(&self, xy: &[[f32; 2]], theta: f32) -> Vec<f32> {
        let n = self.n;
        let (c, s) = (theta.cos(), theta.sin());
        let mut img = vec![0f32; n * n];
        for p in xy {
            let (x, y) = (c * p[0] - s * p[1], s * p[0] + c * p[1]);
            let (i, j) = (((x + RANGE) / COARSE_CELL).floor(), ((y + RANGE) / COARSE_CELL).floor());
            if i >= 0.0 && j >= 0.0 && (i as usize) < n && (j as usize) < n {
                let v = &mut img[j as usize * n + i as usize];
                *v = (*v + 1.0).min(3.0);
            }
        }
        // Gaussian blur, σ = 1 cell, separable.
        let k = [0.0044, 0.054, 0.242, 0.3991, 0.242, 0.054, 0.0044];
        let mut tmp = vec![0f32; n * n];
        for r in 0..n {
            for c in 0..n {
                let mut acc = 0.0;
                for (t, w) in k.iter().enumerate() {
                    let cc = c as i64 + t as i64 - 3;
                    if cc >= 0 && (cc as usize) < n {
                        acc += w * img[r * n + cc as usize];
                    }
                }
                tmp[r * n + c] = acc;
            }
        }
        for r in 0..n {
            for c in 0..n {
                let mut acc = 0.0;
                for (t, w) in k.iter().enumerate() {
                    let rr = r as i64 + t as i64 - 3;
                    if rr >= 0 && (rr as usize) < n {
                        acc += w * tmp[rr as usize * n + c];
                    }
                }
                img[r * n + c] = acc;
            }
        }
        img
    }

    /// 2-D FFT of an `n × n` image zero-padded to `m × m`, transposed.
    fn spectrum(&self, img: &[f32]) -> Vec<Complex<f32>> {
        let (n, m) = (self.n, self.m);
        let mut rows = vec![Complex::new(0.0, 0.0); n * m];
        for r in 0..n {
            for c in 0..n {
                rows[r * m + c].re = img[r * n + c];
            }
        }
        self.fft.process(&mut rows);
        let mut t = vec![Complex::new(0.0, 0.0); m * m];
        for r in 0..n {
            for c in 0..m {
                t[c * m + r] = rows[r * m + c];
            }
        }
        self.fft.process(&mut t);
        t
    }

    /// Correlation peak of A's spectrum with B's image: (score, shift x, y in m).
    fn score(&self, fa: &[Complex<f32>], na: f32, b: &[f32]) -> (f32, [f32; 2]) {
        let m = self.m;
        let nb = b.iter().map(|v| v * v).sum::<f32>().sqrt();
        let fb = self.spectrum(b);
        let mut p: Vec<Complex<f32>> = fa.iter().zip(&fb).map(|(x, y)| x * y.conj()).collect();
        self.ifft.process(&mut p);
        let mut t = vec![Complex::new(0.0, 0.0); m * m];
        for r in 0..m {
            for c in 0..m {
                t[c * m + r] = p[r * m + c];
            }
        }
        self.ifft.process(&mut t);
        let (k, v) = t.iter().enumerate().max_by(|a, b| a.1.re.total_cmp(&b.1.re)).map(|(k, v)| (k, v.re)).unwrap_or((0, 0.0));
        let (iy, ix) = (k / m, k % m);
        let wrap = |i: usize| if i >= self.n { i as f32 - m as f32 } else { i as f32 };
        (v / ((m * m) as f32) / (na * nb + 1e-9), [wrap(ix) * COARSE_CELL, wrap(iy) * COARSE_CELL])
    }

    /// The best few (turn, shift, score) taking B's slice onto A's.
    fn hypotheses(&self, a: &[[f32; 2]], b: &[[f32; 2]]) -> Vec<(f32, [f32; 2], f32)> {
        let ia = self.raster(a, 0.0);
        let na = ia.iter().map(|v| v * v).sum::<f32>().sqrt();
        let fa = self.spectrum(&ia);
        let at = |theta: f32| self.score(&fa, na, &self.raster(b, theta));
        let sweep: Vec<(f32, [f32; 2])> = par_map(360, |d| at((d as f32).to_radians()));
        let mut order: Vec<usize> = (0..360).collect();
        order.sort_by(|x, y| sweep[*y].0.total_cmp(&sweep[*x].0));
        let mut picked: Vec<usize> = Vec::new();
        for d in order {
            if picked.iter().all(|e| {
                let g = (d as i64 - *e as i64).abs();
                g.min(360 - g) >= 10
            }) {
                picked.push(d);
            }
            if picked.len() == HYPOTHESES {
                break;
            }
        }
        picked
            .into_iter()
            .map(|d| {
                let mut best = (sweep[d].0, sweep[d].1, (d as f32).to_radians());
                for step in [0.25f32, 0.05] {
                    let centre = best.2;
                    for k in -4..=4 {
                        let th = centre + (k as f32 * step).to_radians();
                        let (s, t) = at(th);
                        if s > best.0 {
                            best = (s, t, th);
                        }
                    }
                }
                (best.2, best.1, best.0)
            })
            .collect()
    }
}

/// Solve a small dense system `A x = b` (Gaussian elimination, partial pivoting).
fn solve_dense(mut a: Vec<f64>, mut b: Vec<f64>, n: usize) -> Option<Vec<f64>> {
    for c in 0..n {
        let p = (c..n).max_by(|&i, &j| a[i * n + c].abs().total_cmp(&a[j * n + c].abs()))?;
        if a[p * n + c].abs() < 1e-15 {
            return None;
        }
        if p != c {
            for k in 0..n {
                a.swap(p * n + k, c * n + k);
            }
            b.swap(p, c);
        }
        for r in c + 1..n {
            let f = a[r * n + c] / a[c * n + c];
            if f != 0.0 {
                for k in c..n {
                    a[r * n + k] -= f * a[c * n + k];
                }
                b[r] -= f * b[c];
            }
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let s: f64 = (r + 1..n).map(|k| a[r * n + k] * x[k]).sum();
        x[r] = (b[r] - s) / a[r * n + r];
    }
    Some(x)
}

/// Point-to-plane ICP, 4 DOF, from a guess: (turn, shift, rms m, overlap, matches).
/// An ICP result: turn (rad), shift (m), RMS (m), overlap, matches.
type Fit = (f64, [f64; 3], f64, f64, usize);

fn icp(a: &Prepared, b: &Prepared, theta: f64, t: [f64; 3]) -> Option<Fit> {
    let mut x = [theta, t[0], t[1], t[2]];
    let every = (b.pts.len() / ICP_SAMPLE).max(1);
    let sample: Vec<[f32; 3]> = b.pts.iter().step_by(every).copied().collect();
    let moved = |x: &[f64; 4], p: [f32; 3]| {
        let (c, s) = (x[0].cos(), x[0].sin());
        let (px, py) = (p[0] as f64, p[1] as f64);
        [c * px - s * py + x[1], s * px + c * py + x[2], p[2] as f64 + x[3]]
    };
    for dmax in [1.0, 0.5, 0.25, 0.12, 0.08, 0.06, 0.05, 0.05] {
        for _ in 0..4 {
            let mut ata = [0f64; 16];
            let mut atb = [0f64; 4];
            let mut used = 0;
            for &p in &sample {
                let q = moved(&x, p);
                let Some((j, _)) = a.tree.nearest([q[0] as f32, q[1] as f32, q[2] as f32], dmax as f32) else { continue };
                if !a.planar[j] {
                    continue;
                }
                let (aq, n) = (a.pts[j], a.normal[j]);
                let nn = [n[0] as f64, n[1] as f64, n[2] as f64];
                let r = (q[0] - aq[0] as f64) * nn[0] + (q[1] - aq[1] as f64) * nn[1] + (q[2] - aq[2] as f64) * nn[2];
                // d(R p)/d turn about the moved point: (-(y - ty), x - tx, 0).
                let (rx, ry) = (q[0] - x[1], q[1] - x[2]);
                let jrow = [nn[0] * -ry + nn[1] * rx, nn[0], nn[1], nn[2]];
                let w = 1.0 / (r.abs() / (dmax / 3.0)).max(1.0);
                for i in 0..4 {
                    for k in 0..4 {
                        ata[i * 4 + k] += w * jrow[i] * jrow[k];
                    }
                    atb[i] -= w * jrow[i] * r;
                }
                used += 1;
            }
            if used < 200 {
                return None;
            }
            let dx = solve_dense(ata.to_vec(), atb.to_vec(), 4)?;
            for i in 0..4 {
                x[i] += dx[i];
            }
            if dx.iter().all(|d| d.abs() < 1e-5) {
                break;
            }
        }
    }
    // Final fit over all of B's points.
    let (mut ss, mut matches) = (0f64, 0usize);
    for &p in &b.pts {
        let q = moved(&x, p);
        let Some((j, _)) = a.tree.nearest([q[0] as f32, q[1] as f32, q[2] as f32], 0.05) else { continue };
        if !a.planar[j] {
            continue;
        }
        let (aq, n) = (a.pts[j], a.normal[j]);
        let r = (q[0] - aq[0] as f64) * n[0] as f64 + (q[1] - aq[1] as f64) * n[1] as f64 + (q[2] - aq[2] as f64) * n[2] as f64;
        ss += r * r;
        matches += 1;
    }
    (matches > 0).then(|| (x[0], [x[1], x[2], x[3]], (ss / matches as f64).sqrt(), matches as f64 / b.pts.len().max(1) as f64, matches))
}

/// Match B against A (ids for the record): a coarse search, then ICP.
pub fn pair(a_id: &str, a: &Prepared, b_id: &str, b: &Prepared) -> Pair {
    let coarse = Coarse::new();
    let mut tried: Vec<(Fit, f32)> = Vec::new();
    if a.slice.len() >= 50 && b.slice.len() >= 50 {
        for (theta, t, sc) in coarse.hypotheses(&a.slice, &b.slice) {
            if let Some(r) = icp(a, b, theta as f64, [t[0] as f64, t[1] as f64, 0.0]) {
                tried.push((r, sc));
            }
        }
    }
    best_pair(a_id, b_id, tried)
}

/// Match B against A from a guess of B in A (turn rad, x, y, z): ICP only.
pub fn pair_from(a_id: &str, a: &Prepared, b_id: &str, b: &Prepared, guess: [f64; 4]) -> Pair {
    let tried = icp(a, b, guess[0], [guess[1], guess[2], guess[3]]).map(|r| (r, f32::NAN)).into_iter().collect();
    best_pair(a_id, b_id, tried)
}

fn best_pair(a_id: &str, b_id: &str, tried: Vec<(Fit, f32)>) -> Pair {
    let good = |r: &Fit| r.2 <= GOOD_RMS_M;
    let best = tried.into_iter().max_by(|x, y| {
        let kx = (good(&x.0), if good(&x.0) { x.0 .4 as f64 } else { -x.0 .2 });
        let ky = (good(&y.0), if good(&y.0) { y.0 .4 as f64 } else { -y.0 .2 });
        kx.partial_cmp(&ky).unwrap_or(std::cmp::Ordering::Equal)
    });
    let r3 = |v: f64| (v * 1000.0).round() / 1000.0;
    match best {
        None => Pair { a: a_id.into(), b: b_id.into(), turn_deg: 0.0, shift_m: [0.0; 3], rms_mm: None, matches: 0, overlap: 0.0,
                       coarse_score: 0.0, good: false, left_mm: None, left_deg: None, outlier: false },
        Some(((turn, shift, rms, overlap, matches), sc)) => Pair {
            a: a_id.into(),
            b: b_id.into(),
            turn_deg: r3(turn.to_degrees().rem_euclid(360.0)),
            shift_m: shift.map(r3),
            rms_mm: Some((rms * 10_000.0).round() / 10.0),
            matches,
            overlap: r3(overlap),
            coarse_score: if sc.is_nan() { 0.0 } else { r3(sc as f64) },
            good: rms <= GOOD_RMS_M && matches >= GOOD_MATCHES,
            left_mm: None,
            left_deg: None,
            outlier: false,
        },
    }
}

// ----------------------------------------------------------------- global ---

/// A station's pose in the project frame: turn about vertical and position.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Pose {
    pub turn_deg: f64,
    pub xyz_m: [f64; 3],
}

impl Pose {
    pub fn apply(&self, p: [f32; 3]) -> [f32; 3] {
        let (s, c) = self.turn_deg.to_radians().sin_cos();
        let (x, y) = (p[0] as f64, p[1] as f64);
        [(c * x - s * y + self.xyz_m[0]) as f32, (s * x + c * y + self.xyz_m[1]) as f32, (p[2] as f64 + self.xyz_m[2]) as f32]
    }
}

type P4 = [f64; 4];

fn compose(pa: P4, rel: P4) -> P4 {
    let (s, c) = pa[0].sin_cos();
    [pa[0] + rel[0], pa[1] + c * rel[1] - s * rel[2], pa[2] + s * rel[1] + c * rel[2], pa[3] + rel[3]]
}

fn invert(rel: P4) -> P4 {
    let th = -rel[0];
    let (s, c) = th.sin_cos();
    [th, -(c * rel[1] - s * rel[2]), -(s * rel[1] + c * rel[2]), -rel[3]]
}

fn relative(pa: P4, pb: P4) -> P4 {
    let (s, c) = pa[0].sin_cos();
    let (dx, dy) = (pb[1] - pa[1], pb[2] - pa[2]);
    [pb[0] - pa[0], c * dx + s * dy, -s * dx + c * dy, pb[3] - pa[3]]
}

fn wrap(a: f64) -> f64 {
    (a + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
}

/// Poses for `ids` (the first is the frame) from the pairs that fit; `None`
/// for a station no good pair connects. Marks each pair's leftover and outliers.
pub fn solve(ids: &[String], pairs: &mut [Pair]) -> Vec<Option<Pose>> {
    let n = ids.len();
    let at = |id: &str| ids.iter().position(|x| x == id);
    let obs = |p: &Pair| -> P4 { [p.turn_deg.to_radians(), p.shift_m[0], p.shift_m[1], p.shift_m[2]] };
    let mut good: Vec<(usize, usize, P4, f64, usize)> = pairs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.good)
        .filter_map(|(k, p)| Some((at(&p.a)?, at(&p.b)?, obs(p), (p.matches as f64 / GOOD_MATCHES as f64).sqrt(), k)))
        .collect();
    good.sort_by(|x, y| pairs[y.4].matches.cmp(&pairs[x.4].matches));
    // A spanning tree from the first station, strongest pairs first.
    let mut pose: Vec<Option<P4>> = vec![None; n];
    if n == 0 {
        return vec![];
    }
    pose[0] = Some([0.0; 4]);
    loop {
        let mut grew = false;
        for &(a, b, rel, _, _) in &good {
            match (pose[a], pose[b]) {
                (Some(pa), None) => {
                    pose[b] = Some(compose(pa, rel));
                    grew = true;
                }
                (None, Some(pb)) => {
                    pose[a] = Some(compose(pb, invert(rel)));
                    grew = true;
                }
                _ => {}
            }
        }
        if !grew {
            break;
        }
    }
    let free: Vec<usize> = (1..n).filter(|&k| pose[k].is_some()).collect();
    let slot = |k: usize| free.iter().position(|&f| f == k);
    let mut x: Vec<f64> = free.iter().flat_map(|&k| pose[k].unwrap()).collect();
    let cons: Vec<&(usize, usize, P4, f64, usize)> = good.iter().filter(|g| pose[g.0].is_some() && pose[g.1].is_some()).collect();
    let get = |x: &[f64], k: usize| -> P4 {
        match slot(k) {
            None => [0.0; 4],
            Some(s) => [x[4 * s], x[4 * s + 1], x[4 * s + 2], x[4 * s + 3]],
        }
    };
    // Gauss–Newton with a Cauchy loss (IRLS), turns weighted so 0.1 rad ~ 1 m.
    const SCALE: f64 = 0.05;
    let m = 4 * free.len();
    for _ in 0..50 {
        if m == 0 {
            break;
        }
        let mut h = vec![0f64; m * m];
        let mut g = vec![0f64; m];
        for &&(a, b, o, w, _) in &cons {
            let (pa, pb) = (get(&x, a), get(&x, b));
            let rel = relative(pa, pb);
            let mut e = [wrap(rel[0] - o[0]), rel[1] - o[1], rel[2] - o[2], rel[3] - o[3]];
            let unit = [10.0 * w, w, w, w];
            for k in 0..4 {
                e[k] *= unit[k];
            }
            // Jacobians of rel by A's and B's (turn, x, y, z).
            let (s, c) = pa[0].sin_cos();
            let (dx, dy) = (pb[1] - pa[1], pb[2] - pa[2]);
            let ja: [[f64; 4]; 4] = [[-1.0, 0.0, 0.0, 0.0], [-s * dx + c * dy, -c, -s, 0.0], [-c * dx - s * dy, s, -c, 0.0], [0.0, 0.0, 0.0, -1.0]];
            let jb: [[f64; 4]; 4] = [[1.0, 0.0, 0.0, 0.0], [0.0, c, s, 0.0], [0.0, -s, c, 0.0], [0.0, 0.0, 0.0, 1.0]];
            for k in 0..4 {
                let rw = 1.0 / (1.0 + (e[k] / SCALE).powi(2));
                let mut row: Vec<(usize, f64)> = Vec::with_capacity(8);
                for (who, jac) in [(a, &ja), (b, &jb)] {
                    if let Some(sl) = slot(who) {
                        for (q, j) in jac[k].iter().enumerate() {
                            row.push((4 * sl + q, j * unit[k]));
                        }
                    }
                }
                for &(i, vi) in &row {
                    g[i] -= rw * vi * e[k];
                    for &(j, vj) in &row {
                        h[i * m + j] += rw * vi * vj;
                    }
                }
            }
        }
        for i in 0..m {
            h[i * m + i] += 1e-9;
        }
        let Some(dx) = solve_dense(h, g, m) else { break };
        for i in 0..m {
            x[i] += dx[i];
        }
        if dx.iter().all(|d| d.abs() < 1e-7) {
            break;
        }
    }
    let fin: Vec<Option<P4>> = (0..n).map(|k| if k == 0 { Some([0.0; 4]) } else { slot(k).map(|_| get(&x, k)) }).collect();
    for &(a, b, o, _, k) in &good {
        if let (Some(pa), Some(pb)) = (fin[a], fin[b]) {
            let rel = relative(pa, pb);
            let mm = ((rel[1] - o[1]).powi(2) + (rel[2] - o[2]).powi(2) + (rel[3] - o[3]).powi(2)).sqrt() * 1000.0;
            let deg = wrap(rel[0] - o[0]).to_degrees();
            pairs[k].left_mm = Some((mm * 10.0).round() / 10.0);
            pairs[k].left_deg = Some((deg * 1000.0).round() / 1000.0);
            pairs[k].outlier = mm > OUTLIER_M * 1000.0 || deg.abs() > OUTLIER_DEG;
        }
    }
    fin.into_iter()
        .map(|p| p.map(|p| Pose { turn_deg: (p[0].to_degrees().rem_euclid(360.0) * 1000.0).round() / 1000.0, xyz_m: [p[1], p[2], p[3]].map(|v| (v * 1000.0).round() / 1000.0) }))
        .collect()
}

impl Pose {
    fn p4(&self) -> P4 {
        [self.turn_deg.to_radians(), self.xyz_m[0], self.xyz_m[1], self.xyz_m[2]]
    }
}

/// Bring every station in `ids` (station order; the first is the frame) into
/// the frame, keeping `pairs` already matched. A station not yet placed is
/// searched against the placed stations nearest in number until one fits;
/// that gives a predicted pose, and the other placed stations within reach
/// are matched from the prediction (ICP only, no search). `say` hears progress.
pub fn register_all(ids: &[String], prep: &[&Prepared], mut pairs: Vec<Pair>, say: &dyn Fn(usize, usize)) -> (Vec<Pair>, Vec<Option<Pose>>) {
    pairs.retain(|p| ids.contains(&p.a) && ids.contains(&p.b));
    let mut poses = solve(ids, &mut pairs);
    let n = ids.len();
    for b in 1..n {
        if poses[b].is_some() {
            continue;
        }
        let mut cands: Vec<usize> = (0..n).filter(|&a| a != b && poses[a].is_some()).collect();
        cands.sort_by_key(|&a| (a as i64 - b as i64).abs());
        let had = |pairs: &[Pair], a: usize| pairs.iter().position(|p| (p.a == ids[a] && p.b == ids[b]) || (p.a == ids[b] && p.b == ids[a]));
        let mut predicted: Option<P4> = None;
        for &a in &cands {
            let pa = poses[a].expect("placed").p4();
            if let Some(k) = had(&pairs, a) {
                let p = &pairs[k];
                if p.good && predicted.is_none() {
                    let rel = [p.turn_deg.to_radians(), p.shift_m[0], p.shift_m[1], p.shift_m[2]];
                    predicted = Some(if p.a == ids[a] { compose(pa, rel) } else { compose(pa, invert(rel)) });
                }
                continue;
            }
            let p = match predicted {
                None => {
                    say(b, a);
                    pair(&ids[a], prep[a], &ids[b], prep[b])
                }
                Some(pb) => {
                    if (pb[1] - pa[1]).hypot(pb[2] - pa[2]) > 1.5 * RANGE as f64 {
                        continue;
                    }
                    pair_from(&ids[a], prep[a], &ids[b], prep[b], relative(pa, pb))
                }
            };
            if p.good && predicted.is_none() {
                predicted = Some(compose(pa, [p.turn_deg.to_radians(), p.shift_m[0], p.shift_m[1], p.shift_m[2]]));
            }
            pairs.push(p);
        }
        poses = solve(ids, &mut pairs);
    }
    (pairs, poses)
}

// -------------------------------------------------------------- the record ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationPose {
    pub scan_id: String,
    pub station: u16,
    /// None: not placed (no pair with a placed station fits).
    pub pose: Option<Pose>,
}

/// A project's registration as the bridge keeps it (`registrations.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub project_id: String,
    /// The frame: the first station's scan.
    pub frame_scan_id: Option<String>,
    pub stations: Vec<StationPose>,
    pub pairs: Vec<Pair>,
    pub walls: Vec<Wall>,
    pub summary: String,
    pub updated_at: String,
}

impl Registration {
    /// Placed stations' scan ids by station number.
    pub fn station_scans(&self) -> HashMap<u16, String> {
        self.stations.iter().filter(|s| s.pose.is_some()).map(|s| (s.station, s.scan_id.clone())).collect()
    }

    /// "12 of 12 stations in one frame; 26 pairs agree within 9 mm"
    pub fn summarise(&mut self) {
        let placed = self.stations.iter().filter(|s| s.pose.is_some()).count();
        let used: Vec<&Pair> = self.pairs.iter().filter(|p| p.good && !p.outlier && p.left_mm.is_some()).collect();
        let worst = used.iter().filter_map(|p| p.left_mm).fold(0f64, f64::max);
        let rms = used.iter().filter_map(|p| p.rms_mm).fold(0f64, f64::max);
        let s = |u: usize| if u == 1 { "" } else { "s" };
        self.summary = match (self.stations.len(), used.len()) {
            (0, _) => "No full domes in this project yet.".into(),
            (1, _) => "One station: it is the project frame.".into(),
            (n, 0) => format!("{placed} of {n} stations in one frame; no pairs fit yet."),
            // A chain: each pair is the only tie, so there is nothing to cross-check yet.
            (n, u) if u < placed => format!("{placed} of {n} stations in one frame; {u} pair{} fit within {rms:.0} mm (no loop to cross-check yet).", s(u)),
            (n, u) => format!("{placed} of {n} stations in one frame; {u} pair{} agree within {worst:.0} mm.", s(u)),
        };
    }

    pub fn to_json(&self) -> Value {
        let number = |id: &str| self.stations.iter().find(|s| s.scan_id == id).map(|s| s.station);
        let frame = self.stations.first().map(|s| s.station).unwrap_or(1);
        json!({
            "project_id": self.project_id,
            "frame": format!("station {frame}'s scanner: x and y as it happened to face (not north), z up, the origin at its centre; metres"),
            "summary": self.summary,
            "updated_at": self.updated_at,
            "stations": self.stations.iter().map(|s| json!({
                "station": s.station, "scan_id": s.scan_id, "placed": s.pose.is_some(),
                "turn_deg": s.pose.map(|p| p.turn_deg), "xyz_m": s.pose.map(|p| p.xyz_m),
            })).collect::<Vec<_>>(),
            "pairs": self.pairs.iter().filter(|p| p.good).map(|p| json!({
                "stations": [number(&p.a), number(&p.b)], "rms_mm": p.rms_mm, "matches": p.matches,
                "left_mm": p.left_mm, "outlier": p.outlier,
            })).collect::<Vec<_>>(),
            "pairs_tried": self.pairs.len(),
            "walls": self.walls,
            "plan_url": format!("/v1/projects/{}/registration/plan", self.project_id),
        })
    }
}

/// What a mission needs of the registration: walls by id, and judging faces.
pub struct Geo {
    pub walls: Vec<Wall>,
    pub merged: Option<std::sync::Arc<Merged>>,
    pub scans: HashMap<u16, String>,
}

impl Geo {
    pub fn none() -> Geo {
        Geo { walls: vec![], merged: None, scans: HashMap::new() }
    }
    pub fn wall(&self, id: &str) -> Option<Face> {
        self.walls.iter().find(|w| w.wall_id == id).map(|w| Face { from: w.from, to: w.to })
    }
    /// The face's coverage, when the project is registered.
    pub fn judge(&self, f: &Face) -> Option<Value> {
        self.merged.as_ref().map(|m| face_coverage(m, f, &self.scans))
    }
}

// ------------------------------------------------------------------ merged ---

/// Vertical surfaces of every registered station in the project frame (10 cm
/// points): position, point spacing there (mm) and which station saw it.
#[derive(Default)]
pub struct Merged {
    pub pts: Vec<[f32; 3]>,
    pub spacing_mm: Vec<f32>,
    pub station: Vec<u16>,
    /// Every 10 cm point in plan (x, y), for drawing the ground and clutter.
    pub plan: Vec<[f32; 2]>,
}

const MERGED_MAGIC: &[u8; 4] = b"X9M1";

impl Merged {
    pub fn add(&mut self, p: &Prepared, step_rad: f32, pose: &Pose, station: u16) {
        for i in 0..p.pts.len() {
            let w = pose.apply(p.pts[i]);
            self.plan.push([w[0], w[1]]);
            if p.wall[i] {
                self.pts.push(w);
                self.spacing_mm.push(p.range[i] * step_rad * 1000.0);
                self.station.push(station);
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(12 + self.pts.len() * 18 + self.plan.len() * 8);
        b.extend_from_slice(MERGED_MAGIC);
        b.extend_from_slice(&(self.pts.len() as u32).to_le_bytes());
        b.extend_from_slice(&(self.plan.len() as u32).to_le_bytes());
        for (i, p) in self.pts.iter().enumerate() {
            for v in p {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(&self.spacing_mm[i].to_le_bytes());
            b.extend_from_slice(&self.station[i].to_le_bytes());
        }
        for p in &self.plan {
            b.extend_from_slice(&p[0].to_le_bytes());
            b.extend_from_slice(&p[1].to_le_bytes());
        }
        b
    }

    pub fn from_bytes(b: &[u8]) -> Result<Merged, String> {
        let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        if b.len() < 12 || &b[..4] != MERGED_MAGIC {
            return Err("not a merged cloud".into());
        }
        let n = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let np = u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize;
        if b.len() != 12 + n * 18 + np * 8 {
            return Err("merged cloud is truncated".into());
        }
        let mut m = Merged::default();
        for i in 0..n {
            let o = 12 + i * 18;
            m.pts.push([f(o), f(o + 4), f(o + 8)]);
            m.spacing_mm.push(f(o + 12));
            m.station.push(u16::from_le_bytes([b[o + 16], b[o + 17]]));
        }
        let o = 12 + n * 18;
        m.plan = (0..np).map(|i| [f(o + i * 8), f(o + i * 8 + 4)]).collect();
        Ok(m)
    }
}

// ------------------------------------------------------------------- walls ---

/// A straight run of wall in plan (project frame, metres).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wall {
    pub wall_id: String,
    pub from: [f64; 2],
    pub to: [f64; 2],
    pub length_m: f64,
    /// Height seen (bottom and top, project frame).
    pub z_m: [f64; 2],
    /// Station numbers that saw it.
    pub seen_by: Vec<u16>,
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545F4914F6CDD1D) % n.max(1) as u64) as usize
    }
}

const WALL_TOL: f32 = 0.04;
const WALL_GAP: f32 = 0.6;
const WALL_MIN: f32 = 1.5;
const MAX_WALLS: usize = 60;

/// Straight walls in the merged vertical points, longest first.
pub fn walls(m: &Merged) -> Vec<Wall> {
    // One point per 5 cm plan cell (keeping a station that saw it and its height span).
    // Plan cell → (a point, lowest and highest z, stations seen by, as bits).
    type Cell = ([f32; 2], f32, f32, u64);
    let mut cells: HashMap<(i32, i32), Cell> = HashMap::new();
    for (i, p) in m.pts.iter().enumerate() {
        let k = ((p[0] / 0.05).floor() as i32, (p[1] / 0.05).floor() as i32);
        let e = cells.entry(k).or_insert(([p[0], p[1]], p[2], p[2], 0));
        e.1 = e.1.min(p[2]);
        e.2 = e.2.max(p[2]);
        e.3 |= 1u64 << (m.station[i].min(63));
    }
    // A plan cell is wall if its points span at least 0.5 m of height.
    let mut pts: Vec<Cell> = cells.into_values().filter(|c| c.2 - c.1 >= 0.5).collect();
    pts.sort_by(|a, b| a.0[0].total_cmp(&b.0[0]).then(a.0[1].total_cmp(&b.0[1])));
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::new();
    let mut alive: Vec<bool> = vec![true; pts.len()];
    let mut misses = 0;
    while out.len() < MAX_WALLS && misses < 8 {
        let idx: Vec<usize> = (0..pts.len()).filter(|&i| alive[i]).collect();
        if idx.len() < 40 {
            break;
        }
        let mut best: Option<([f32; 2], [f32; 2], usize)> = None;
        for _ in 0..600 {
            let i = idx[rng.below(idx.len())];
            let j = idx[rng.below(idx.len())];
            let (a, b) = (pts[i].0, pts[j].0);
            let d = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            if !(0.5..=8.0).contains(&d) {
                continue;
            }
            let dir = [(b[0] - a[0]) / d, (b[1] - a[1]) / d];
            let nrm = [-dir[1], dir[0]];
            let count = idx.iter().filter(|&&k| ((pts[k].0[0] - a[0]) * nrm[0] + (pts[k].0[1] - a[1]) * nrm[1]).abs() < WALL_TOL).count();
            if best.is_none_or(|b| count > b.2) {
                best = Some((a, nrm, count));
            }
        }
        let Some((a, nrm, count)) = best else { break };
        if count < 30 {
            break;
        }
        // Refit to the inliers (principal direction), then split at gaps.
        let inl: Vec<usize> = idx.iter().copied().filter(|&k| ((pts[k].0[0] - a[0]) * nrm[0] + (pts[k].0[1] - a[1]) * nrm[1]).abs() < WALL_TOL).collect();
        let n = inl.len() as f32;
        let c = inl.iter().fold([0f32; 2], |s, &k| [s[0] + pts[k].0[0] / n, s[1] + pts[k].0[1] / n]);
        let (mut sxx, mut sxy, mut syy) = (0f32, 0f32, 0f32);
        for &k in &inl {
            let (dx, dy) = (pts[k].0[0] - c[0], pts[k].0[1] - c[1]);
            sxx += dx * dx;
            sxy += dx * dy;
            syy += dy * dy;
        }
        let ang = 0.5 * (2.0 * sxy).atan2(sxx - syy);
        let dir = [ang.cos(), ang.sin()];
        let nrm = [-dir[1], dir[0]];
        let mut on: Vec<(f32, usize)> = idx
            .iter()
            .copied()
            .filter(|&k| ((pts[k].0[0] - c[0]) * nrm[0] + (pts[k].0[1] - c[1]) * nrm[1]).abs() < WALL_TOL)
            .map(|k| ((pts[k].0[0] - c[0]) * dir[0] + (pts[k].0[1] - c[1]) * dir[1], k))
            .collect();
        on.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut kept = false;
        let mut start = 0;
        for e in 1..=on.len() {
            if e < on.len() && on[e].0 - on[e - 1].0 <= WALL_GAP {
                continue;
            }
            let run = &on[start..e];
            let len = run.last().map(|l| l.0).unwrap_or(0.0) - run[0].0;
            // At least half the run's 5 cm cells hold wall.
            if len >= WALL_MIN && run.len() as f32 >= 0.5 * len / 0.05 {
                let (u0, u1) = (run[0].0, run.last().unwrap().0);
                let p = |u: f32| [((c[0] + dir[0] * u) * 100.0).round() as f64 / 100.0, ((c[1] + dir[1] * u) * 100.0).round() as f64 / 100.0];
                let zlo = run.iter().map(|r| pts[r.1].1).fold(f32::MAX, f32::min);
                let zhi = run.iter().map(|r| pts[r.1].2).fold(f32::MIN, f32::max);
                let mask = run.iter().fold(0u64, |s, r| s | pts[r.1].3);
                out.push(Wall {
                    wall_id: String::new(),
                    from: p(u0),
                    to: p(u1),
                    length_m: ((u1 - u0) as f64 * 100.0).round() / 100.0,
                    z_m: [(zlo as f64 * 100.0).round() / 100.0, (zhi as f64 * 100.0).round() / 100.0],
                    seen_by: (0..64u16).filter(|b| mask & (1u64 << b) != 0).collect(),
                });
                kept = true;
            }
            start = e;
        }
        for &(_, k) in &on {
            alive[k] = false;
        }
        misses = if kept { 0 } else { misses + 1 };
    }
    out.sort_by(|a, b| b.length_m.total_cmp(&a.length_m));
    for (i, w) in out.iter_mut().enumerate() {
        w.wall_id = format!("w{}", i + 1);
    }
    out
}

// ------------------------------------------------------------------- faces ---

/// A wall face in plan: the stretch from `from` to `to` (project frame, m).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Face {
    pub from: [f64; 2],
    pub to: [f64; 2],
}

/// Points within this of the face's line count as on it.
const FACE_TOL: f32 = 0.15;
/// The face is judged in columns this wide; a column is seen when its points
/// span at least `COLUMN_SEEN` of height (windows leave the wall around them).
const COLUMN: f32 = 0.5;
const COLUMN_SEEN: f32 = 1.0;
/// Share of the face's length that must be seen for it to count as covered.
pub const FACE_COVERED: f64 = 0.8;

/// How much of a face the registered stations saw: share of its length,
/// spacing there, which stations, and the unseen stretches.
pub fn face_coverage(m: &Merged, f: &Face, station_scan: &HashMap<u16, String>) -> Value {
    let (a, b) = ([f.from[0] as f32, f.from[1] as f32], [f.to[0] as f32, f.to[1] as f32]);
    let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
    if len < 0.2 {
        return json!({"error": "the face is shorter than 20 cm"});
    }
    let dir = [(b[0] - a[0]) / len, (b[1] - a[1]) / len];
    let nrm = [-dir[1], dir[0]];
    let ncol = (len / COLUMN).ceil().max(1.0) as usize;
    let mut col: Vec<(f32, f32)> = vec![(f32::MAX, f32::MIN); ncol];
    let mut per_station: HashMap<u16, HashSet<usize>> = HashMap::new();
    let mut spacing: Vec<f32> = Vec::new();
    let mut zs: Vec<f32> = Vec::new();
    for (i, p) in m.pts.iter().enumerate() {
        let (dx, dy) = (p[0] - a[0], p[1] - a[1]);
        let u = dx * dir[0] + dy * dir[1];
        if u < 0.0 || u > len || (dx * nrm[0] + dy * nrm[1]).abs() > FACE_TOL {
            continue;
        }
        let c = ((u / COLUMN) as usize).min(ncol - 1);
        col[c].0 = col[c].0.min(p[2]);
        col[c].1 = col[c].1.max(p[2]);
        per_station.entry(m.station[i]).or_default().insert(c);
        spacing.push(m.spacing_mm[i]);
        zs.push(p[2]);
    }
    let seen: Vec<bool> = col.iter().map(|(lo, hi)| hi - lo >= COLUMN_SEEN).collect();
    let share = seen.iter().filter(|s| **s).count() as f64 / ncol as f64;
    // Unseen stretches, as plan points.
    let at = |u: f32| [((a[0] + dir[0] * u) * 100.0).round() as f64 / 100.0, ((a[1] + dir[1] * u) * 100.0).round() as f64 / 100.0];
    let mut gaps = Vec::new();
    let mut i = 0;
    while i < ncol {
        if seen[i] {
            i += 1;
            continue;
        }
        let j = (i..ncol).find(|&k| seen[k]).unwrap_or(ncol);
        gaps.push(json!({"from": at(i as f32 * COLUMN), "to": at((j as f32 * COLUMN).min(len)), "length_m": (((j - i) as f32 * COLUMN).min(len) * 10.0).round() / 10.0}));
        i = j;
    }
    spacing.sort_by(|x, y| x.total_cmp(y));
    zs.sort_by(|x, y| x.total_cmp(y));
    let pct = |v: &[f32], q: f64| v.get(((v.len() as f64 - 1.0) * q).round() as usize).map(|x| (*x as f64 * 10.0).round() / 10.0);
    let mut stations: Vec<(u16, usize)> = per_station.iter().map(|(s, c)| (*s, c.iter().filter(|k| seen[**k]).count())).filter(|x| x.1 > 0).collect();
    stations.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
    json!({
        "length_m": (len as f64 * 100.0).round() / 100.0,
        "seen_share": (share * 1000.0).round() / 1000.0,
        "covered": share >= FACE_COVERED,
        "height_m": [pct(&zs, 0.02), pct(&zs, 0.98)],
        "spacing_mm": {"p50": pct(&spacing, 0.5), "p90": pct(&spacing, 0.9)},
        "stations": stations.iter().map(|(s, c)| json!({"station": s, "scan_id": station_scan.get(s), "seen_share": ((*c as f64 / ncol as f64) * 1000.0).round() / 1000.0})).collect::<Vec<_>>(),
        "unseen": gaps,
    })
}

// ------------------------------------------------------------------ picture ---

/// What to draw on the plan besides the points.
pub struct PlanMarks<'a> {
    pub stations: &'a [(u16, Pose)],
    pub walls: &'a [Wall],
    /// Target faces: id, face, colour.
    pub faces: Vec<(String, Face, [u8; 3])>,
}

/// The registered plan: every point grey, vertical surfaces coloured by
/// spacing, walls with their ids, stations numbered, a 5 m grid. PNG bytes.
pub fn plan_png(m: &Merged, marks: &PlanMarks) -> Vec<u8> {
    use crate::station::Canvas;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in m.plan.iter().chain(m.pts.iter().map(|p| [p[0], p[1]]).collect::<Vec<_>>().iter()) {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    if x0 > x1 {
        (x0, y0, x1, y1) = (-5.0, -5.0, 5.0, 5.0);
    }
    let (x0, y0, x1, y1) = (x0 - 2.0, y0 - 2.0, x1 + 2.0, y1 + 2.0);
    let ppm = (1800.0 / (x1 - x0).max(y1 - y0)).clamp(8.0, 40.0);
    let (w, h) = (((x1 - x0) * ppm) as usize + 1, ((y1 - y0) * ppm) as usize + 1);
    let to_px = |x: f32, y: f32| ((x - x0) * ppm, (y1 - y) * ppm);
    let mut c = Canvas::new(w, h, [17, 24, 39]);
    // 5 m grid.
    let mut g = (x0 / 5.0).ceil() * 5.0;
    while g < x1 {
        let (px, _) = to_px(g, 0.0);
        c.line((px, 0.0), (px, h as f32), [38, 46, 64], 1.0);
        g += 5.0;
    }
    let mut g = (y0 / 5.0).ceil() * 5.0;
    while g < y1 {
        let (_, py) = to_px(0.0, g);
        c.line((0.0, py), (w as f32, py), [38, 46, 64], 1.0);
        g += 5.0;
    }
    for p in &m.plan {
        let (px, py) = to_px(p[0], p[1]);
        c.put(px as i64, py as i64, [90, 96, 110], 0.35);
    }
    for (i, p) in m.pts.iter().enumerate() {
        let (px, py) = to_px(p[0], p[1]);
        let col = crate::view::class_colour(crate::view::class_of(m.spacing_mm[i] as f64));
        c.put(px as i64, py as i64, col, 1.0);
    }
    let font = crate::station::font();
    for wl in marks.walls {
        let (a, b) = (to_px(wl.from[0] as f32, wl.from[1] as f32), to_px(wl.to[0] as f32, wl.to[1] as f32));
        // A wall a target sits on is drawn as the target.
        if marks.faces.iter().any(|(_, f, _)| f.from == wl.from && f.to == wl.to) {
            continue;
        }
        let long = wl.length_m >= 3.0;
        c.line(a, b, [226, 232, 240], if long { 2.5 } else { 1.0 });
        if long {
            c.text(&font, 16.0, ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0 - 10.0), 0.5, &wl.wall_id, [226, 232, 240]);
        }
    }
    for (id, f, col) in &marks.faces {
        let (a, b) = (to_px(f.from[0] as f32, f.from[1] as f32), to_px(f.to[0] as f32, f.to[1] as f32));
        c.line(a, b, *col, 5.0);
        c.text(&font, 18.0, ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0 + 8.0), 0.5, id, *col);
    }
    for (n, pose) in marks.stations {
        let (px, py) = to_px(pose.xyz_m[0] as f32, pose.xyz_m[1] as f32);
        c.ellipse((px, py), (7.0, 7.0), [255, 255, 255], 1.0, true);
        // A tick for the way that station's scanner faced (its x axis).
        let t = pose.turn_deg.to_radians() as f32;
        c.line((px, py), (px + 16.0 * t.cos(), py - 16.0 * t.sin()), [255, 255, 255], 2.0);
        c.text(&font, 20.0, (px + 12.0, py - 32.0), 0.0, &n.to_string(), [255, 255, 255]);
    }
    c.text(&font, 16.0, (12.0, h as f32 - 34.0), 0.0, "grid 5 m · project frame (station 1's), not north", [203, 213, 225]);
    c.png()
}

// -------------------------------------------------------------- offline run ---

/// `x9-bridge register OUT_DIR STATION.tzf …`: register scan files offline
/// (the first is the frame) and write poses.json, pairs.json, walls.json and
/// plan.png to OUT_DIR, with timings. For checking and benchmarking.
pub fn cli(args: &[String]) -> anyhow::Result<()> {
    use anyhow::Context;
    let [out, files @ ..] = args else { anyhow::bail!("usage: x9-bridge register OUT_DIR STATION.tzf …") };
    std::fs::create_dir_all(out)?;
    let t0 = std::time::Instant::now();
    let mut clouds = Vec::new();
    for f in files {
        let t = trimble_tzf::Tzf::open(std::path::Path::new(f)).with_context(|| format!("opening {f}"))?;
        let meta = t.scan_meta().with_context(|| format!("{f}: metadata"))?;
        let pts = t.grid_points().with_context(|| format!("{f}: decoding"))?;
        let c = Cloud::from_grid(&pts, crate::view::step_rad(&meta));
        eprintln!("{f}: {} points -> {} voxels ({:.1} s)", pts.len(), c.pts.len(), t0.elapsed().as_secs_f64());
        clouds.push(c);
    }
    let t1 = std::time::Instant::now();
    let prep: Vec<Prepared> = clouds.iter().map(prepare).collect();
    eprintln!("prepared in {:.1} s", t1.elapsed().as_secs_f64());
    let ids: Vec<String> = (1..=files.len()).map(|i| i.to_string()).collect();
    // As the bridge runs it: one station more each time, keeping earlier pairs.
    let (mut pairs, mut poses) = (Vec::new(), Vec::new());
    for k in 1..=files.len() {
        let t = std::time::Instant::now();
        let refs: Vec<&Prepared> = prep[..k].iter().collect();
        (pairs, poses) = register_all(&ids[..k], &refs, pairs, &|b, a| eprintln!("  searching station {} against {}", b + 1, a + 1));
        let fits: Vec<String> = pairs.iter().filter(|p| p.good && (p.a == ids[k - 1] || p.b == ids[k - 1])).map(|p| format!("{}-{} {} mm", p.a, p.b, p.rms_mm.unwrap_or(f64::NAN))).collect();
        eprintln!("station {k}: {} ({:.1} s) fits {}", if poses[k - 1].is_some() { "placed" } else { "NOT placed" }, t.elapsed().as_secs_f64(), fits.join(", "));
    }
    let mut merged = Merged::default();
    let mut stations = Vec::new();
    for (k, p) in poses.iter().enumerate() {
        if let Some(p) = p {
            merged.add(&prep[k], clouds[k].step_rad, p, k as u16 + 1);
            stations.push((k as u16 + 1, *p));
        }
    }
    let ws = walls(&merged);
    std::fs::write(format!("{out}/poses.json"), serde_json::to_vec_pretty(&poses)?)?;
    std::fs::write(format!("{out}/pairs.json"), serde_json::to_vec_pretty(&pairs)?)?;
    std::fs::write(format!("{out}/walls.json"), serde_json::to_vec_pretty(&ws)?)?;
    std::fs::write(format!("{out}/plan.png"), plan_png(&merged, &PlanMarks { stations: &stations, walls: &ws, faces: vec![] }))?;
    let used = pairs.iter().filter(|p| p.good && !p.outlier).count();
    let worst = pairs.iter().filter_map(|p| p.left_mm).fold(0f64, f64::max);
    eprintln!("{} of {} stations registered, {used} pairs, worst leftover {worst} mm, {} walls; total {:.1} s",
              poses.iter().flatten().count(), poses.len(), ws.len(), t0.elapsed().as_secs_f64());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kd_tree_matches_brute_force() {
        let mut r = Rng(7);
        let pts: Vec<[f32; 3]> = (0..2000).map(|_| [r.below(1000) as f32 / 100.0, r.below(1000) as f32 / 100.0, r.below(300) as f32 / 100.0]).collect();
        let t = Tree::new(&pts);
        for q in pts.iter().step_by(97) {
            let q = [q[0] + 0.03, q[1] - 0.02, q[2] + 0.01];
            let brute = (0..pts.len()).min_by(|&a, &b| d2(pts[a], q).total_cmp(&d2(pts[b], q))).unwrap();
            assert_eq!(t.nearest(q, 1.0).unwrap().0, brute);
            let (nb, n) = t.knn::<5>(q);
            let mut all: Vec<f32> = pts.iter().map(|p| d2(*p, q)).collect();
            all.sort_by(|a, b| a.total_cmp(b));
            assert_eq!(n, 5);
            for k in 0..5 {
                assert!((nb[k].0 - all[k]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn smallest_eigenvector_of_a_flat_patch_is_its_normal() {
        let (v, curv) = smallest_eigen([[4.0, 0.0, 0.0], [0.0, 0.01, 0.0], [0.0, 0.0, 3.0]]);
        assert!(v[1].abs() > 0.999 && curv < 0.01);
    }

    #[test]
    fn poses_compose_and_invert() {
        let a = [0.3, 1.0, -2.0, 0.1];
        let rel = [1.1, 4.0, 2.0, -0.2];
        let b = compose(a, rel);
        let back = relative(a, b);
        for k in 0..4 {
            assert!((back[k] - rel[k]).abs() < 1e-12);
        }
        let ab = compose(b, invert(rel));
        for k in 0..4 {
            assert!((ab[k] - a[k]).abs() < 1e-12);
        }
    }

    /// A room-like L of walls seen from two stations: the global solve recovers
    /// the second station from a pair, and a wrong third pair can't drag it.
    #[test]
    fn global_fit_is_robust_to_a_wrong_pair() {
        let ids: Vec<String> = ["s1", "s2", "s3"].iter().map(|s| s.to_string()).collect();
        let truth2 = [0.5f64, 3.0, 1.0, 0.1];
        let truth3 = [1.2f64, -2.0, 4.0, 0.0];
        let mk = |a: &str, b: &str, rel: P4, good: bool| Pair {
            a: a.into(), b: b.into(), turn_deg: rel[0].to_degrees(), shift_m: [rel[1], rel[2], rel[3]], rms_mm: Some(10.0),
            matches: 20000, overlap: 0.3, coarse_score: 0.5, good, left_mm: None, left_deg: None, outlier: false,
        };
        let rel23 = relative(truth2, truth3);
        let mut wrong = rel23;
        wrong[1] += 2.0;
        let mut pairs = vec![mk("s1", "s2", truth2, true), mk("s1", "s3", truth3, true), mk("s2", "s3", rel23, true), mk("s2", "s3", wrong, true)];
        let poses = solve(&ids, &mut pairs);
        let p2 = poses[1].unwrap();
        assert!((p2.xyz_m[0] - 3.0).abs() < 0.01 && (p2.turn_deg - 0.5f64.to_degrees()).abs() < 0.05);
        assert!(pairs[3].outlier && !pairs[0].outlier);
    }

    #[test]
    fn a_wall_is_found_and_its_face_judged() {
        let mut m = Merged::default();
        // A 10 m wall along x at y = 5, 3 m tall, seen from station 1 over x 0..8.
        for i in 0..200 {
            for k in 0..30 {
                let x = i as f32 * 0.05;
                if x < 8.0 {
                    m.pts.push([x, 5.0, -1.5 + k as f32 * 0.1]);
                    m.spacing_mm.push(5.0);
                    m.station.push(1);
                }
            }
        }
        let ws = walls(&m);
        assert_eq!(ws.len(), 1);
        assert!((ws[0].length_m - 7.95).abs() < 0.1 && (ws[0].from[1] - 5.0).abs() < 0.02, "{:?}", ws[0]);
        let face = Face { from: [0.0, 5.0], to: [10.0, 5.0] };
        let scans: HashMap<u16, String> = [(1u16, "scan-1".to_string())].into();
        let v = face_coverage(&m, &face, &scans);
        assert_eq!(v["seen_share"], 0.8);
        assert_eq!(v["covered"], true);
        assert_eq!(v["stations"][0]["scan_id"], "scan-1");
        assert_eq!(v["unseen"][0]["to"], json!([10.0, 5.0]));
    }

    #[test]
    fn clouds_round_trip() {
        let c = Cloud { step_rad: 0.001, pts: vec![[1.0, 2.0, 3.0], [-1.0, 0.5, 0.0]], range: vec![3.7, 1.1] };
        let d = Cloud::from_bytes(&c.to_bytes()).unwrap();
        assert_eq!(d.pts, c.pts);
        assert_eq!(d.range, c.range);
        let mut m = Merged::default();
        m.pts.push([1.0, 2.0, 3.0]);
        m.spacing_mm.push(4.0);
        m.station.push(3);
        m.plan.push([5.0, 6.0]);
        let n = Merged::from_bytes(&m.to_bytes()).unwrap();
        assert_eq!((n.pts, n.station, n.plan), (m.pts, m.station, m.plan));
    }
}
