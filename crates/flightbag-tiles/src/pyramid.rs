//! Zoom-progressive MVT 2.1 tile pyramid builder (`z2..=10`) with watertight 3D manifold
//! slicing for multi-shelf controlled and special-use airspaces.

use std::collections::BTreeMap;

use flightbag_core::{
    AirspaceClassification, AirspacePolygon, AirspaceVolume, AltitudeDatum,
    ControlledAirspaceClass, ControlledLocalType, METRES_PER_FOOT, SuaKind,
};
use flightbag_nasr::boolean2d::{subtract_polygons, union_polygons};

use crate::clip::{
    ProjectedPolygon, TILE_EXTENT, clip_polygons_to_tile, covered_tiles, project_polygon,
};
use crate::mvt::{AttributeValue, EncodedFeature, FeatureGeometry, MvtLayerInput, encode_tile};

/// Lowest zoom level stored in `airspace.pmtiles`.
pub const MIN_ZOOM: u8 = 2;

/// Highest zoom level stored in `airspace.pmtiles` (overscaled seamlessly at `z11+`).
pub const MAX_ZOOM: u8 = 10;

/// Base of Class A airspace (`18,000 ft MSL`), used as the VFR sectional display cap for
/// stratospheric Special Use Airspaces (`FL180+`) while preserving legal `upper_ft`/`upper_m`.
const VFR_SUA_DISPLAY_CAP_FT: i32 = 18_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifoldPart {
    Full,
    Walls { emit_surface_rim: bool },
    Floor,
    Roof,
}

impl ManifoldPart {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Walls { .. } => "walls",
            Self::Floor => "floor",
            Self::Roof => "roof",
        }
    }
}

struct PreparedPrimitive<'a> {
    id: u64,
    template: &'a AirspaceVolume,
    include_sector: bool,
    min_zoom: u8,
    is_enroute_blanket: bool,
    part: ManifoldPart,
    lower_ft: i32,
    upper_ft: i32,
    lower_m: f64,
    upper_m: f64,
    display_upper_m: f64,
    lower_datum: &'static str,
    upper_datum: &'static str,
    display_upper_datum: &'static str,
    projected: Vec<ProjectedPolygon>,
    min_mx: f64,
    min_my: f64,
    max_mx: f64,
    max_my: f64,
}

/// Builds the complete `(zoom, x, y) -> raw .mvt bytes` pyramid from `volumes`.
pub fn build_mvt_pyramid(volumes: &[AirspaceVolume]) -> BTreeMap<(u8, u32, u32), Vec<u8>> {
    let primitives = prepare_primitives(volumes);

    let mut tile_buckets: BTreeMap<(u8, u32, u32), Vec<usize>> = BTreeMap::new();
    for (p_idx, prim) in primitives.iter().enumerate() {
        for zoom in prim.min_zoom..=MAX_ZOOM {
            let Some((x0, y0, x1, y1)) =
                covered_tiles(prim.min_mx, prim.min_my, prim.max_mx, prim.max_my, zoom)
            else {
                continue;
            };
            for tx in x0..=x1 {
                for ty in y0..=y1 {
                    tile_buckets.entry((zoom, tx, ty)).or_default().push(p_idx);
                }
            }
        }
    }

    let mut encoded_tiles = BTreeMap::new();
    for ((zoom, tx, ty), indices) in tile_buckets {
        let mut poly_features = Vec::with_capacity(indices.len());
        let mut line_features = Vec::with_capacity(indices.len());

        for p_idx in indices {
            let Some(prim) = primitives.get(p_idx) else {
                continue;
            };
            let (tile_polys, tile_lines) = clip_polygons_to_tile(&prim.projected, zoom, tx, ty);
            if tile_polys.is_empty() && tile_lines.is_empty() {
                continue;
            }
            let base_attrs = build_poly_attributes(prim);
            if !tile_polys.is_empty() {
                poly_features.push(EncodedFeature {
                    id: prim.id,
                    attributes: base_attrs.clone(),
                    geometry: FeatureGeometry::Polygons(tile_polys),
                });
            }
            if !tile_lines.is_empty() {
                match prim.part {
                    ManifoldPart::Walls {
                        emit_surface_rim: false,
                    } => {}
                    ManifoldPart::Walls {
                        emit_surface_rim: true,
                    }
                    | ManifoldPart::Floor => {
                        line_features.push(EncodedFeature {
                            id: prim.id,
                            attributes: with_rim_attributes(
                                &base_attrs,
                                "floor",
                                prim.lower_m,
                                prim.lower_datum,
                            ),
                            geometry: FeatureGeometry::Lines(tile_lines),
                        });
                    }
                    ManifoldPart::Roof => {
                        line_features.push(EncodedFeature {
                            id: prim.id,
                            attributes: with_rim_attributes(
                                &base_attrs,
                                "ceiling",
                                prim.display_upper_m,
                                prim.display_upper_datum,
                            ),
                            geometry: FeatureGeometry::Lines(tile_lines),
                        });
                    }
                    ManifoldPart::Full => {
                        if prim.lower_ft > 0 && !prim.is_enroute_blanket {
                            line_features.push(EncodedFeature {
                                id: prim.id.saturating_mul(2),
                                attributes: with_rim_attributes(
                                    &base_attrs,
                                    "floor",
                                    prim.lower_m,
                                    prim.lower_datum,
                                ),
                                geometry: FeatureGeometry::Lines(tile_lines.clone()),
                            });
                        }
                        line_features.push(EncodedFeature {
                            id: prim.id.saturating_mul(2).saturating_add(1),
                            attributes: with_rim_attributes(
                                &base_attrs,
                                "ceiling",
                                prim.display_upper_m,
                                prim.display_upper_datum,
                            ),
                            geometry: FeatureGeometry::Lines(tile_lines),
                        });
                    }
                }
            }
        }

        if poly_features.is_empty() && line_features.is_empty() {
            continue;
        }

        let mvt_bytes = encode_tile(&[
            MvtLayerInput {
                name: "airspace".to_owned(),
                extent: TILE_EXTENT as u32,
                features: poly_features,
            },
            MvtLayerInput {
                name: "airspace_boundary".to_owned(),
                extent: TILE_EXTENT as u32,
                features: line_features,
            },
        ]);
        if !mvt_bytes.is_empty() {
            encoded_tiles.insert((zoom, tx, ty), mvt_bytes);
        }
    }

    encoded_tiles
}

fn prepare_primitives(volumes: &[AirspaceVolume]) -> Vec<PreparedPrimitive<'_>> {
    let mut groups: BTreeMap<&str, Vec<&AirspaceVolume>> = BTreeMap::new();
    for vol in volumes {
        if zoom_eligibility(vol).is_some() {
            groups
                .entry(vol.airspace_key.as_str())
                .or_default()
                .push(vol);
        }
    }

    let mut out = Vec::with_capacity(volumes.len());
    let mut next_id: u64 = 1;

    for group in groups.into_values() {
        if can_manifold_slice(&group) {
            slice_manifold_group(&group, &mut next_id, &mut out);
        } else {
            for vol in group {
                let Some((min_zoom, is_enroute_blanket)) = zoom_eligibility(vol) else {
                    continue;
                };
                let (display_upper_m, display_upper_datum, _eff_ft) = compute_display_ceiling(vol);
                push_projected_primitive(
                    &mut out,
                    &mut next_id,
                    PrimitiveSpec {
                        template: vol,
                        include_sector: true,
                        min_zoom,
                        is_enroute_blanket,
                        part: ManifoldPart::Full,
                        lower_ft: vol.lower.value_ft,
                        upper_ft: vol.upper.value_ft,
                        lower_m: vol.lower.metres(),
                        upper_m: vol.upper.metres(),
                        display_upper_m,
                        lower_datum: vol.lower.datum.renderer_datum(),
                        upper_datum: vol.upper.datum.renderer_datum(),
                        display_upper_datum,
                    },
                    &vol.polygons,
                );
            }
        }
    }

    out
}

struct PrimitiveSpec<'a> {
    template: &'a AirspaceVolume,
    include_sector: bool,
    min_zoom: u8,
    is_enroute_blanket: bool,
    part: ManifoldPart,
    lower_ft: i32,
    upper_ft: i32,
    lower_m: f64,
    upper_m: f64,
    display_upper_m: f64,
    lower_datum: &'static str,
    upper_datum: &'static str,
    display_upper_datum: &'static str,
}

fn push_projected_primitive<'a>(
    out: &mut Vec<PreparedPrimitive<'a>>,
    next_id: &mut u64,
    spec: PrimitiveSpec<'a>,
    polygons: &[AirspacePolygon],
) {
    let proj: Vec<ProjectedPolygon> = polygons.iter().filter_map(project_polygon).collect();
    let Some(first) = proj.first() else {
        return;
    };
    let (mut min_mx, mut min_my, mut max_mx, mut max_my) =
        (first.min_mx, first.min_my, first.max_mx, first.max_my);
    for p in &proj {
        min_mx = min_mx.min(p.min_mx);
        min_my = min_my.min(p.min_my);
        max_mx = max_mx.max(p.max_mx);
        max_my = max_my.max(p.max_my);
    }
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    out.push(PreparedPrimitive {
        id,
        template: spec.template,
        include_sector: spec.include_sector,
        min_zoom: spec.min_zoom,
        is_enroute_blanket: spec.is_enroute_blanket,
        part: spec.part,
        lower_ft: spec.lower_ft,
        upper_ft: spec.upper_ft,
        lower_m: spec.lower_m,
        upper_m: spec.upper_m,
        display_upper_m: spec.display_upper_m,
        lower_datum: spec.lower_datum,
        upper_datum: spec.upper_datum,
        display_upper_datum: spec.display_upper_datum,
        projected: proj,
        min_mx,
        min_my,
        max_mx,
        max_my,
    });
}

fn is_msl_like(datum: AltitudeDatum) -> bool {
    matches!(datum, AltitudeDatum::Msl | AltitudeDatum::FlightLevel)
}

fn can_manifold_slice(group: &[&AirspaceVolume]) -> bool {
    let Some(&first) = group.first() else {
        return false;
    };
    if group.len() < 2 {
        return false;
    }
    let Some(first_zoom) = zoom_eligibility(first) else {
        return false;
    };
    group.iter().all(|vol| {
        let (_, disp_datum, eff_upper_ft) = compute_display_ceiling(vol);
        zoom_eligibility(vol) == Some(first_zoom)
            && disp_datum == "msl"
            && eff_upper_ft > vol.lower.value_ft
            && (vol.lower.value_ft == 0 || is_msl_like(vol.lower.datum))
    })
}

/// Slices a multi-shelf complex (`Class B`, `Class C`, stepped `Class D`, or multi-slab `SUA`)
/// across all unique altitude planes `z_0 < z_1 < ... < z_m` into watertight exterior walls,
/// exposed underside terraces (`part = "floor"`), and exposed top ceilings (`part = "roof"`).
fn slice_manifold_group<'a>(
    group: &[&'a AirspaceVolume],
    next_id: &mut u64,
    out: &mut Vec<PreparedPrimitive<'a>>,
) {
    let Some(&rep) = group.first() else {
        return;
    };
    let Some((min_zoom, is_enroute_blanket)) = zoom_eligibility(rep) else {
        return;
    };

    let zero_is_sfc = group.iter().any(|v| {
        v.lower.value_ft == 0
            && matches!(v.lower.datum, AltitudeDatum::Surface | AltitudeDatum::Agl)
    });
    let max_legal_upper_ft = group
        .iter()
        .map(|v| v.upper.value_ft)
        .max()
        .unwrap_or(rep.upper.value_ft);
    let is_wedding_cake_class = matches!(
        rep.classification,
        AirspaceClassification::Controlled {
            class: ControlledAirspaceClass::ClassB | ControlledAirspaceClass::ClassC,
            ..
        }
    );

    let mut z_planes = Vec::with_capacity(group.len() * 2);
    for vol in group {
        let (_, _, eff_upper_ft) = compute_display_ceiling(vol);
        z_planes.push(vol.lower.value_ft);
        z_planes.push(eff_upper_ft);
    }
    z_planes.sort_unstable();
    z_planes.dedup();
    if z_planes.len() < 2 {
        return;
    }

    let slab_count = z_planes.len().saturating_sub(1);
    let mut unions: Vec<Vec<AirspacePolygon>> = Vec::with_capacity(slab_count);
    for window in z_planes.windows(2) {
        let (Some(&z_low), Some(&z_high)) = (window.first(), window.get(1)) else {
            continue;
        };
        let mut slab_union: Vec<AirspacePolygon> = Vec::new();
        for vol in group {
            let (_, _, eff_upper_ft) = compute_display_ceiling(vol);
            if vol.lower.value_ft <= z_low && eff_upper_ft >= z_high {
                slab_union = union_polygons(&slab_union, &vol.polygons);
            }
        }
        unions.push(slab_union);
    }

    let empty_polys: Vec<AirspacePolygon> = Vec::new();
    for slab_idx in 0..unions.len() {
        let (Some(&z_low), Some(&z_high), Some(u_cur)) = (
            z_planes.get(slab_idx),
            z_planes.get(slab_idx + 1),
            unions.get(slab_idx),
        ) else {
            continue;
        };
        if u_cur.is_empty() || z_high <= z_low {
            continue;
        }

        let lower_datum = if z_low == 0 && zero_is_sfc {
            "agl"
        } else {
            "msl"
        };
        let lower_m = f64::from(z_low) * METRES_PER_FOOT;
        let display_upper_m = f64::from(z_high) * METRES_PER_FOOT;
        let legal_upper_ft = if matches!(rep.classification, AirspaceClassification::SpecialUse(_))
            && z_high == VFR_SUA_DISPLAY_CAP_FT
            && max_legal_upper_ft > VFR_SUA_DISPLAY_CAP_FT
        {
            max_legal_upper_ft
        } else {
            z_high
        };
        let legal_upper_m = f64::from(legal_upper_ft) * METRES_PER_FOOT;

        // 1. Exterior vertical walls across [z_low, z_high] (plus ground rim for SFC Class B/C core).
        push_projected_primitive(
            out,
            next_id,
            PrimitiveSpec {
                template: rep,
                include_sector: false,
                min_zoom,
                is_enroute_blanket,
                part: ManifoldPart::Walls {
                    emit_surface_rim: z_low == 0 && is_wedding_cake_class,
                },
                lower_ft: z_low,
                upper_ft: legal_upper_ft,
                lower_m,
                upper_m: legal_upper_m,
                display_upper_m,
                lower_datum,
                upper_datum: "msl",
                display_upper_datum: "msl",
            },
            u_cur,
        );

        // 2. Exposed floating underside terrace at z_low (`U_cur \ U_prev`).
        if z_low > 0 {
            let u_prev = slab_idx
                .checked_sub(1)
                .and_then(|prev| unions.get(prev))
                .unwrap_or(&empty_polys);
            let floor_terrace = subtract_polygons(u_cur, u_prev);
            if !floor_terrace.is_empty() {
                push_projected_primitive(
                    out,
                    next_id,
                    PrimitiveSpec {
                        template: rep,
                        include_sector: false,
                        min_zoom,
                        is_enroute_blanket,
                        part: ManifoldPart::Floor,
                        lower_ft: z_low,
                        upper_ft: legal_upper_ft,
                        lower_m,
                        upper_m: legal_upper_m,
                        display_upper_m,
                        lower_datum: "msl",
                        upper_datum: "msl",
                        display_upper_datum: "msl",
                    },
                    &floor_terrace,
                );
            }
        }

        // 3. Exposed top ceiling at z_high (`U_cur \ U_next`).
        let u_next = unions.get(slab_idx + 1).unwrap_or(&empty_polys);
        let roof_cap = subtract_polygons(u_cur, u_next);
        if !roof_cap.is_empty() {
            push_projected_primitive(
                out,
                next_id,
                PrimitiveSpec {
                    template: rep,
                    include_sector: false,
                    min_zoom,
                    is_enroute_blanket,
                    part: ManifoldPart::Roof,
                    lower_ft: z_low,
                    upper_ft: legal_upper_ft,
                    lower_m,
                    upper_m: legal_upper_m,
                    display_upper_m,
                    lower_datum,
                    upper_datum: "msl",
                    display_upper_datum: "msl",
                },
                &roof_cap,
            );
        }
    }
}

/// Determines the minimum zoom at which `vol` enters the vector pyramid, and whether it is a
/// continental/enroute Class E blanket (`1,200 ft+` `CLASS_E5`/`E6`/`E7`).
fn zoom_eligibility(vol: &AirspaceVolume) -> Option<(u8, bool)> {
    match vol.classification {
        AirspaceClassification::Controlled { class, local_type } => match class {
            ControlledAirspaceClass::ClassB => Some((2, false)),
            ControlledAirspaceClass::ClassC => Some((3, false)),
            ControlledAirspaceClass::ClassD => Some((5, false)),
            ControlledAirspaceClass::ClassE => match local_type {
                ControlledLocalType::ClassE2Surface
                | ControlledLocalType::ClassE3Extension
                | ControlledLocalType::ClassE4Extension => Some((6, false)),
                ControlledLocalType::ClassE5Transition => {
                    if vol.lower.value_ft < 1_200 {
                        Some((7, false))
                    } else if approx_footprint_deg2(vol) <= 4.0 {
                        Some((7, true))
                    } else {
                        None
                    }
                }
                ControlledLocalType::ClassE6Enroute | ControlledLocalType::ClassEOther => {
                    if approx_footprint_deg2(vol) <= 12.0 {
                        Some((6, true))
                    } else {
                        None
                    }
                }
                ControlledLocalType::ClassB
                | ControlledLocalType::ClassC
                | ControlledLocalType::ClassD => Some((5, false)),
            },
        },
        AirspaceClassification::SpecialUse(sua) => match sua {
            SuaKind::Prohibited
            | SuaKind::Restricted
            | SuaKind::Warning
            | SuaKind::Moa
            | SuaKind::Alert => Some((2, false)),
            SuaKind::NationalSecurityArea => Some((5, false)),
        },
    }
}

fn approx_footprint_deg2(vol: &AirspaceVolume) -> f64 {
    let mut total = 0.0;
    for poly in &vol.polygons {
        if let Some(b) = flightbag_nasr::GeoBbox::from_positions(&poly.exterior) {
            total += (b.max_lon - b.min_lon).max(0.0) * (b.max_lat - b.min_lat).max(0.0);
        }
    }
    total
}

/// Computes `(display_upper_m, display_upper_datum, effective_upper_ft)` for VFR 3D portrayal while
/// preserving legal `upper_m` / `upper_ft` attributes on every feature:
/// - `700 ft AGL` `CLASS_E5` transition rings render as the `700..1,200 ft AGL` transition step.
/// - `0 SFC` `CLASS_E2`/`E3`/`E4` with `18,000 ft MSL` (`-9998` / `AA`) render to `700 ft AGL`.
/// - Stratospheric `SpecialUse` (`> 18,000 ft MSL`, e.g. `FL800` offshore Warning Areas) cap VFR
///   display height at `18,000 ft MSL` (`FL180`, base of Class A).
fn compute_display_ceiling(vol: &AirspaceVolume) -> (f64, &'static str, i32) {
    match vol.classification {
        AirspaceClassification::Controlled {
            local_type: ControlledLocalType::ClassE5Transition,
            ..
        } if matches!(vol.lower.datum, AltitudeDatum::Agl | AltitudeDatum::Surface)
            && vol.lower.value_ft > 0
            && vol.lower.value_ft < 1_200 =>
        {
            (1_200.0 * METRES_PER_FOOT, "agl", 1_200)
        }
        AirspaceClassification::Controlled {
            local_type:
                ControlledLocalType::ClassE2Surface
                | ControlledLocalType::ClassE3Extension
                | ControlledLocalType::ClassE4Extension,
            ..
        } if vol.lower.value_ft == 0
            && vol.upper.datum == AltitudeDatum::Msl
            && vol.upper.value_ft == VFR_SUA_DISPLAY_CAP_FT =>
        {
            (700.0 * METRES_PER_FOOT, "agl", 700)
        }
        AirspaceClassification::SpecialUse(_)
            if is_msl_like(vol.upper.datum) && vol.upper.value_ft > VFR_SUA_DISPLAY_CAP_FT =>
        {
            (
                f64::from(VFR_SUA_DISPLAY_CAP_FT) * METRES_PER_FOOT,
                "msl",
                VFR_SUA_DISPLAY_CAP_FT,
            )
        }
        _ => (
            vol.upper.metres(),
            vol.upper.datum.renderer_datum(),
            vol.upper.value_ft,
        ),
    }
}

fn build_poly_attributes(prim: &PreparedPrimitive<'_>) -> Vec<(String, AttributeValue)> {
    let vol = prim.template;
    let lower_m = round_cm(prim.lower_m);
    let upper_m = round_cm(prim.upper_m);
    let display_upper_m = round_cm(prim.display_upper_m);
    let datum_mode = format!("{}_{}", prim.lower_datum, prim.display_upper_datum);

    let mut attrs = vec![
        (
            "airspace_key".to_owned(),
            AttributeValue::String(vol.airspace_key.clone()),
        ),
        (
            "category".to_owned(),
            AttributeValue::String(vol.classification.category_tag().to_owned()),
        ),
        (
            "class".to_owned(),
            AttributeValue::String(vol.classification.class_code().to_owned()),
        ),
        (
            "local_type".to_owned(),
            AttributeValue::String(vol.classification.local_type_tag().to_owned()),
        ),
        (
            "part".to_owned(),
            AttributeValue::String(prim.part.as_str().to_owned()),
        ),
        (
            "ident".to_owned(),
            AttributeValue::String(vol.ident.clone()),
        ),
        ("name".to_owned(), AttributeValue::String(vol.name.clone())),
        ("lower_m".to_owned(), AttributeValue::Double(lower_m)),
        ("upper_m".to_owned(), AttributeValue::Double(upper_m)),
        (
            "display_upper_m".to_owned(),
            AttributeValue::Double(display_upper_m),
        ),
        (
            "lower_ft".to_owned(),
            AttributeValue::Int(i64::from(prim.lower_ft)),
        ),
        (
            "upper_ft".to_owned(),
            AttributeValue::Int(i64::from(prim.upper_ft)),
        ),
        (
            "lower_datum".to_owned(),
            AttributeValue::String(prim.lower_datum.to_owned()),
        ),
        (
            "upper_datum".to_owned(),
            AttributeValue::String(prim.upper_datum.to_owned()),
        ),
        (
            "display_upper_datum".to_owned(),
            AttributeValue::String(prim.display_upper_datum.to_owned()),
        ),
        ("datum_mode".to_owned(), AttributeValue::String(datum_mode)),
        (
            "floating_floor".to_owned(),
            AttributeValue::Bool(prim.lower_ft > 0),
        ),
        (
            "is_enroute_blanket".to_owned(),
            AttributeValue::Bool(prim.is_enroute_blanket),
        ),
    ];

    if prim.include_sector
        && let Some(ref sec) = vol.sector
    {
        attrs.push(("sector".to_owned(), AttributeValue::String(sec.clone())));
    }
    if let Some(ref hrs) = vol.hours_of_use {
        attrs.push(("hours".to_owned(), AttributeValue::String(hrs.clone())));
    }
    if let Some(ref ag) = vol.controlling_agency {
        attrs.push(("agency".to_owned(), AttributeValue::String(ag.clone())));
    }
    attrs
}

fn with_rim_attributes(
    base_attrs: &[(String, AttributeValue)],
    role: &'static str,
    rim_m: f64,
    rim_datum: &'static str,
) -> Vec<(String, AttributeValue)> {
    let mut attrs = Vec::with_capacity(base_attrs.len() + 3);
    attrs.extend_from_slice(base_attrs);
    attrs.push((
        "boundary_role".to_owned(),
        AttributeValue::String(role.to_owned()),
    ));
    attrs.push(("rim_m".to_owned(), AttributeValue::Double(round_cm(rim_m))));
    attrs.push((
        "rim_datum".to_owned(),
        AttributeValue::String(rim_datum.to_owned()),
    ));
    attrs
}

fn round_cm(val: f64) -> f64 {
    (val * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use flightbag_core::VerticalLimit;
    use flightbag_nasr::boolean2d::normalize_raw_rings;

    fn rect_poly(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> Vec<AirspacePolygon> {
        normalize_raw_rings(&[vec![
            [min_lon, min_lat],
            [max_lon, min_lat],
            [max_lon, max_lat],
            [min_lon, max_lat],
        ]])
    }

    #[test]
    fn slices_class_b_wedding_cake_into_single_roof_and_stepped_floor_terrace() {
        let core = AirspaceVolume {
            airspace_key: "B:SEA_B".into(),
            ident: "SEA".into(),
            name: "SEATTLE CLASS B".into(),
            sector: Some("AREA A".into()),
            classification: AirspaceClassification::Controlled {
                class: ControlledAirspaceClass::ClassB,
                local_type: ControlledLocalType::ClassB,
            },
            lower: VerticalLimit::SURFACE,
            upper: VerticalLimit::msl_ft(10_000),
            hours_of_use: None,
            controlling_agency: None,
            polygons: rect_poly(-122.4, 47.4, -122.3, 47.5),
        };
        let shelf = AirspaceVolume {
            sector: Some("AREA H".into()),
            lower: VerticalLimit::msl_ft(3_000),
            polygons: subtract_polygons(&rect_poly(-122.5, 47.3, -122.2, 47.6), &core.polygons),
            ..core.clone()
        };
        let vols = vec![core, shelf];
        let prims = prepare_primitives(&vols);

        let roofs: Vec<_> = prims
            .iter()
            .filter(|p| p.part == ManifoldPart::Roof)
            .collect();
        let floors: Vec<_> = prims
            .iter()
            .filter(|p| p.part == ManifoldPart::Floor)
            .collect();
        let walls: Vec<_> = prims
            .iter()
            .filter(|p| matches!(p.part, ManifoldPart::Walls { .. }))
            .collect();

        // Exactly 1 merged 10,000 ft roof with NO interior holes, 1 stepped 3,000 ft donut floor
        // terrace WITH an interior hole, and 2 wall bands ([0..3000] core, [3000..10000] outer).
        assert_eq!(roofs.len(), 1);
        assert!(roofs.first().is_some_and(|r| {
            r.upper_ft == 10_000 && r.projected.iter().all(|poly| poly.interiors.is_empty())
        }));
        assert_eq!(floors.len(), 1);
        assert!(floors.first().is_some_and(|f| {
            f.lower_ft == 3_000 && f.projected.iter().any(|poly| !poly.interiors.is_empty())
        }));
        assert_eq!(walls.len(), 2);
    }
}
