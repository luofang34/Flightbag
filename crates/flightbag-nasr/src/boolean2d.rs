//! 2D boolean polygon operations (`union`, `difference`, `intersection`, morphological gap
//! closing, sliver pruning, and ring normalization) backed by `i_overlay` over WGS-84 `(lon, lat)`.

use std::f64::consts::PI;

use flightbag_core::AirspacePolygon;
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::overlay::FloatOverlay;
use i_overlay::mesh::outline::offset::OutlineOffset;
use i_overlay::mesh::style::{LineJoin, OutlineStyle};
use world_core::GeodeticPosition;

/// Minimum retained polygon area in square degrees (`~10 m x 10 m` sliver threshold) after boolean cuts.
const MIN_POLYGON_AREA_DEG2: f64 = 1e-9;

/// Normalizes a raw collection of rings (`[lon, lat]`) into clean, non-self-intersecting
/// [`AirspacePolygon`] components using `EvenOdd` winding classification.
pub fn normalize_raw_rings(rings: &[Vec<[f64; 2]>]) -> Vec<AirspacePolygon> {
    let filtered: Vec<Vec<[f64; 2]>> = rings
        .iter()
        .filter_map(|ring| clean_contour(ring))
        .collect();
    if filtered.is_empty() {
        return Vec::new();
    }
    let empty: Vec<Vec<[f64; 2]>> = Vec::new();
    let shapes = FloatOverlay::with_subj_and_clip(&filtered, &empty)
        .overlay(OverlayRule::Subject, FillRule::EvenOdd);
    shapes_to_polygons(shapes)
}

/// Computes the 2D boolean union `a ∪ b`.
pub fn union_polygons(a: &[AirspacePolygon], b: &[AirspacePolygon]) -> Vec<AirspacePolygon> {
    if a.is_empty() {
        return b.to_vec();
    }
    if b.is_empty() {
        return a.to_vec();
    }
    let subj = polygons_to_shapes(a);
    let clip = polygons_to_shapes(b);
    let shapes = FloatOverlay::with_subj_and_clip(&subj, &clip)
        .overlay(OverlayRule::Union, FillRule::EvenOdd);
    shapes_to_polygons(shapes)
}

/// Computes the 2D boolean difference `subject \ clip`.
pub fn subtract_polygons(
    subject: &[AirspacePolygon],
    clip: &[AirspacePolygon],
) -> Vec<AirspacePolygon> {
    if subject.is_empty() || clip.is_empty() {
        return subject.to_vec();
    }
    let subj = polygons_to_shapes(subject);
    let clp = polygons_to_shapes(clip);
    let shapes = FloatOverlay::with_subj_and_clip(&subj, &clp)
        .overlay(OverlayRule::Difference, FillRule::EvenOdd);
    shapes_to_polygons(shapes)
}

/// Computes the 2D boolean intersection `subject ∩ clip`.
pub fn intersect_polygons(
    subject: &[AirspacePolygon],
    clip: &[AirspacePolygon],
) -> Vec<AirspacePolygon> {
    if subject.is_empty() || clip.is_empty() {
        return Vec::new();
    }
    let subj = polygons_to_shapes(subject);
    let clp = polygons_to_shapes(clip);
    let shapes = FloatOverlay::with_subj_and_clip(&subj, &clp)
        .overlay(OverlayRule::Intersect, FillRule::EvenOdd);
    shapes_to_polygons(shapes)
}

/// Computes the net planar `(lon, lat)` area in square degrees (`exterior - interiors`) of `polygons`.
pub fn polygons_area_deg2(polygons: &[AirspacePolygon]) -> f64 {
    let mut total = 0.0;
    for poly in polygons {
        let ext: Vec<[f64; 2]> = poly
            .exterior
            .iter()
            .map(|p| [p.longitude_deg, p.latitude_deg])
            .collect();
        let mut area = contour_abs_area(&ext);
        for hole in &poly.interiors {
            let h: Vec<[f64; 2]> = hole
                .iter()
                .map(|p| [p.longitude_deg, p.latitude_deg])
                .collect();
            area -= contour_abs_area(&h);
        }
        total += area.max(0.0);
    }
    total
}

/// Performs morphological closing `(polygons ⊕ eps_deg) ⊖ eps_deg` with miter joins so
/// sub-`2 * eps_deg` digitization micro-gaps between abutting same-class sectors weld shut
/// without shifting exterior boundaries or real VFR corridors.
pub fn close_micro_gaps(polygons: &[AirspacePolygon], eps_deg: f64) -> Vec<AirspacePolygon> {
    if polygons.is_empty() || eps_deg <= 0.0 {
        return polygons.to_vec();
    }
    let shapes = polygons_to_shapes(polygons);
    if shapes.is_empty() {
        return Vec::new();
    }
    let expand = OutlineStyle::new(eps_deg).line_join(LineJoin::Miter(0.35 * PI));
    let shrink = OutlineStyle::new(-eps_deg).line_join(LineJoin::Miter(0.35 * PI));
    let dilated = shapes.outline(&expand);
    if dilated.is_empty() {
        return polygons.to_vec();
    }
    let closed = dilated.outline(&shrink);
    if closed.is_empty() {
        return polygons.to_vec();
    }
    shapes_to_polygons(closed)
}

/// Removes thin digitization seam slivers and T-junction whiskers narrower than
/// `2 * min_half_width_deg` while preserving exact vertex coordinates along all real edges.
pub fn prune_thin_slivers(
    polygons: &[AirspacePolygon],
    min_half_width_deg: f64,
) -> Vec<AirspacePolygon> {
    if polygons.is_empty() || min_half_width_deg <= 0.0 {
        return polygons.to_vec();
    }
    let shapes = polygons_to_shapes(polygons);
    if shapes.is_empty() {
        return Vec::new();
    }
    let erode_style = OutlineStyle::new(-min_half_width_deg).line_join(LineJoin::Miter(0.35 * PI));
    let core = shapes.outline(&erode_style);
    if core.is_empty() {
        return Vec::new();
    }
    let regrow_style =
        OutlineStyle::new(min_half_width_deg * 1.45).line_join(LineJoin::Miter(0.35 * PI));
    let mask_shapes = core.outline(&regrow_style);
    if mask_shapes.is_empty() {
        return Vec::new();
    }
    let clipped = FloatOverlay::with_subj_and_clip(&shapes, &mask_shapes)
        .overlay(OverlayRule::Intersect, FillRule::EvenOdd);
    shapes_to_polygons(clipped)
}

/// Dilates `polygons` outward by `offset_deg` (`> 0.0`) with miter joins.
pub fn dilate_polygons(polygons: &[AirspacePolygon], offset_deg: f64) -> Vec<AirspacePolygon> {
    if polygons.is_empty() || offset_deg <= 0.0 {
        return polygons.to_vec();
    }
    let shapes = polygons_to_shapes(polygons);
    if shapes.is_empty() {
        return Vec::new();
    }
    let style = OutlineStyle::new(offset_deg).line_join(LineJoin::Miter(0.12 * PI));
    shapes_to_polygons(shapes.outline(&style))
}

/// Erodes `polygons` inward by `offset_deg` (`> 0.0`) with miter joins.
pub fn erode_polygons(polygons: &[AirspacePolygon], offset_deg: f64) -> Vec<AirspacePolygon> {
    if polygons.is_empty() || offset_deg <= 0.0 {
        return polygons.to_vec();
    }
    let shapes = polygons_to_shapes(polygons);
    if shapes.is_empty() {
        return Vec::new();
    }
    let style = OutlineStyle::new(-offset_deg).line_join(LineJoin::Miter(0.12 * PI));
    shapes_to_polygons(shapes.outline(&style))
}

/// Computes the axis-aligned bounding box `(min_lon, min_lat, max_lon, max_lat)` across `polygons`.
pub fn polygons_bbox(polygons: &[AirspacePolygon]) -> Option<crate::GeoBbox> {
    let mut agg: Option<crate::GeoBbox> = None;
    for poly in polygons {
        if let Some(b) = crate::GeoBbox::from_positions(&poly.exterior) {
            agg = Some(match agg {
                None => b,
                Some(cur) => crate::GeoBbox {
                    min_lon: cur.min_lon.min(b.min_lon),
                    min_lat: cur.min_lat.min(b.min_lat),
                    max_lon: cur.max_lon.max(b.max_lon),
                    max_lat: cur.max_lat.max(b.max_lat),
                },
            });
        }
    }
    agg
}

fn clean_contour(ring: &[[f64; 2]]) -> Option<Vec<[f64; 2]>> {
    let mut out: Vec<[f64; 2]> = Vec::with_capacity(ring.len());
    for &[lon, lat] in ring {
        if !lon.is_finite() || !lat.is_finite() {
            continue;
        }
        let lon_clamped = lon.clamp(-180.0, 180.0);
        let lat_clamped = lat.clamp(-89.999_999, 89.999_999);
        if let Some(&[prev_lon, prev_lat]) = out.last()
            && (prev_lon - lon_clamped).abs() < 1e-10
            && (prev_lat - lat_clamped).abs() < 1e-10
        {
            continue;
        }
        out.push([lon_clamped, lat_clamped]);
    }
    if let (Some(&[f_lon, f_lat]), Some(&[l_lon, l_lat])) = (out.first(), out.last())
        && out.len() > 3
        && (f_lon - l_lon).abs() < 1e-10
        && (f_lat - l_lat).abs() < 1e-10
    {
        out.pop();
    }
    if out.len() >= 3 { Some(out) } else { None }
}

fn polygons_to_shapes(polygons: &[AirspacePolygon]) -> Vec<Vec<Vec<[f64; 2]>>> {
    let mut shapes = Vec::with_capacity(polygons.len());
    for poly in polygons {
        let mut shape = Vec::with_capacity(1 + poly.interiors.len());
        let ext: Vec<[f64; 2]> = poly
            .exterior
            .iter()
            .map(|p| [p.longitude_deg, p.latitude_deg])
            .collect();
        if ext.len() < 3 {
            continue;
        }
        shape.push(ext);
        for hole in &poly.interiors {
            let h: Vec<[f64; 2]> = hole
                .iter()
                .map(|p| [p.longitude_deg, p.latitude_deg])
                .collect();
            if h.len() >= 3 {
                shape.push(h);
            }
        }
        shapes.push(shape);
    }
    shapes
}

fn shapes_to_polygons(shapes: Vec<Vec<Vec<[f64; 2]>>>) -> Vec<AirspacePolygon> {
    let mut result = Vec::with_capacity(shapes.len());
    for shape in shapes {
        let Some((ext_raw, holes_raw)) = shape.split_first() else {
            continue;
        };
        if ext_raw.len() < 3 || contour_abs_area(ext_raw) < MIN_POLYGON_AREA_DEG2 {
            continue;
        }
        let exterior = ext_raw
            .iter()
            .map(|&[lon, lat]| {
                GeodeticPosition::wgs84(
                    lat.clamp(-89.999_999, 89.999_999),
                    lon.clamp(-180.0, 180.0),
                    0.0,
                )
            })
            .collect();
        let interiors = holes_raw
            .iter()
            .filter(|h| h.len() >= 3 && contour_abs_area(h) >= MIN_POLYGON_AREA_DEG2)
            .map(|h| {
                h.iter()
                    .map(|&[lon, lat]| {
                        GeodeticPosition::wgs84(
                            lat.clamp(-89.999_999, 89.999_999),
                            lon.clamp(-180.0, 180.0),
                            0.0,
                        )
                    })
                    .collect()
            })
            .collect();
        result.push(AirspacePolygon {
            exterior,
            interiors,
        });
    }
    result
}

fn contour_abs_area(contour: &[[f64; 2]]) -> f64 {
    let Some(&last_pt) = contour.last() else {
        return 0.0;
    };
    let [mut prev_x, mut prev_y] = last_pt;
    let mut twice_area = 0.0;
    for &[cur_x, cur_y] in contour {
        twice_area += prev_x * cur_y - cur_x * prev_y;
        prev_x = cur_x;
        prev_y = cur_y;
    }
    0.5 * twice_area.abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closes_digitization_micro_gaps_without_altering_outer_bounds() {
        // Two sectors separated by a 1.6e-4 deg (~16 m) NASR digitization crack.
        let left = normalize_raw_rings(&[vec![
            [-117.95, 33.60],
            [-117.85, 33.60],
            [-117.85, 33.70],
            [-117.95, 33.70],
        ]]);
        let right = normalize_raw_rings(&[vec![
            [-117.849_84, 33.60],
            [-117.75, 33.60],
            [-117.75, 33.70],
            [-117.849_84, 33.70],
        ]]);
        let raw_union = union_polygons(&left, &right);
        assert_eq!(raw_union.len(), 2);

        let welded = close_micro_gaps(&raw_union, 2.6e-4);
        assert_eq!(welded.len(), 1);
        assert!(welded.first().is_some_and(|p| p.interiors.is_empty()));
        let bb = polygons_bbox(&welded);
        assert!(bb.is_some_and(|b| {
            (b.min_lon - (-117.95)).abs() < 1e-5
                && (b.max_lon - (-117.75)).abs() < 1e-5
                && (b.min_lat - 33.60).abs() < 1e-5
                && (b.max_lat - 33.70).abs() < 1e-5
        }));
    }

    #[test]
    fn prunes_thin_whiskers_while_preserving_real_shelf_edges() {
        let shelf = normalize_raw_rings(&[vec![
            [-74.10, 40.70],
            [-74.00, 40.70],
            [-74.00, 40.749_8],
            [-73.95, 40.749_8],
            [-73.95, 40.750_0],
            [-74.00, 40.750_0],
            [-74.00, 40.80],
            [-74.10, 40.80],
        ]]);
        let cleaned = prune_thin_slivers(&shelf, 2.6e-4);
        assert_eq!(cleaned.len(), 1);
        let bb = polygons_bbox(&cleaned);
        assert!(bb.is_some_and(|b| { (b.min_lon - (-74.10)).abs() < 1e-6 && b.max_lon < -73.999 }));
    }
}
