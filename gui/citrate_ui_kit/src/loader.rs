//! CitrateLoader morph engine — pure-Rust port of the canonical Citrate
//! loading animation (`citrate-landing/src/components/loader/CitrateLoader.tsx`).
//!
//! The mark is nine facets. Each frame every facet morphs between its home
//! position in the assembled triangle and an evenly-spaced slot on a
//! continuously spinning ring of tapered "liquid" strokes, peeling out
//! top-first clockwise (center facet last) and rebuilding in reverse, with
//! full-triangle and full-ring holds at the seams.
//!
//! Two layers:
//!
//! * [`MorphEngine`] and the free math functions ([`resample`], [`build_arc`],
//!   [`smootherstep`], …) — plain Rust, **no Slint types**, unit-testable.
//!   All heavy geometry (SVG outline sampling, arc construction, offset
//!   alignment) is precomputed once in [`MorphEngine::new`]; per-frame work is
//!   interpolation + path-string assembly only.
//! * [`start_loader`] / [`LoaderHandle`] — a thin adapter that drives a
//!   [`slint::Timer`] at ~60 fps and feeds per-facet SVG command strings into
//!   a `VecModel<SharedString>` which the caller binds to the
//!   `facet-commands` property of the `CitrateLoader` component
//!   (`ui/loader/citrate_loader.slint`).
//!
//! Wiring example (consumer crate, on the UI thread):
//!
//! ```ignore
//! let handle = citrate_ui_kit::loader::start_loader(Default::default());
//! app.set_loader_facets(handle.model()); // in property <[string]> loader-facets
//! // keep `handle` alive for as long as the loader should animate;
//! // handle.stop() / handle.restart() pause and resume.
//! ```

use std::fmt::Write as _;

/// Points each facet outline is resampled to. Matches the web loader's
/// `N = 160`. Lower it if profiling shows Path tessellation cost on
/// low-end machines.
pub const N_POINTS: usize = 160;

/// The mark has nine facets.
pub const FACET_COUNT: usize = 9;

/// SVG viewBox of the mark: `viewBox="11.63 2.14 100 100"`.
pub const VIEWBOX_X: f64 = 11.63;
pub const VIEWBOX_Y: f64 = 2.14;
pub const VIEWBOX_W: f64 = 100.0;
pub const VIEWBOX_H: f64 = 100.0;

/// The nine facet outlines of the Citrate triangle mark, transcribed exactly
/// from `CitrateLoader.tsx` (`PATHS`). Coordinates live in the viewBox space
/// above.
pub const FACET_PATHS: [&str; FACET_COUNT] = [
    "M40.05,65c2.21-.14,4.02-.63,5.3-1.1-1.94-2.21-3.88-4.41-5.82-6.62l-4.27,7.4c1.23.24,2.87.43,4.79.31Z",
    "M41.7,73.61c4.34-.04,7.98-.72,10.68-1.44-2.03-2.28-4.06-4.56-6.1-6.84-1.4.54-3.41,1.14-5.88,1.35-2.41.2-4.45-.04-5.9-.32l-3.47,6.01c2.71.66,6.35,1.28,10.68,1.24Z",
    "M53.37,74.02c-2.94.76-6.87,1.48-11.53,1.53-4.65.05-8.58-.59-11.53-1.28-.86,1.5-1.73,2.99-2.59,4.49-1.03,1.78.26,4.01,2.32,4.01h30.93c-2.53-2.92-5.06-5.83-7.59-8.75Z",
    "M62.58,47.91c.69-.85,1.4-1.69,2.13-2.51,1.49-1.69,3.08-3.3,4.81-4.74.92-.76,1.98-1.39,3.04-1.95.16-.08.32-.16.48-.25l-8.79-15.22c-1.03-1.78-3.6-1.78-4.63,0l-8.62,14.93c3.86,3.25,7.72,6.49,11.57,9.74Z",
    "M75.24,58.35c.44-.33.9-.66,1.37-.96,1.4-.89,2.8-1.77,4.27-2.56.5-.27,1.01-.53,1.52-.78l-8-13.85c-.86.42-1.69.9-2.48,1.43-.94.63-1.78,1.4-2.6,2.17-1.62,1.53-3.09,3.2-4.51,4.91-.17.2-.33.41-.5.61,3.64,3.01,7.29,6.02,10.93,9.03Z",
    "M79.77,57.69c-1.14.7-2.12,1.42-2.94,2.1,6.07,5.26,12.14,10.51,18.21,15.77l-11.43-19.8c-1.14.46-2.45,1.09-3.84,1.93Z",
    "M75.19,61.37s-.01.01-.02.01c-2.22,1.74-3.69,3.28-4.22,3.82-1.88,1.93-2.99,3.07-4.71,4.26-2.4,1.66-4.66,2.49-6.6,3.21-1.3.48-2.41.82-3.23,1.04,2.62,3.04,5.25,6.07,7.87,9.11h29.33c1.77,0,2.96-1.64,2.61-3.23-7.01-6.07-14.02-12.15-21.03-18.22Z",
    "M55.03,71.84c.12-.03.3-.07.51-.12,1.32-.33,3.76-.93,5.99-1.92,2.95-1.31,5-3.05,5.84-3.82,1.21-1.1,1.45-1.58,3.58-3.65,1.16-1.12,2.15-2,2.82-2.59-3.57-2.95-7.14-5.89-10.71-8.84-2.36,2.93-4.65,5.92-7.3,8.61-2.08,2.1-4.39,3.99-7.03,5.26l6.29,7.07Z",
    "M47.12,63.06c3.38-1.53,6.18-4.17,8.64-6.92,1.95-2.18,3.73-4.49,5.56-6.76-3.81-3.2-7.61-6.4-11.42-9.6l-9.21,15.96c2.14,2.44,4.29,4.88,6.43,7.32Z",
];

/// A 2-D point in viewBox space.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

/// A point in polar coordinates (about whatever origin the context defines).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Polar {
    pub rad: f64,
    pub ang: f64,
}

/// Animation parameters. Field-for-field mirror of the web component's props
/// (defaults identical).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MorphConfig {
    /// Playback speed multiplier.
    pub speed: f64,
    /// Full macro-cycle length in ms (break apart + reassemble), before speed.
    pub cycle_ms: f64,
    /// Whole ring turns per cycle (integer keeps the loop seamless).
    pub turns: f64,
    /// Fraction of the cycle held on the full triangle and the full ring.
    pub hold: f64,
    /// Per-facet morph ramp, as a fraction of a half-cycle.
    pub ramp: f64,
    /// Ring radius in the 100-unit viewBox space.
    pub ring_radius: f64,
    /// Angular sweep of each liquid stroke, degrees.
    pub arc_sweep_deg: f64,
    /// Max stroke thickness in viewBox units.
    pub thickness: f64,
}

impl Default for MorphConfig {
    fn default() -> Self {
        Self {
            speed: 2.1,
            cycle_ms: 11_000.0,
            turns: 1.0,
            hold: 0.008,
            ramp: 0.09,
            ring_radius: 52.0,
            arc_sweep_deg: 26.0,
            thickness: 18.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure math (ports of the TSX helpers)
// ---------------------------------------------------------------------------

/// Smootherstep easing: `p³(p(6p − 15) + 10)`, clamped to [0, 1].
pub fn smootherstep(p: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    p * p * p * (p * (p * 6.0 - 15.0) + 10.0)
}

/// Signed area of a closed polygon (shoelace / 2). Sign encodes winding.
pub fn signed_area(pts: &[Pt]) -> f64 {
    let mut s = 0.0;
    for i in 0..pts.len() {
        let a = pts[i];
        let b = pts[(i + 1) % pts.len()];
        s += a.x * b.y - b.x * a.y;
    }
    s / 2.0
}

/// Resample a closed polygon to exactly `n` points, evenly spaced by arc
/// length, starting at `pts[0]`. Direct port of the TSX `resample`.
pub fn resample(pts: &[Pt], n: usize) -> Vec<Pt> {
    let len = pts.len();
    let mut seg = Vec::with_capacity(len);
    let mut total = 0.0;
    for i in 0..len {
        let a = pts[i];
        let b = pts[(i + 1) % len];
        let d = (b.x - a.x).hypot(b.y - a.y);
        seg.push(d);
        total += d;
    }
    let mut out = Vec::with_capacity(n);
    let step = total / n as f64;
    let mut i = 0usize;
    let mut acc = 0.0f64;
    for j in 0..n {
        let dist = j as f64 * step;
        while i < seg.len() - 1 && acc + seg[i] < dist {
            acc += seg[i];
            i += 1;
        }
        let a = pts[i];
        let b = pts[(i + 1) % len];
        let f = if seg[i] > 0.0 {
            (dist - acc) / seg[i]
        } else {
            0.0
        };
        out.push(Pt {
            x: a.x + (b.x - a.x) * f,
            y: a.y + (b.y - a.y) * f,
        });
    }
    out
}

/// One liquid stroke: pointed thin tail to a rounded fat head, curved along
/// the ring. Returns the resampled outline as polar coords about the origin,
/// plus the signed area of the resampled cartesian polygon (winding probe).
/// Direct port of the TSX `buildArc`.
pub fn build_arc(r: f64, sweep_deg: f64, t_max: f64, n: usize) -> (Vec<Polar>, f64) {
    let sweep = sweep_deg.to_radians();
    const M: usize = 200;
    const CAP: usize = 26;
    let mut dense: Vec<Pt> = Vec::with_capacity(2 * (M + 1) + CAP);
    let thick = |s: f64| t_max * s.powf(0.62);
    for k in 0..=M {
        let s = k as f64 / M as f64;
        let ang = -sweep / 2.0 + sweep * s;
        let ro = r + thick(s) / 2.0;
        dense.push(Pt {
            x: ro * ang.cos(),
            y: ro * ang.sin(),
        });
    }
    let ang1 = sweep / 2.0;
    let rad = Pt {
        x: ang1.cos(),
        y: ang1.sin(),
    };
    let tan = Pt {
        x: -ang1.sin(),
        y: ang1.cos(),
    };
    let cc = Pt {
        x: r * ang1.cos(),
        y: r * ang1.sin(),
    };
    let cr = t_max / 2.0;
    for k in 1..CAP {
        let phi = std::f64::consts::PI * k as f64 / CAP as f64;
        dense.push(Pt {
            x: cc.x + cr * (phi.cos() * rad.x + phi.sin() * tan.x),
            y: cc.y + cr * (phi.cos() * rad.y + phi.sin() * tan.y),
        });
    }
    for k in (0..=M).rev() {
        let s = k as f64 / M as f64;
        let ang = -sweep / 2.0 + sweep * s;
        let ri = r - thick(s) / 2.0;
        dense.push(Pt {
            x: ri * ang.cos(),
            y: ri * ang.sin(),
        });
    }
    let res = resample(&dense, n);
    let sign = signed_area(&res);
    let polar = res
        .iter()
        .map(|p| Polar {
            rad: p.x.hypot(p.y),
            ang: p.y.atan2(p.x),
        })
        .collect();
    (polar, sign)
}

/// Rotation-match a facet's home points against its ring slot: find the
/// cyclic index offset that minimizes summed squared distance (sampled every
/// 4th point, with early exit). Direct port of the TSX `alignOffsets` body.
fn align_offset(src: &[Pt], arc: &[Polar], slot_deg: f64, cx: f64, cy: f64) -> usize {
    let n = src.len();
    let base = slot_deg.to_radians();
    let mut tx = vec![0.0f64; n];
    let mut ty = vec![0.0f64; n];
    for i in 0..n {
        let a = base + arc[i].ang;
        tx[i] = cx + arc[i].rad * a.cos();
        ty[i] = cy + arc[i].rad * a.sin();
    }
    let mut best = 0usize;
    let mut best_err = f64::INFINITY;
    for o in 0..n {
        let mut err = 0.0f64;
        let mut k = 0usize;
        while k < n {
            let s = src[k];
            let j = (k + o) % n;
            let dx = s.x - tx[j];
            let dy = s.y - ty[j];
            err += dx * dx + dy * dy;
            if err >= best_err {
                break;
            }
            k += 4;
        }
        if err < best_err {
            best_err = err;
            best = o;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// SVG path outline sampling
// ---------------------------------------------------------------------------
//
// The web version leans on the DOM (`getTotalLength` / `getPointAtLength`)
// to turn each facet's `d` attribute into N evenly spaced points. Here we
// parse the subset of SVG path syntax the mark uses (plus the common rest),
// flatten curves to a dense polyline, and reuse `resample` for the
// equal-arc-length sampling — numerically equivalent.

/// Segments each cubic/quadratic Bézier is flattened into before resampling.
const CURVE_SUBDIV: usize = 48;

/// Parse an SVG path `d` string into a dense closed polyline.
///
/// Supports M/m L/l H/h V/v C/c S/s Q/q T/t Z/z (no elliptical arcs — the
/// mark does not use them). Only single-subpath outlines are expected.
pub fn sample_svg_outline(d: &str) -> Result<Vec<Pt>, String> {
    let bytes = d.as_bytes();
    let mut pos = 0usize;

    let skip_sep = |pos: &mut usize| {
        while *pos < bytes.len() && (bytes[*pos].is_ascii_whitespace() || bytes[*pos] == b',') {
            *pos += 1;
        }
    };
    // SVG number: sign? digits? ('.' digits*)? exponent?  — a second '.'
    // terminates the number (e.g. "1.23.24" is two numbers).
    let next_number = |pos: &mut usize| -> Option<f64> {
        skip_sep(pos);
        let start = *pos;
        let mut p = *pos;
        if p < bytes.len() && (bytes[p] == b'+' || bytes[p] == b'-') {
            p += 1;
        }
        let mut digits = 0;
        while p < bytes.len() && bytes[p].is_ascii_digit() {
            p += 1;
            digits += 1;
        }
        if p < bytes.len() && bytes[p] == b'.' {
            p += 1;
            while p < bytes.len() && bytes[p].is_ascii_digit() {
                p += 1;
                digits += 1;
            }
        }
        if digits == 0 {
            return None;
        }
        if p < bytes.len() && (bytes[p] == b'e' || bytes[p] == b'E') {
            let mut q = p + 1;
            if q < bytes.len() && (bytes[q] == b'+' || bytes[q] == b'-') {
                q += 1;
            }
            let mut exp_digits = 0;
            while q < bytes.len() && bytes[q].is_ascii_digit() {
                q += 1;
                exp_digits += 1;
            }
            if exp_digits > 0 {
                p = q;
            }
        }
        let s = std::str::from_utf8(&bytes[start..p]).ok()?;
        let v = s.parse::<f64>().ok()?;
        *pos = p;
        Some(v)
    };

    let mut out: Vec<Pt> = Vec::new();
    let mut cur = Pt::default();
    let mut start = Pt::default();
    let mut prev_cubic_ctrl: Option<Pt> = None;
    let mut prev_quad_ctrl: Option<Pt> = None;
    let mut cmd: u8 = 0;

    let flatten_cubic = |out: &mut Vec<Pt>, p0: Pt, c1: Pt, c2: Pt, p3: Pt| {
        for i in 1..=CURVE_SUBDIV {
            let t = i as f64 / CURVE_SUBDIV as f64;
            let mt = 1.0 - t;
            let x = mt * mt * mt * p0.x
                + 3.0 * mt * mt * t * c1.x
                + 3.0 * mt * t * t * c2.x
                + t * t * t * p3.x;
            let y = mt * mt * mt * p0.y
                + 3.0 * mt * mt * t * c1.y
                + 3.0 * mt * t * t * c2.y
                + t * t * t * p3.y;
            out.push(Pt { x, y });
        }
    };
    let flatten_quad = |out: &mut Vec<Pt>, p0: Pt, c: Pt, p2: Pt| {
        for i in 1..=CURVE_SUBDIV {
            let t = i as f64 / CURVE_SUBDIV as f64;
            let mt = 1.0 - t;
            let x = mt * mt * p0.x + 2.0 * mt * t * c.x + t * t * p2.x;
            let y = mt * mt * p0.y + 2.0 * mt * t * c.y + t * t * p2.y;
            out.push(Pt { x, y });
        }
    };

    loop {
        skip_sep(&mut pos);
        if pos >= bytes.len() {
            break;
        }
        let ch = bytes[pos];
        if ch.is_ascii_alphabetic() {
            cmd = ch;
            pos += 1;
            if cmd == b'Z' || cmd == b'z' {
                cur = start;
                continue;
            }
        } else if cmd == 0 {
            return Err(format!("path does not start with a command: {d:?}"));
        } else if cmd == b'M' {
            cmd = b'L'; // implicit lineto after moveto
        } else if cmd == b'm' {
            cmd = b'l';
        }

        let rel = cmd.is_ascii_lowercase();
        let upper = cmd.to_ascii_uppercase();
        match upper {
            b'M' => {
                let x = next_number(&mut pos).ok_or("expected number after M")?;
                let y = next_number(&mut pos).ok_or("expected number after M")?;
                cur = if rel {
                    Pt {
                        x: cur.x + x,
                        y: cur.y + y,
                    }
                } else {
                    Pt { x, y }
                };
                start = cur;
                out.push(cur);
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
            }
            b'L' => {
                let x = next_number(&mut pos).ok_or("expected number after L")?;
                let y = next_number(&mut pos).ok_or("expected number after L")?;
                cur = if rel {
                    Pt {
                        x: cur.x + x,
                        y: cur.y + y,
                    }
                } else {
                    Pt { x, y }
                };
                out.push(cur);
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
            }
            b'H' => {
                let x = next_number(&mut pos).ok_or("expected number after H")?;
                cur = Pt {
                    x: if rel { cur.x + x } else { x },
                    y: cur.y,
                };
                out.push(cur);
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
            }
            b'V' => {
                let y = next_number(&mut pos).ok_or("expected number after V")?;
                cur = Pt {
                    x: cur.x,
                    y: if rel { cur.y + y } else { y },
                };
                out.push(cur);
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
            }
            b'C' | b'S' => {
                let c1 = if upper == b'C' {
                    let x1 = next_number(&mut pos).ok_or("expected number in C")?;
                    let y1 = next_number(&mut pos).ok_or("expected number in C")?;
                    if rel {
                        Pt {
                            x: cur.x + x1,
                            y: cur.y + y1,
                        }
                    } else {
                        Pt { x: x1, y: y1 }
                    }
                } else {
                    // Smooth: reflect the previous cubic control about `cur`.
                    match prev_cubic_ctrl {
                        Some(pc) => Pt {
                            x: 2.0 * cur.x - pc.x,
                            y: 2.0 * cur.y - pc.y,
                        },
                        None => cur,
                    }
                };
                let x2 = next_number(&mut pos).ok_or("expected number in C/S")?;
                let y2 = next_number(&mut pos).ok_or("expected number in C/S")?;
                let x = next_number(&mut pos).ok_or("expected number in C/S")?;
                let y = next_number(&mut pos).ok_or("expected number in C/S")?;
                let c2 = if rel {
                    Pt {
                        x: cur.x + x2,
                        y: cur.y + y2,
                    }
                } else {
                    Pt { x: x2, y: y2 }
                };
                let end = if rel {
                    Pt {
                        x: cur.x + x,
                        y: cur.y + y,
                    }
                } else {
                    Pt { x, y }
                };
                flatten_cubic(&mut out, cur, c1, c2, end);
                prev_cubic_ctrl = Some(c2);
                prev_quad_ctrl = None;
                cur = end;
            }
            b'Q' | b'T' => {
                let c = if upper == b'Q' {
                    let x1 = next_number(&mut pos).ok_or("expected number in Q")?;
                    let y1 = next_number(&mut pos).ok_or("expected number in Q")?;
                    if rel {
                        Pt {
                            x: cur.x + x1,
                            y: cur.y + y1,
                        }
                    } else {
                        Pt { x: x1, y: y1 }
                    }
                } else {
                    match prev_quad_ctrl {
                        Some(pc) => Pt {
                            x: 2.0 * cur.x - pc.x,
                            y: 2.0 * cur.y - pc.y,
                        },
                        None => cur,
                    }
                };
                let x = next_number(&mut pos).ok_or("expected number in Q/T")?;
                let y = next_number(&mut pos).ok_or("expected number in Q/T")?;
                let end = if rel {
                    Pt {
                        x: cur.x + x,
                        y: cur.y + y,
                    }
                } else {
                    Pt { x, y }
                };
                flatten_quad(&mut out, cur, c, end);
                prev_quad_ctrl = Some(c);
                prev_cubic_ctrl = None;
                cur = end;
            }
            _ => return Err(format!("unsupported path command {:?}", cmd as char)),
        }
    }
    if out.len() < 3 {
        return Err(format!("degenerate outline ({} points): {d:?}", out.len()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Facet {
    /// N home points in viewBox space (orientation-fixed).
    src: Vec<Pt>,
    /// `src` in polar coords about the global centroid.
    src_polar: Vec<Polar>,
    /// Peel order: 0 leaves first (top, clockwise); center facet is last.
    order: usize,
    /// Ring slot angle in degrees (slot 0 at −90° = top).
    slot_deg: f64,
    /// Cyclic point-index offset rotation-matching `src` to the ring stroke.
    offset: usize,
}

/// Precomputed morph state. Construction does all the heavy geometry; call
/// [`MorphEngine::frame_commands`] (or [`MorphEngine::frame_points`]) with a
/// time since start in ms to get the nine facet outlines for that instant,
/// in the same order as [`FACET_PATHS`].
#[derive(Debug, Clone)]
pub struct MorphEngine {
    cfg: MorphConfig,
    cx: f64,
    cy: f64,
    arc: Vec<Polar>,
    facets: Vec<Facet>,
}

impl MorphEngine {
    pub fn new(cfg: MorphConfig) -> Self {
        // Winding probe — the web version derives the target winding from
        // `buildArc(30, 30, 11, N)` before the real arc exists; keep that
        // exact call so orientation matches the reference bit-for-bit.
        let (_, target_sign) = build_arc(30.0, 30.0, 11.0, N_POINTS);
        let (arc, _) = build_arc(cfg.ring_radius, cfg.arc_sweep_deg, cfg.thickness, N_POINTS);

        // Sample each facet outline to N points; unify winding with the arc.
        let mut srcs: Vec<Vec<Pt>> = FACET_PATHS
            .iter()
            .map(|d| {
                let dense = sample_svg_outline(d).expect("FACET_PATHS entry failed to parse");
                resample(&dense, N_POINTS)
            })
            .collect();
        for src in &mut srcs {
            if signed_area(src).signum() != target_sign.signum() {
                src.reverse();
            }
        }

        // Global centroid of all sampled points.
        let (mut gx, mut gy, mut gc) = (0.0f64, 0.0f64, 0usize);
        for src in &srcs {
            for p in src {
                gx += p.x;
                gy += p.y;
                gc += 1;
            }
        }
        let cx = gx / gc as f64;
        let cy = gy / gc as f64;

        // Per-facet centroid → clockwise-from-top rank.
        struct Meta {
            idx: usize,
            rad: f64,
            cw: f64,
        }
        let metas: Vec<Meta> = srcs
            .iter()
            .enumerate()
            .map(|(idx, src)| {
                let (mut sx, mut sy) = (0.0, 0.0);
                for p in src {
                    sx += p.x;
                    sy += p.y;
                }
                let fx = sx / src.len() as f64;
                let fy = sy / src.len() as f64;
                let rad = (fx - cx).hypot(fy - cy);
                let ang = (fy - cy).atan2(fx - cx).to_degrees();
                let cw = ((ang + 90.0) % 360.0 + 360.0) % 360.0;
                Meta { idx, rad, cw }
            })
            .collect();

        // Peel order: by clockwise angle from top, center facet forced last.
        let mut by_angle: Vec<usize> = (0..metas.len()).collect();
        by_angle.sort_by(|&a, &b| metas[a].cw.partial_cmp(&metas[b].cw).unwrap());
        let mut center = 0usize;
        for m in &metas {
            if m.rad < metas[center].rad {
                center = m.idx;
            }
        }
        let mut peel: Vec<usize> = by_angle.into_iter().filter(|&i| i != center).collect();
        peel.push(center);

        let nn = peel.len() as f64;
        let step = 360.0 / nn;
        let mut order = vec![0usize; srcs.len()];
        let mut slot = vec![0.0f64; srcs.len()];
        for (m, &idx) in peel.iter().enumerate() {
            order[idx] = m;
            slot[idx] = -90.0 + m as f64 * step;
        }

        let facets: Vec<Facet> = srcs
            .into_iter()
            .enumerate()
            .map(|(idx, src)| {
                let src_polar = src
                    .iter()
                    .map(|p| Polar {
                        rad: (p.x - cx).hypot(p.y - cy),
                        ang: (p.y - cy).atan2(p.x - cx),
                    })
                    .collect();
                let offset = align_offset(&src, &arc, slot[idx], cx, cy);
                Facet {
                    src,
                    src_polar,
                    order: order[idx],
                    slot_deg: slot[idx],
                    offset,
                }
            })
            .collect();

        Self {
            cfg,
            cx,
            cy,
            arc,
            facets,
        }
    }

    /// Centroid of the assembled mark (viewBox space).
    pub fn centroid(&self) -> Pt {
        Pt {
            x: self.cx,
            y: self.cy,
        }
    }

    pub fn config(&self) -> &MorphConfig {
        &self.cfg
    }

    /// The N resampled home points of facet `idx` (assembled-triangle pose).
    pub fn home_points(&self, idx: usize) -> &[Pt] {
        &self.facets[idx].src
    }

    /// Morph progress (0 = home, 1 = on ring) for facet `idx` at `phase` ∈ [0, 1).
    fn eased_progress(&self, order: usize, phase: f64) -> f64 {
        let c = &self.cfg;
        let n = self.facets.len() as f64;
        let span = 0.05f64.max((1.0 - 2.0 * c.hold) / 2.0);
        let down_start = c.hold;
        let circ_start = down_start + span;
        let up_start = circ_start + c.hold;
        let m = order as f64;
        let out_at = down_start + (m / n) * (span - c.ramp);
        let in_at = up_start + (m / n) * (span - c.ramp);
        if phase < out_at {
            0.0
        } else if phase < out_at + c.ramp {
            smootherstep((phase - out_at) / c.ramp)
        } else if phase < in_at {
            1.0
        } else if phase < in_at + c.ramp {
            smootherstep(1.0 - (phase - in_at) / c.ramp)
        } else {
            0.0
        }
    }

    /// Facet outlines at `t_ms` (milliseconds since animation start), in
    /// [`FACET_PATHS`] order. Each outline has [`N_POINTS`] points.
    pub fn frame_points(&self, t_ms: f64) -> Vec<Vec<Pt>> {
        let c = &self.cfg;
        let t = c.cycle_ms / c.speed;
        let phase = (t_ms.rem_euclid(t)) / t;
        let spin = 360.0 * c.turns * phase;

        self.facets
            .iter()
            .map(|pc| {
                let ep = self.eased_progress(pc.order, phase);
                let mid_ang = (pc.slot_deg + spin).to_radians();
                let n = pc.src_polar.len();
                let mut pts = Vec::with_capacity(n);
                for k in 0..n {
                    let ai = (k + pc.offset) % n;
                    let rr = self.arc[ai].rad;
                    let ar = mid_ang + self.arc[ai].ang;
                    let hp = pc.src_polar[k];
                    let r = hp.rad + (rr - hp.rad) * ep;
                    let mut da = ar - hp.ang;
                    da = da.sin().atan2(da.cos());
                    let a = hp.ang + da * ep;
                    pts.push(Pt {
                        x: self.cx + r * a.cos(),
                        y: self.cy + r * a.sin(),
                    });
                }
                pts
            })
            .collect()
    }

    /// Facet outlines at `t_ms` as SVG path-command strings
    /// (`M x y L x y … Z`, 2 decimals — same as the web loader), in
    /// [`FACET_PATHS`] order. Feed these to `CitrateLoader.facet-commands`.
    pub fn frame_commands(&self, t_ms: f64) -> Vec<String> {
        self.frame_points(t_ms)
            .into_iter()
            .map(|pts| {
                let mut d = String::with_capacity(pts.len() * 16 + 2);
                for (k, p) in pts.iter().enumerate() {
                    let _ = write!(d, "{}{:.2} {:.2}", if k == 0 { 'M' } else { 'L' }, p.x, p.y);
                }
                d.push('Z');
                d
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Slint adapter
// ---------------------------------------------------------------------------

/// Keeps the loader animation alive: owns the ~60 fps [`slint::Timer`] and
/// the `VecModel<SharedString>` holding the nine per-facet command strings.
/// Dropping the handle stops the animation (the model freezes on the last
/// frame; bind `running: false` on the component to snap back to the mark).
pub struct LoaderHandle {
    timer: slint::Timer,
    model: std::rc::Rc<slint::VecModel<slint::SharedString>>,
}

impl LoaderHandle {
    /// Model to assign to the `facet-commands` property of `CitrateLoader`.
    pub fn model(&self) -> slint::ModelRc<slint::SharedString> {
        slint::ModelRc::from(self.model.clone())
    }

    /// Pause the animation (model keeps the last frame).
    pub fn stop(&self) {
        self.timer.stop();
    }

    /// Resume after [`LoaderHandle::stop`].
    pub fn restart(&self) {
        self.timer.restart();
    }

    pub fn running(&self) -> bool {
        self.timer.running()
    }
}

/// Start driving the loader at ~60 fps. Must be called on the Slint event
/// loop (UI) thread. Returns a [`LoaderHandle`]; keep it alive for as long
/// as the loader should animate.
pub fn start_loader(cfg: MorphConfig) -> LoaderHandle {
    let engine = MorphEngine::new(cfg);
    let initial: Vec<slint::SharedString> = engine
        .frame_commands(0.0)
        .into_iter()
        .map(slint::SharedString::from)
        .collect();
    let model = std::rc::Rc::new(slint::VecModel::from(initial));

    let timer = slint::Timer::default();
    let started = std::time::Instant::now();
    let model_for_timer = model.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        move || {
            use slint::Model as _;
            let t_ms = started.elapsed().as_secs_f64() * 1000.0;
            for (i, cmd) in engine.frame_commands(t_ms).into_iter().enumerate() {
                model_for_timer.set_row_data(i, cmd.into());
            }
        },
    );

    LoaderHandle { timer, model }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn square(side: f64) -> Vec<Pt> {
        vec![
            Pt { x: 0.0, y: 0.0 },
            Pt { x: side, y: 0.0 },
            Pt { x: side, y: side },
            Pt { x: 0.0, y: side },
        ]
    }

    #[test]
    fn ease_boundaries_and_monotonic() {
        assert_eq!(smootherstep(0.0), 0.0);
        assert_eq!(smootherstep(1.0), 1.0);
        // Clamped outside [0,1].
        assert_eq!(smootherstep(-0.5), 0.0);
        assert_eq!(smootherstep(1.5), 1.0);
        // Monotonic non-decreasing on [0,1].
        let mut prev = 0.0;
        for i in 0..=1000 {
            let v = smootherstep(i as f64 / 1000.0);
            assert!(
                v >= prev - EPS,
                "ease not monotonic at step {i}: {v} < {prev}"
            );
            prev = v;
        }
    }

    #[test]
    fn resample_count_and_endpoints() {
        let pts = square(10.0);
        let out = resample(&pts, N_POINTS);
        assert_eq!(out.len(), N_POINTS);
        // Starts at the input's first point.
        assert!((out[0].x - pts[0].x).abs() < EPS && (out[0].y - pts[0].y).abs() < EPS);
        // Even spacing: perimeter 40, step 0.25; the last point sits one step
        // short of closing the loop back to out[0].
        let step = 40.0 / N_POINTS as f64;
        let last = out[N_POINTS - 1];
        let close = (last.x - out[0].x).hypot(last.y - out[0].y);
        assert!(
            close > 0.0 && close <= step + EPS,
            "closing gap {close} vs step {step}"
        );
        // Consecutive spacing is uniform (square has no curvature shortcuts
        // except at corners, where chord ≤ arc).
        for w in out.windows(2) {
            let d = (w[1].x - w[0].x).hypot(w[1].y - w[0].y);
            assert!(d <= step + EPS, "spacing {d} exceeds step {step}");
        }
    }

    #[test]
    fn svg_outlines_parse_and_look_like_the_mark() {
        for (i, d) in FACET_PATHS.iter().enumerate() {
            let dense = sample_svg_outline(d).unwrap_or_else(|e| panic!("facet {i}: {e}"));
            assert!(dense.len() > 10, "facet {i} suspiciously sparse");
            let area = signed_area(&resample(&dense, N_POINTS));
            assert!(area.abs() > 0.5, "facet {i} degenerate area {area}");
        }
        // Parser sanity: the assembled mark must land inside the viewBox
        // (11.63 2.14 100 100) and span a substantial part of it. (The
        // point-cloud centroid is NOT the viewBox center — the web version
        // computes it too, landing near (61.6, 61.2).)
        let engine = MorphEngine::new(MorphConfig::default());
        let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
        let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for i in 0..FACET_COUNT {
            for p in engine.home_points(i) {
                min_x = min_x.min(p.x);
                min_y = min_y.min(p.y);
                max_x = max_x.max(p.x);
                max_y = max_y.max(p.y);
            }
        }
        assert!(
            min_x >= VIEWBOX_X && max_x <= VIEWBOX_X + VIEWBOX_W,
            "x span {min_x}..{max_x}"
        );
        assert!(
            min_y >= VIEWBOX_Y && max_y <= VIEWBOX_Y + VIEWBOX_H,
            "y span {min_y}..{max_y}"
        );
        assert!(
            max_x - min_x > 40.0 && max_y - min_y > 40.0,
            "mark suspiciously small"
        );
        let c = engine.centroid();
        assert!((c.x - 61.63).abs() < 8.0, "centroid x {}", c.x);
        assert!(c.y > min_y && c.y < max_y, "centroid y {}", c.y);
    }

    #[test]
    fn at_t0_and_during_hold_output_is_home_geometry() {
        let cfg = MorphConfig::default();
        let engine = MorphEngine::new(cfg);
        let t_cycle = cfg.cycle_ms / cfg.speed;
        // t = 0 and the middle of the initial full-triangle hold.
        for t_ms in [0.0, 0.5 * cfg.hold * t_cycle] {
            let frame = engine.frame_points(t_ms);
            assert_eq!(frame.len(), FACET_COUNT);
            for (i, pts) in frame.iter().enumerate() {
                assert_eq!(pts.len(), N_POINTS);
                for (k, p) in pts.iter().enumerate() {
                    let h = engine.home_points(i)[k];
                    assert!(
                        (p.x - h.x).abs() < 1e-6 && (p.y - h.y).abs() < 1e-6,
                        "facet {i} pt {k} at t={t_ms}: ({}, {}) != home ({}, {})",
                        p.x,
                        p.y,
                        h.x,
                        h.y
                    );
                }
            }
        }
    }

    #[test]
    fn at_full_ring_phase_points_lie_on_the_ring() {
        let cfg = MorphConfig::default();
        let engine = MorphEngine::new(cfg);
        let t_cycle = cfg.cycle_ms / cfg.speed;
        // Middle of the full-ring hold: phase = hold + span + hold/2.
        let span = 0.05f64.max((1.0 - 2.0 * cfg.hold) / 2.0);
        let phase = cfg.hold + span + cfg.hold / 2.0;
        let frame = engine.frame_points(phase * t_cycle);
        let c = engine.centroid();
        let r_min = cfg.ring_radius - cfg.thickness / 2.0 - 1.0; // resample chord slack
        let r_max = cfg.ring_radius + cfg.thickness / 2.0 + 1.0;
        for (i, pts) in frame.iter().enumerate() {
            for (k, p) in pts.iter().enumerate() {
                let r = (p.x - c.x).hypot(p.y - c.y);
                assert!(
                    (r_min..=r_max).contains(&r),
                    "facet {i} pt {k} radius {r} outside ring [{r_min}, {r_max}]"
                );
            }
        }
    }

    #[test]
    fn build_arc_is_a_closed_polygon_with_expected_count() {
        let (arc, sign) = build_arc(52.0, 26.0, 18.0, N_POINTS);
        assert_eq!(arc.len(), N_POINTS);
        assert!(sign.abs() > 1.0, "arc polygon area collapsed: {sign}");
        for (k, p) in arc.iter().enumerate() {
            assert!(
                p.rad >= 52.0 - 9.0 - 1.0 && p.rad <= 52.0 + 9.0 + 1.0,
                "arc pt {k} radius {} outside stroke envelope",
                p.rad
            );
        }
        // Winding is construction-invariant (the engine relies on the
        // web version's `buildArc(30, 30, 11)` probe being representative).
        let (_, probe_sign) = build_arc(30.0, 30.0, 11.0, N_POINTS);
        assert_eq!(sign.signum(), probe_sign.signum());
    }

    #[test]
    fn facet_ordering_center_last_and_slots_even() {
        let engine = MorphEngine::new(MorphConfig::default());
        let mut orders: Vec<usize> = engine.facets.iter().map(|f| f.order).collect();
        orders.sort_unstable();
        assert_eq!(orders, (0..FACET_COUNT).collect::<Vec<_>>());
        // The center facet (smallest centroid radius) peels last.
        let c = engine.centroid();
        let mut center = 0;
        let mut best = f64::INFINITY;
        for (i, f) in engine.facets.iter().enumerate() {
            let (mut sx, mut sy) = (0.0, 0.0);
            for p in &f.src {
                sx += p.x;
                sy += p.y;
            }
            let r = (sx / f.src.len() as f64 - c.x).hypot(sy / f.src.len() as f64 - c.y);
            if r < best {
                best = r;
                center = i;
            }
        }
        assert_eq!(engine.facets[center].order, FACET_COUNT - 1);
        // Slots are -90 + 40k degrees.
        for f in &engine.facets {
            let expected = -90.0 + f.order as f64 * (360.0 / FACET_COUNT as f64);
            assert!((f.slot_deg - expected).abs() < EPS);
            assert!(f.offset < N_POINTS);
        }
    }

    #[test]
    fn frame_commands_are_well_formed() {
        let engine = MorphEngine::new(MorphConfig::default());
        for t_ms in [0.0, 1234.5, 4000.0] {
            let cmds = engine.frame_commands(t_ms);
            assert_eq!(cmds.len(), FACET_COUNT);
            for d in &cmds {
                assert!(
                    d.starts_with('M'),
                    "missing moveto: {}",
                    &d[..20.min(d.len())]
                );
                assert!(d.ends_with('Z'), "missing closepath");
                assert_eq!(d.matches('L').count(), N_POINTS - 1);
            }
        }
    }
}
