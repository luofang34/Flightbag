//! Web Mercator tile projection, zoom simplification, and buffered `[-64, 4160]` clipping
//! for polygons and boundary lines.

use std::f64::consts::PI;

use flightbag_core::AirspacePolygon;
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::overlay::FloatOverlay;
use world_core::GeodeticPosition;

use crate::mvt::TilePolygon;

/// Standard MVT tile coordinate extent (`4096`).
pub const TILE_EXTENT: f64 = 4_096.0;

/// Guard band outside `[0, 4096]` so `maplibre-rs` distinguishes interior vertices from tile seams.
const CLIP_BUFFER: f64 = 64.0;
const CLIP_MIN: f64 = -CLIP_BUFFER;
const CLIP_MAX: f64 = TILE_EXTENT + CLIP_BUFFER;

const MAX_MERCATOR_LAT: f64 = 85.051_128_78;

/// Projects `(lon_deg, lat_deg)` into normalized `[0, 1]^2` Web Mercator coordinates.
pub fn lon_lat_to_mercator01(lon_deg: f64, lat_deg: f64) -> [f64; 2] {
    let x = (lon_deg + 180.0) / 360.0;
    let lat_rad = lat_deg
        .clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT)
        .to_radians();
    let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) * 0.5;
    [x, y]
}

/// Converts normalized `[0, 1]^2` Web Mercator coordinates into tile-local `[0, 4096]` space.
pub fn mercator01_to_tile(mx: f64, my: f64, zoom: u8, tile_x: u32, tile_y: u32) -> [f64; 2] {
    let n = f64::from(1_u32 << zoom);
    [
        (mx * n - f64::from(tile_x)) * TILE_EXTENT,
        (my * n - f64::from(tile_y)) * TILE_EXTENT,
    ]
}

/// Computes the inclusive tile range `(min_tx, min_ty, max_tx, max_ty)` touched by `merc_bbox` at `zoom`.
pub fn covered_tiles(
    min_mx: f64,
    min_my: f64,
    max_mx: f64,
    max_my: f64,
    zoom: u8,
) -> Option<(u32, u32, u32, u32)> {
    let n = 1_u32 << zoom;
    let nf = f64::from(n);
    let pad = CLIP_BUFFER / TILE_EXTENT;
    let x0 = ((min_mx * nf - pad).floor() as i64).clamp(0, i64::from(n) - 1) as u32;
    let x1 = ((max_mx * nf + pad).floor() as i64).clamp(0, i64::from(n) - 1) as u32;
    let y0 = ((min_my * nf - pad).floor() as i64).clamp(0, i64::from(n) - 1) as u32;
    let y1 = ((max_my * nf + pad).floor() as i64).clamp(0, i64::from(n) - 1) as u32;
    if x0 <= x1 && y0 <= y1 {
        Some((x0, y0, x1, y1))
    } else {
        None
    }
}

/// Pre-projected Mercator polygon (`[mx, my]` in `[0, 1]^2`) with bounding box.
#[derive(Clone, Debug)]
pub struct ProjectedPolygon {
    /// Minimum Mercator X.
    pub min_mx: f64,
    /// Minimum Mercator Y.
    pub min_my: f64,
    /// Maximum Mercator X.
    pub max_mx: f64,
    /// Maximum Mercator Y.
    pub max_my: f64,
    /// Exterior Mercator contour.
    pub exterior: Vec<[f64; 2]>,
    /// Interior hole contours.
    pub interiors: Vec<Vec<[f64; 2]>>,
}

/// Projects a WGS-84 [`AirspacePolygon`] into Web Mercator `[0, 1]^2`.
pub fn project_polygon(poly: &AirspacePolygon) -> Option<ProjectedPolygon> {
    let exterior = project_ring(&poly.exterior);
    let &[fx, fy] = exterior.first()?;
    if exterior.len() < 3 {
        return None;
    }
    let (mut min_mx, mut min_my, mut max_mx, mut max_my) = (fx, fy, fx, fy);
    for &[mx, my] in &exterior {
        min_mx = min_mx.min(mx);
        max_mx = max_mx.max(mx);
        min_my = min_my.min(my);
        max_my = max_my.max(my);
    }
    let interiors = poly
        .interiors
        .iter()
        .map(|h| project_ring(h))
        .filter(|h| h.len() >= 3)
        .collect();
    Some(ProjectedPolygon {
        min_mx,
        min_my,
        max_mx,
        max_my,
        exterior,
        interiors,
    })
}

/// Clips a slice of [`ProjectedPolygon`]s to `(zoom, tile_x, tile_y)`'s `[-64, 4160]` clip box,
/// returning integer MVT [`TilePolygon`]s and clipped boundary polylines.
pub fn clip_polygons_to_tile(
    polys: &[ProjectedPolygon],
    zoom: u8,
    tile_x: u32,
    tile_y: u32,
) -> (Vec<TilePolygon>, Vec<Vec<(i32, i32)>>) {
    let n = f64::from(1_u32 << zoom);
    let pad = CLIP_BUFFER / TILE_EXTENT;
    let t_min_mx = (f64::from(tile_x) - pad) / n;
    let t_max_mx = (f64::from(tile_x) + 1.0 + pad) / n;
    let t_min_my = (f64::from(tile_y) - pad) / n;
    let t_max_my = (f64::from(tile_y) + 1.0 + pad) / n;

    let clip_rect = vec![
        [CLIP_MIN, CLIP_MIN],
        [CLIP_MAX, CLIP_MIN],
        [CLIP_MAX, CLIP_MAX],
        [CLIP_MIN, CLIP_MAX],
    ];

    let mut subj_shapes: Vec<Vec<Vec<[f64; 2]>>> = Vec::new();
    let mut boundary_lines: Vec<Vec<(i32, i32)>> = Vec::new();

    for poly in polys {
        if poly.max_mx < t_min_mx
            || poly.min_mx > t_max_mx
            || poly.max_my < t_min_my
            || poly.min_my > t_max_my
        {
            continue;
        }
        let ext_tile =
            simplify_closed_ring(&to_tile_ring(&poly.exterior, zoom, tile_x, tile_y), 0.35);
        if ext_tile.len() < 3 {
            continue;
        }
        clip_closed_ring_as_lines(&ext_tile, &mut boundary_lines);

        let mut shape = Vec::with_capacity(1 + poly.interiors.len());
        shape.push(ext_tile);
        for hole in &poly.interiors {
            let hole_tile = simplify_closed_ring(&to_tile_ring(hole, zoom, tile_x, tile_y), 0.35);
            if hole_tile.len() >= 3 {
                clip_closed_ring_as_lines(&hole_tile, &mut boundary_lines);
                shape.push(hole_tile);
            }
        }
        subj_shapes.push(shape);
    }

    if subj_shapes.is_empty() {
        return (Vec::new(), boundary_lines);
    }

    let clipped = FloatOverlay::with_subj_and_clip(&subj_shapes, &clip_rect)
        .overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    let mut tile_polys = Vec::with_capacity(clipped.len());
    for shape in clipped {
        let Some((ext_f, holes_f)) = shape.split_first() else {
            continue;
        };
        let exterior = quantize_ring(ext_f);
        if exterior.len() < 3 {
            continue;
        }
        let interiors = holes_f
            .iter()
            .map(|h| quantize_ring(h))
            .filter(|h| h.len() >= 3)
            .collect();
        tile_polys.push(TilePolygon {
            exterior,
            interiors,
        });
    }

    (tile_polys, boundary_lines)
}

fn project_ring(pts: &[GeodeticPosition]) -> Vec<[f64; 2]> {
    pts.iter()
        .map(|p| lon_lat_to_mercator01(p.longitude_deg, p.latitude_deg))
        .collect()
}

fn to_tile_ring(merc_ring: &[[f64; 2]], zoom: u8, tx: u32, ty: u32) -> Vec<[f64; 2]> {
    merc_ring
        .iter()
        .map(|&[mx, my]| mercator01_to_tile(mx, my, zoom, tx, ty))
        .collect()
}

fn quantize_ring(ring: &[[f64; 2]]) -> Vec<(i32, i32)> {
    let mut out = Vec::with_capacity(ring.len());
    for &[x, y] in ring {
        let pt = (x.round() as i32, y.round() as i32);
        if out.last().copied() != Some(pt) {
            out.push(pt);
        }
    }
    if let (Some(&first), Some(&last)) = (out.first(), out.last())
        && out.len() > 3
        && first == last
    {
        out.pop();
    }
    out
}

fn simplify_closed_ring(ring: &[[f64; 2]], tol_units: f64) -> Vec<[f64; 2]> {
    if ring.len() <= 8 {
        return ring.to_vec();
    }
    let mut closed = ring.to_vec();
    if let Some(&first) = ring.first() {
        closed.push(first);
    }
    let simplified = rdp_polyline(&closed, tol_units * tol_units);
    let mut out = simplified;
    if out.len() > 3 {
        let _ = out.pop();
    }
    out
}

fn rdp_polyline(pts: &[[f64; 2]], eps_sq: f64) -> Vec<[f64; 2]> {
    if pts.len() <= 2 {
        return pts.to_vec();
    }
    let Some(&start) = pts.first() else {
        return Vec::new();
    };
    let Some(&end) = pts.last() else {
        return Vec::new();
    };

    let mut max_dist_sq = -1.0_f64;
    let mut split_idx = 0_usize;
    for (idx, &pt) in pts.iter().enumerate().skip(1) {
        if idx + 1 >= pts.len() {
            break;
        }
        let d2 = point_segment_dist_sq(pt, start, end);
        if d2 > max_dist_sq {
            max_dist_sq = d2;
            split_idx = idx;
        }
    }

    if max_dist_sq > eps_sq && split_idx > 0 {
        let left = pts
            .get(..=split_idx)
            .map_or_else(Vec::new, |s| rdp_polyline(s, eps_sq));
        let right = pts
            .get(split_idx..)
            .map_or_else(Vec::new, |s| rdp_polyline(s, eps_sq));
        let mut combined = left;
        let _ = combined.pop();
        combined.extend(right);
        combined
    } else {
        vec![start, end]
    }
}

fn point_segment_dist_sq(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let [px, py] = p;
    let [ax, ay] = a;
    let [bx, by] = b;
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    if len_sq <= 1e-18 {
        let ex = px - ax;
        let ey = py - ay;
        return ex * ex + ey * ey;
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    let ex = px - cx;
    let ey = py - cy;
    ex * ex + ey * ey
}

/// Clips a closed ring in tile coordinates against `[CLIP_MIN, CLIP_MAX]^2` into quantized polylines.
fn clip_closed_ring_as_lines(ring: &[[f64; 2]], out: &mut Vec<Vec<(i32, i32)>>) {
    let Some(&first) = ring.first() else {
        return;
    };
    let mut ring_runs: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut cur_run: Vec<(i32, i32)> = Vec::new();
    let mut prev = first;
    for &next in ring.iter().skip(1).chain(std::iter::once(&first)) {
        if let Some(([x0, y0], [x1, y1])) = liang_barsky_clip(prev, next) {
            let p0 = (x0.round() as i32, y0.round() as i32);
            let p1 = (x1.round() as i32, y1.round() as i32);
            if cur_run.last().copied() != Some(p0) {
                if cur_run.len() >= 2 {
                    ring_runs.push(std::mem::take(&mut cur_run));
                } else {
                    cur_run.clear();
                }
                cur_run.push(p0);
            }
            if cur_run.last().copied() != Some(p1) {
                cur_run.push(p1);
            }
        } else if cur_run.len() >= 2 {
            ring_runs.push(std::mem::take(&mut cur_run));
        } else {
            cur_run.clear();
        }
        prev = next;
    }
    if cur_run.len() >= 2 {
        ring_runs.push(cur_run);
    }

    if ring_runs.len() >= 2
        && let (Some(first_pt), Some(last_pt)) = (
            ring_runs.first().and_then(|r| r.first()).copied(),
            ring_runs.last().and_then(|r| r.last()).copied(),
        )
        && first_pt == last_pt
    {
        let head = ring_runs.remove(0);
        if let Some(tail) = ring_runs.last_mut()
            && let Some((_, rest)) = head.split_first()
        {
            tail.extend_from_slice(rest);
        }
    }

    out.extend(ring_runs);
}

fn liang_barsky_clip(a: [f64; 2], b: [f64; 2]) -> Option<([f64; 2], [f64; 2])> {
    let [x0, y0] = a;
    let [x1, y1] = b;
    let dx = x1 - x0;
    let dy = y1 - y0;
    let mut t0 = 0.0_f64;
    let mut t1 = 1.0_f64;

    for (p, q) in [
        (-dx, x0 - CLIP_MIN),
        (dx, CLIP_MAX - x0),
        (-dy, y0 - CLIP_MIN),
        (dy, CLIP_MAX - y0),
    ] {
        if p.abs() <= 1e-12 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                if r > t0 {
                    t0 = r;
                }
            } else {
                if r < t0 {
                    return None;
                }
                if r < t1 {
                    t1 = r;
                }
            }
        }
    }

    Some(([x0 + t0 * dx, y0 + t0 * dy], [x0 + t1 * dx, y0 + t1 * dy]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liang_barsky_and_ring_clipper_preserve_exact_endpoints_and_stitch_wraparound() {
        let inside = [
            [500.0, 600.0],
            [1500.0, 900.0],
            [1200.0, 1800.0],
            [400.0, 1400.0],
        ];
        let mut runs = Vec::new();
        clip_closed_ring_as_lines(&inside, &mut runs);
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs.first().map(Vec::as_slice),
            Some(
                [
                    (500, 600),
                    (1500, 900),
                    (1200, 1800),
                    (400, 1400),
                    (500, 600)
                ]
                .as_slice()
            )
        );

        // Ring starts inside the clip box, exits on `+X`, and re-enters before `first`:
        // wraparound stitching merges tail + head into one continuous polyline.
        let crossing = [
            [3500.0, 1000.0],
            [5000.0, 1500.0],
            [5000.0, 2500.0],
            [3500.0, 3000.0],
        ];
        let mut clipped_runs = Vec::new();
        clip_closed_ring_as_lines(&crossing, &mut clipped_runs);
        assert_eq!(clipped_runs.len(), 1);
        if let Some(run) = clipped_runs.first() {
            assert_eq!(run.len(), 4);
            assert!(run.iter().all(|&(x, y)| {
                f64::from(x) >= CLIP_MIN
                    && f64::from(x) <= CLIP_MAX
                    && f64::from(y) >= CLIP_MIN
                    && f64::from(y) <= CLIP_MAX
            }));
        }
    }
}
