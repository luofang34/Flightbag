//! Pure-Rust AIXM 5.0 Special Use Airspace (`SUA`) compiler for
//! `Additional_Data/AIXM/SAA-AIXM_5_Schema/SaaSubscriberFile.zip` (`Saa_Sub_File.zip/*.xml`).
//!
//! Supports:
//! - All 6 FAA SUA categories (`PA` -> `P`, `RA` -> `R`, `WA` -> `W`, `MOA` -> `MOA`, `AA` -> `A`, `NSA` -> `NSA`).
//! - OGC `CRS84` (`lon lat`) `<LinearRing>`, `<LineStringSegment>`, `<CircleByCenterPoint>`, and
//!   `<ArcByCenterPoint>` geodesic sampling.
//! - Intra-document `<AirspaceVolumeDependency>` (`xlink:href="#Airspace2"`) resolution.
//! - Sequential 3D slab boolean CSG (`BASE`, `UNION`, `SUBTR` ordered by `<operationSequence>`).

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek};

use flightbag_core::{
    AirspaceClassification, AirspacePolygon, AirspaceVolume, SuaKind, VerticalLimit,
};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::boolean2d::{
    intersect_polygons, normalize_raw_rings, polygons_bbox, subtract_polygons, union_polygons,
};
use crate::{GeoBbox, NasrCompileError, canonical_airspace_key};

const OUTER_SAA_ZIP: &str = "Additional_Data/AIXM/SAA-AIXM_5_Schema/SaaSubscriberFile.zip";
const INNER_SAA_ZIP: &str = "Saa_Sub_File.zip";
const EARTH_RADIUS_M: f64 = 6_371_000.0;
const METRES_PER_NM: f64 = 1_852.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeomOp {
    Base,
    Union,
    Subtr,
}

#[derive(Clone, Debug)]
struct RawVolumeComponent {
    seq: i32,
    op: GeomOp,
    lower: VerticalLimit,
    upper: VerticalLimit,
    polygons: Vec<AirspacePolygon>,
    contributor_id: Option<String>,
}

#[derive(Clone, Debug)]
struct ParsedDocAirspace {
    gml_id: String,
    designator: String,
    name: String,
    sua_kind: Option<SuaKind>,
    components: Vec<RawVolumeComponent>,
}

#[derive(Clone, Debug)]
struct Slab3d {
    polygons: Vec<AirspacePolygon>,
    lower: VerticalLimit,
    upper: VerticalLimit,
}

/// Compiles all FAA Special Use Airspace (`P, R, W, MOA, A, NSA`) 3D volumes from `archive`.
///
/// # Errors
///
/// Returns [`NasrCompileError`] if the nested SAA ZIP containers or XML payloads fail to parse.
pub fn compile_sua_airspace<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    bbox_filter: Option<GeoBbox>,
) -> Result<Vec<AirspaceVolume>, NasrCompileError> {
    let outer_bytes = {
        let mut entry = archive
            .by_name(OUTER_SAA_ZIP)
            .map_err(|source| NasrCompileError::Zip {
                context: OUTER_SAA_ZIP.to_owned(),
                source,
            })?;
        let mut buf = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
        entry
            .read_to_end(&mut buf)
            .map_err(|err| NasrCompileError::Xml {
                entry: OUTER_SAA_ZIP.to_owned(),
                reason: err.to_string(),
            })?;
        buf
    };

    let mut outer_zip =
        zip::ZipArchive::new(Cursor::new(outer_bytes)).map_err(|source| NasrCompileError::Zip {
            context: OUTER_SAA_ZIP.to_owned(),
            source,
        })?;

    let inner_bytes = {
        let mut entry =
            outer_zip
                .by_name(INNER_SAA_ZIP)
                .map_err(|source| NasrCompileError::Zip {
                    context: INNER_SAA_ZIP.to_owned(),
                    source,
                })?;
        let mut buf = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
        entry
            .read_to_end(&mut buf)
            .map_err(|err| NasrCompileError::Xml {
                entry: INNER_SAA_ZIP.to_owned(),
                reason: err.to_string(),
            })?;
        buf
    };

    let mut inner_zip =
        zip::ZipArchive::new(Cursor::new(inner_bytes)).map_err(|source| NasrCompileError::Zip {
            context: INNER_SAA_ZIP.to_owned(),
            source,
        })?;

    let mut xml_names: Vec<String> = (0..inner_zip.len())
        .filter_map(|idx| inner_zip.by_index(idx).ok().map(|f| f.name().to_owned()))
        .filter(|name| name.ends_with(".xml"))
        .collect();
    xml_names.sort();

    let mut out = Vec::with_capacity(xml_names.len());
    for entry_name in xml_names {
        let mut xml_str = String::new();
        {
            let mut file =
                inner_zip
                    .by_name(&entry_name)
                    .map_err(|source| NasrCompileError::Zip {
                        context: entry_name.clone(),
                        source,
                    })?;
            file.read_to_string(&mut xml_str)
                .map_err(|err| NasrCompileError::Xml {
                    entry: entry_name.clone(),
                    reason: err.to_string(),
                })?;
        }
        let parsed = parse_sua_xml(&entry_name, &xml_str, bbox_filter)?;
        out.extend(parsed);
    }

    Ok(out)
}

#[derive(Clone, Debug, Default)]
struct SegmentScratch {
    kind: String,
    positions: Vec<[f64; 2]>,
    radius_val: Option<f64>,
    radius_uom: String,
    start_angle_deg: Option<f64>,
    end_angle_deg: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct PatchScratch {
    in_interior: bool,
    current_ring: Vec<[f64; 2]>,
    rings: Vec<Vec<[f64; 2]>>,
}

#[derive(Clone, Debug)]
struct CompScratch {
    seq: i32,
    op: GeomOp,
    lower_val: String,
    lower_uom: String,
    lower_ref: String,
    upper_val: String,
    upper_uom: String,
    upper_ref: String,
    contributor_id: Option<String>,
    polygons: Vec<AirspacePolygon>,
}

impl Default for CompScratch {
    fn default() -> Self {
        Self {
            seq: 0,
            op: GeomOp::Base,
            lower_val: String::new(),
            lower_uom: String::new(),
            lower_ref: String::new(),
            upper_val: String::new(),
            upper_uom: String::new(),
            upper_ref: String::new(),
            contributor_id: None,
            polygons: Vec::new(),
        }
    }
}

fn parse_sua_xml(
    entry_name: &str,
    xml: &str,
    bbox_filter: Option<GeoBbox>,
) -> Result<Vec<AirspaceVolume>, NasrCompileError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<String> = Vec::with_capacity(24);
    let mut airspaces: Vec<ParsedDocAirspace> = Vec::new();
    let mut cur_airspace: Option<ParsedDocAirspace> = None;
    let mut cur_comp: Option<CompScratch> = None;
    let mut cur_patch: Option<PatchScratch> = None;
    let mut cur_seg: Option<SegmentScratch> = None;
    let mut active_limit_uom = String::new();

    let mut working_hours: Option<String> = None;
    let mut cur_unit_name = String::new();
    let mut cur_unit_type = String::new();
    let mut controlling_agency: Option<String> = None;
    let mut fallback_agency: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = local_name_bytes(e.name().as_ref());
                handle_open_tag(
                    &tag,
                    &e,
                    &stack,
                    &mut cur_airspace,
                    &mut cur_comp,
                    &mut cur_patch,
                    &mut cur_seg,
                    &mut active_limit_uom,
                );
                stack.push(tag);
            }
            Ok(Event::Empty(e)) => {
                let tag = local_name_bytes(e.name().as_ref());
                handle_open_tag(
                    &tag,
                    &e,
                    &stack,
                    &mut cur_airspace,
                    &mut cur_comp,
                    &mut cur_patch,
                    &mut cur_seg,
                    &mut active_limit_uom,
                );
            }
            Ok(Event::Text(t)) => {
                let text = std::str::from_utf8(t.as_ref()).unwrap_or("").trim();
                if !text.is_empty() {
                    handle_text(
                        text,
                        &stack,
                        &active_limit_uom,
                        &mut cur_airspace,
                        &mut cur_comp,
                        &mut cur_patch,
                        &mut cur_seg,
                        &mut working_hours,
                        &mut cur_unit_name,
                        &mut cur_unit_type,
                    );
                }
            }
            Ok(Event::End(e)) => {
                let tag = local_name_bytes(e.name().as_ref());
                let _ = stack.pop();
                match tag.as_str() {
                    "LineStringSegment"
                    | "GeodesicString"
                    | "CircleByCenterPoint"
                    | "ArcByCenterPoint" => {
                        if let (Some(seg), Some(patch)) = (cur_seg.take(), cur_patch.as_mut()) {
                            append_segment_points(&seg, &mut patch.current_ring);
                        }
                    }
                    "exterior" | "interior" => {
                        if let Some(patch) = cur_patch.as_mut() {
                            if patch.current_ring.len() >= 3 {
                                patch.rings.push(std::mem::take(&mut patch.current_ring));
                            } else {
                                patch.current_ring.clear();
                            }
                            patch.in_interior = false;
                        }
                    }
                    "PolygonPatch" => {
                        if let (Some(patch), Some(comp)) = (cur_patch.take(), cur_comp.as_mut()) {
                            let polys = normalize_raw_rings(&patch.rings);
                            comp.polygons.extend(polys);
                        }
                    }
                    "AirspaceGeometryComponent" => {
                        if let (Some(comp), Some(air)) = (cur_comp.take(), cur_airspace.as_mut()) {
                            let lower = parse_sua_limit(
                                &comp.lower_val,
                                &comp.lower_uom,
                                &comp.lower_ref,
                                false,
                            );
                            let upper = parse_sua_limit(
                                &comp.upper_val,
                                &comp.upper_uom,
                                &comp.upper_ref,
                                true,
                            );
                            air.components.push(RawVolumeComponent {
                                seq: comp.seq,
                                op: comp.op,
                                lower,
                                upper,
                                polygons: comp.polygons,
                                contributor_id: comp.contributor_id,
                            });
                        }
                    }
                    "Airspace" => {
                        if let Some(air) = cur_airspace.take() {
                            airspaces.push(air);
                        }
                    }
                    "UnitTimeSlice" => {
                        if !cur_unit_name.is_empty() && cur_unit_name != "NOT AVAILABLE" {
                            if matches!(cur_unit_type.as_str(), "ARTCC" | "TRACON" | "TWR") {
                                controlling_agency = Some(cur_unit_name.clone());
                            } else if fallback_agency.is_none() {
                                fallback_agency = Some(cur_unit_name.clone());
                            }
                        }
                        cur_unit_name.clear();
                        cur_unit_type.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(NasrCompileError::Xml {
                    entry: entry_name.to_owned(),
                    reason: err.to_string(),
                });
            }
        }
    }

    let Some(primary) = airspaces.first().cloned() else {
        return Ok(Vec::new());
    };
    let Some(sua_kind) = primary.sua_kind else {
        return Ok(Vec::new());
    };

    let mut by_id: BTreeMap<String, Vec<AirspacePolygon>> = BTreeMap::new();
    for a in &airspaces {
        let mut merged = Vec::new();
        for c in &a.components {
            if matches!(c.op, GeomOp::Base | GeomOp::Union) {
                merged = union_polygons(&merged, &c.polygons);
            }
        }
        if !a.gml_id.is_empty() {
            by_id.insert(a.gml_id.clone(), merged);
        }
    }

    let mut comps = primary.components;
    comps.sort_by_key(|c| c.seq);

    let mut slabs: Vec<Slab3d> = Vec::new();
    for mut comp in comps {
        if comp.polygons.is_empty()
            && let Some(ref cid) = comp.contributor_id
            && let Some(contrib) = by_id.get(cid)
        {
            comp.polygons = contrib.clone();
        }
        if comp.polygons.is_empty() {
            continue;
        }
        match comp.op {
            GeomOp::Base | GeomOp::Union => {
                if comp.upper.value_ft > comp.lower.value_ft || comp.upper.datum != comp.lower.datum
                {
                    slabs.push(Slab3d {
                        polygons: comp.polygons,
                        lower: comp.lower,
                        upper: comp.upper,
                    });
                }
            }
            GeomOp::Subtr => {
                slabs = apply_3d_subtraction(slabs, &comp);
            }
        }
    }

    let ident = preferred_sua_ident(&primary.designator, &primary.name, sua_kind);
    let airspace_key = canonical_airspace_key(&ident, sua_kind.code());
    let agency = controlling_agency.or(fallback_agency);
    let multi = slabs.len() > 1;

    let mut volumes = Vec::with_capacity(slabs.len());
    for (idx, slab) in slabs.into_iter().enumerate() {
        if slab.polygons.is_empty() {
            continue;
        }
        if let Some(filter) = bbox_filter {
            let Some(pb) = polygons_bbox(&slab.polygons) else {
                continue;
            };
            if !filter.intersects(pb) {
                continue;
            }
        }
        let sector = if multi {
            Some(format!("PART {}", idx + 1))
        } else {
            None
        };
        volumes.push(AirspaceVolume {
            airspace_key: airspace_key.clone(),
            ident: ident.clone(),
            name: if primary.name.is_empty() {
                ident.clone()
            } else {
                primary.name.clone()
            },
            sector,
            classification: AirspaceClassification::SpecialUse(sua_kind),
            lower: slab.lower,
            upper: slab.upper,
            hours_of_use: working_hours.clone(),
            controlling_agency: agency.clone(),
            polygons: slab.polygons,
        });
    }

    Ok(volumes)
}

#[allow(clippy::too_many_arguments)]
fn handle_open_tag(
    tag: &str,
    elem: &BytesStart<'_>,
    stack: &[String],
    cur_airspace: &mut Option<ParsedDocAirspace>,
    cur_comp: &mut Option<CompScratch>,
    cur_patch: &mut Option<PatchScratch>,
    cur_seg: &mut Option<SegmentScratch>,
    active_limit_uom: &mut String,
) {
    match tag {
        "Airspace" if !stack.iter().any(|s| s == "Airspace") => {
            let gml_id = attr_local(elem, "id").unwrap_or_default();
            *cur_airspace = Some(ParsedDocAirspace {
                gml_id,
                designator: String::new(),
                name: String::new(),
                sua_kind: None,
                components: Vec::new(),
            });
        }
        "AirspaceGeometryComponent" if cur_airspace.is_some() => {
            *cur_comp = Some(CompScratch::default());
        }
        "upperLimit" | "lowerLimit" if cur_comp.is_some() => {
            *active_limit_uom = attr_local(elem, "uom").unwrap_or_default();
        }
        "theAirspace" if cur_comp.is_some() => {
            if let (Some(href), Some(comp)) = (attr_local(elem, "href"), cur_comp.as_mut()) {
                let id = href.trim_start_matches('#').to_owned();
                if !id.is_empty() {
                    comp.contributor_id = Some(id);
                }
            }
        }
        "PolygonPatch" if cur_comp.is_some() => {
            *cur_patch = Some(PatchScratch::default());
        }
        "interior" => {
            if let Some(patch) = cur_patch.as_mut() {
                patch.in_interior = true;
            }
        }
        "LineStringSegment" | "GeodesicString" | "CircleByCenterPoint" | "ArcByCenterPoint"
            if cur_patch.is_some() =>
        {
            *cur_seg = Some(SegmentScratch {
                kind: tag.to_owned(),
                ..SegmentScratch::default()
            });
        }
        "radius" => {
            if let Some(seg) = cur_seg.as_mut() {
                seg.radius_uom = attr_local(elem, "uom").unwrap_or_else(|| "NM".to_owned());
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_text(
    text: &str,
    stack: &[String],
    active_limit_uom: &str,
    cur_airspace: &mut Option<ParsedDocAirspace>,
    cur_comp: &mut Option<CompScratch>,
    cur_patch: &mut Option<PatchScratch>,
    cur_seg: &mut Option<SegmentScratch>,
    working_hours: &mut Option<String>,
    cur_unit_name: &mut String,
    cur_unit_type: &mut String,
) {
    let Some(leaf) = stack.last().map(String::as_str) else {
        return;
    };
    let parent = stack
        .len()
        .checked_sub(2)
        .and_then(|i| stack.get(i))
        .map(String::as_str)
        .unwrap_or("");

    if let Some(seg) = cur_seg.as_mut() {
        match leaf {
            "pos" | "posList" => {
                seg.positions.extend(parse_lon_lat_pairs(text));
                return;
            }
            "radius" => {
                seg.radius_val = text.parse::<f64>().ok();
                return;
            }
            "startAngle" => {
                seg.start_angle_deg = text.parse::<f64>().ok();
                return;
            }
            "endAngle" => {
                seg.end_angle_deg = text.parse::<f64>().ok();
                return;
            }
            _ => {}
        }
    }

    if let Some(patch) = cur_patch.as_mut()
        && matches!(leaf, "pos" | "posList")
        && parent == "LinearRing"
    {
        patch.current_ring.extend(parse_lon_lat_pairs(text));
        return;
    }

    if let Some(comp) = cur_comp.as_mut() {
        let in_airspace_volume =
            stack.iter().any(|s| s == "AirspaceVolume") && !stack.iter().any(|s| s == "Note");
        match leaf {
            "operation" => {
                comp.op = match text {
                    "SUBTR" => GeomOp::Subtr,
                    "UNION" => GeomOp::Union,
                    _ => GeomOp::Base,
                };
                return;
            }
            "operationSequence" => {
                if let Ok(seq) = text.parse::<i32>() {
                    comp.seq = seq;
                }
                return;
            }
            "upperLimit" if in_airspace_volume => {
                comp.upper_val = text.to_owned();
                comp.upper_uom = active_limit_uom.to_owned();
                return;
            }
            "upperLimitReference" if in_airspace_volume => {
                comp.upper_ref = text.to_owned();
                return;
            }
            "lowerLimit" if in_airspace_volume => {
                comp.lower_val = text.to_owned();
                comp.lower_uom = active_limit_uom.to_owned();
                return;
            }
            "lowerLimitReference" if in_airspace_volume => {
                comp.lower_ref = text.to_owned();
                return;
            }
            _ => {}
        }
    }

    if let Some(air) = cur_airspace.as_mut() {
        if parent == "AirspaceTimeSlice" {
            if leaf == "designator" && air.designator.is_empty() {
                air.designator = text.to_owned();
                return;
            }
            if leaf == "name" && air.name.is_empty() {
                air.name = text.to_owned();
                return;
            }
        }
        if leaf == "suaType" {
            air.sua_kind = match text {
                "PA" => Some(SuaKind::Prohibited),
                "RA" => Some(SuaKind::Restricted),
                "WA" => Some(SuaKind::Warning),
                "MOA" => Some(SuaKind::Moa),
                "AA" => Some(SuaKind::Alert),
                "NSA" => Some(SuaKind::NationalSecurityArea),
                _ => None,
            };
            return;
        }
    }

    if leaf == "workingHours" && working_hours.is_none() {
        *working_hours = Some(text.to_owned());
    } else if parent == "UnitTimeSlice" {
        if leaf == "name" && cur_unit_name.is_empty() {
            *cur_unit_name = text.to_owned();
        } else if leaf == "type" && cur_unit_type.is_empty() {
            *cur_unit_type = text.to_owned();
        }
    }
}

fn apply_3d_subtraction(slabs: Vec<Slab3d>, sub: &RawVolumeComponent) -> Vec<Slab3d> {
    let sub_low = sub.lower.value_ft;
    let sub_high = sub.upper.value_ft;
    let mut next_slabs = Vec::with_capacity(slabs.len() + 2);

    for slab in slabs {
        let s_low = slab.lower.value_ft;
        let s_high = slab.upper.value_ft;
        if sub_high <= s_low || sub_low >= s_high {
            next_slabs.push(slab);
            continue;
        }
        let outside = subtract_polygons(&slab.polygons, &sub.polygons);
        let overlap = intersect_polygons(&slab.polygons, &sub.polygons);
        if overlap.is_empty() {
            next_slabs.push(slab);
            continue;
        }
        if !outside.is_empty() {
            next_slabs.push(Slab3d {
                polygons: outside,
                lower: slab.lower,
                upper: slab.upper,
            });
        }
        if sub_low > s_low {
            next_slabs.push(Slab3d {
                polygons: overlap.clone(),
                lower: slab.lower,
                upper: sub.lower,
            });
        }
        if sub_high < s_high {
            next_slabs.push(Slab3d {
                polygons: overlap,
                lower: sub.upper,
                upper: slab.upper,
            });
        }
    }
    next_slabs
}

fn append_segment_points(seg: &SegmentScratch, out_ring: &mut Vec<[f64; 2]>) {
    match seg.kind.as_str() {
        "CircleByCenterPoint" => {
            if let (Some(&[clon, clat]), Some(radius)) = (seg.positions.first(), seg.radius_val) {
                let radius_m = radius_to_metres(radius, &seg.radius_uom);
                let steps: usize = 64;
                for idx in 0..steps {
                    let math_deg = (idx as f64) * (360.0 / (steps as f64));
                    out_ring.push(geodesic_point_from_math_angle(
                        clon, clat, radius_m, math_deg,
                    ));
                }
            }
        }
        "ArcByCenterPoint" => {
            if let (Some(&[clon, clat]), Some(radius), Some(start_deg), Some(end_deg)) = (
                seg.positions.first(),
                seg.radius_val,
                seg.start_angle_deg,
                seg.end_angle_deg,
            ) {
                let radius_m = radius_to_metres(radius, &seg.radius_uom);
                let sweep = end_deg - start_deg;
                let steps = ((sweep.abs() / 3.0).ceil() as usize).clamp(8, 128);
                for idx in 0..=steps {
                    let frac = (idx as f64) / (steps as f64);
                    let math_deg = start_deg + frac * sweep;
                    out_ring.push(geodesic_point_from_math_angle(
                        clon, clat, radius_m, math_deg,
                    ));
                }
            }
        }
        _ => {
            out_ring.extend_from_slice(&seg.positions);
        }
    }
}

/// Evaluates a spherical geodesic offset at radius `dist_m` and GML `CRS84` math angle `math_deg`
/// (`0° = +East`, `+90° = +North`, matching FAA `ArcByCenterPoint` `startAngle`/`endAngle`).
fn geodesic_point_from_math_angle(
    clon_deg: f64,
    clat_deg: f64,
    dist_m: f64,
    math_deg: f64,
) -> [f64; 2] {
    let bearing_rad = (90.0 - math_deg).to_radians();
    let ang_dist = dist_m / EARTH_RADIUS_M;
    let lat1 = clat_deg.to_radians();
    let lon1 = clon_deg.to_radians();

    let sin_lat2 = lat1.sin() * ang_dist.cos() + lat1.cos() * ang_dist.sin() * bearing_rad.cos();
    let lat2 = sin_lat2.clamp(-1.0, 1.0).asin();
    let y = bearing_rad.sin() * ang_dist.sin() * lat1.cos();
    let x = ang_dist.cos() - lat1.sin() * lat2.sin();
    let lon2 = lon1 + y.atan2(x);

    let mut lon_deg = lon2.to_degrees();
    if lon_deg > 180.0 {
        lon_deg -= 360.0;
    } else if lon_deg < -180.0 {
        lon_deg += 360.0;
    }
    [lon_deg, lat2.to_degrees()]
}

fn radius_to_metres(val: f64, uom: &str) -> f64 {
    match uom.to_ascii_uppercase().as_str() {
        "M" => val,
        "KM" => val * 1_000.0,
        "FT" => val * 0.3048,
        "MI" => val * 1_609.344,
        _ => val * METRES_PER_NM,
    }
}

fn parse_sua_limit(val_raw: &str, uom_raw: &str, ref_raw: &str, is_upper: bool) -> VerticalLimit {
    let v = val_raw.trim();
    if v.eq_ignore_ascii_case("GND") || v.eq_ignore_ascii_case("SFC") {
        return VerticalLimit::SURFACE;
    }
    if v.eq_ignore_ascii_case("UNL") {
        return VerticalLimit::unlimited(60_000);
    }
    let default_ft = if is_upper { 18_000 } else { 0 };
    let parsed_ft = v
        .parse::<i32>()
        .ok()
        .or_else(|| v.parse::<f64>().ok().map(|f| f.round() as i32))
        .unwrap_or(default_ft);

    if uom_raw.eq_ignore_ascii_case("FL") || ref_raw.eq_ignore_ascii_case("STD") {
        let fl = u16::try_from(parsed_ft.max(0)).unwrap_or(180);
        VerticalLimit::flight_level(fl)
    } else if ref_raw.eq_ignore_ascii_case("SFC") || ref_raw.eq_ignore_ascii_case("AGL") {
        VerticalLimit::agl_ft(parsed_ft)
    } else {
        VerticalLimit::msl_ft(parsed_ft.max(0))
    }
}

fn preferred_sua_ident(designator: &str, name: &str, kind: SuaKind) -> String {
    let first_tok = name.split_whitespace().next().unwrap_or("");
    if matches!(
        kind,
        SuaKind::Prohibited | SuaKind::Restricted | SuaKind::Warning | SuaKind::Alert
    ) && first_tok.contains('-')
    {
        return first_tok.trim_end_matches(',').to_owned();
    }
    if !name.is_empty() && matches!(kind, SuaKind::Moa | SuaKind::NationalSecurityArea) {
        let before_comma = name.split(',').next().unwrap_or(name).trim();
        if !before_comma.is_empty() {
            return before_comma.to_owned();
        }
    }
    if !designator.is_empty() {
        designator.to_owned()
    } else {
        name.to_owned()
    }
}

fn parse_lon_lat_pairs(text: &str) -> Vec<[f64; 2]> {
    let mut nums = text
        .split_whitespace()
        .filter_map(|tok| tok.parse::<f64>().ok());
    let mut pts = Vec::new();
    while let (Some(lon), Some(lat)) = (nums.next(), nums.next()) {
        pts.push([lon, lat]);
    }
    pts
}

fn local_name_bytes(raw: &[u8]) -> String {
    let slice = match raw.iter().rposition(|&b| b == b':') {
        Some(pos) => raw.get(pos + 1..).unwrap_or(raw),
        None => raw,
    };
    String::from_utf8_lossy(slice).into_owned()
}

fn attr_local(elem: &BytesStart<'_>, target: &str) -> Option<String> {
    for attr_res in elem.attributes().with_checks(false) {
        let Ok(attr) = attr_res else {
            continue;
        };
        if local_name_bytes(attr.key.as_ref()) == target {
            return Some(String::from_utf8_lossy(&attr.value).into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sua_3d_subtr_preserves_overhead_shelf_above_partial_altitude_cutout() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<message:AIXMBasicMessage xmlns:message="http://www.aixm.aero/schema/5.1/message"
    xmlns:aixm="http://www.aixm.aero/schema/5.1"
    xmlns:gml="http://www.opengis.net/gml/3.2"
    xmlns:sua="urn:us:gov:dot:faa:aim:saa:sua"
    xmlns:xlink="http://www.w3.org/1999/xlink">
  <message:hasMember>
    <aixm:Airspace gml:id="Airspace1">
      <aixm:timeSlice>
        <aixm:AirspaceTimeSlice>
          <aixm:type>OTHER</aixm:type>
          <aixm:designator>FALLON2</aixm:designator>
          <aixm:name>FALLON SOUTH 2 MOA, NV</aixm:name>
          <aixm:geometryComponent>
            <aixm:AirspaceGeometryComponent>
              <aixm:operation>BASE</aixm:operation>
              <aixm:operationSequence>1</aixm:operationSequence>
              <aixm:theAirspaceVolume>
                <aixm:AirspaceVolume>
                  <aixm:upperLimit uom="FL">180</aixm:upperLimit>
                  <aixm:upperLimitReference>STD</aixm:upperLimitReference>
                  <aixm:lowerLimit uom="FT">500</aixm:lowerLimit>
                  <aixm:lowerLimitReference>SFC</aixm:lowerLimitReference>
                  <aixm:horizontalProjection>
                    <aixm:Surface>
                      <gml:patches>
                        <gml:PolygonPatch>
                          <gml:exterior>
                            <gml:Ring>
                              <gml:curveMember>
                                <gml:Curve>
                                  <gml:segments>
                                    <gml:LineStringSegment>
                                      <gml:posList>-118.5 38.5 -117.5 38.5 -117.5 39.5 -118.5 39.5 -118.5 38.5</gml:posList>
                                    </gml:LineStringSegment>
                                  </gml:segments>
                                </gml:Curve>
                              </gml:curveMember>
                            </gml:Ring>
                          </gml:exterior>
                        </gml:PolygonPatch>
                      </gml:patches>
                    </aixm:Surface>
                  </aixm:horizontalProjection>
                </aixm:AirspaceVolume>
              </aixm:theAirspaceVolume>
            </aixm:AirspaceGeometryComponent>
          </aixm:geometryComponent>
          <aixm:geometryComponent>
            <aixm:AirspaceGeometryComponent>
              <aixm:operation>SUBTR</aixm:operation>
              <aixm:operationSequence>2</aixm:operationSequence>
              <aixm:theAirspaceVolume>
                <aixm:AirspaceVolume>
                  <aixm:upperLimit uom="FT">2000</aixm:upperLimit>
                  <aixm:upperLimitReference>SFC</aixm:upperLimitReference>
                  <aixm:lowerLimit uom="FT">GND</aixm:lowerLimit>
                  <aixm:lowerLimitReference>SFC</aixm:lowerLimitReference>
                  <aixm:horizontalProjection>
                    <aixm:Surface>
                      <gml:patches>
                        <gml:PolygonPatch>
                          <gml:exterior>
                            <gml:Ring>
                              <gml:curveMember>
                                <gml:Curve>
                                  <gml:segments>
                                    <gml:CircleByCenterPoint numArc="1">
                                      <gml:pos>-118.0 39.0</gml:pos>
                                      <gml:radius uom="NM">3</gml:radius>
                                    </gml:CircleByCenterPoint>
                                  </gml:segments>
                                </gml:Curve>
                              </gml:curveMember>
                            </gml:Ring>
                          </gml:exterior>
                        </gml:PolygonPatch>
                      </gml:patches>
                    </aixm:Surface>
                  </aixm:horizontalProjection>
                </aixm:AirspaceVolume>
              </aixm:theAirspaceVolume>
            </aixm:AirspaceGeometryComponent>
          </aixm:geometryComponent>
          <aixm:extension>
            <sua:AirspaceExtension>
              <sua:suaType>MOA</sua:suaType>
            </sua:AirspaceExtension>
          </aixm:extension>
        </aixm:AirspaceTimeSlice>
      </aixm:timeSlice>
    </aixm:Airspace>
  </message:hasMember>
</message:AIXMBasicMessage>"#;

        let volumes = parse_sua_xml("FALLON.xml", xml, None).unwrap_or_default();
        // Should split into 2 slabs:
        // 1. Outside the 3 NM circle: 500 ft AGL .. FL180
        // 2. Overhead above the 3 NM circle: 2,000 ft AGL .. FL180
        assert_eq!(volumes.len(), 2);
        assert!(
            volumes
                .iter()
                .any(|v| v.lower.value_ft == 500 && v.upper.value_ft == 18_000)
        );
        assert!(
            volumes
                .iter()
                .any(|v| v.lower.value_ft == 2_000 && v.upper.value_ft == 18_000)
        );
    }

    #[test]
    fn arc_by_center_point_follows_crs84_math_angles() {
        // 0° = East (+lon, same lat), 90° = North (same lon, +lat)
        let east = geodesic_point_from_math_angle(-122.0, 47.0, 10_000.0, 0.0);
        let north = geodesic_point_from_math_angle(-122.0, 47.0, 10_000.0, 90.0);
        assert!(east[0] > -122.0 && (east[1] - 47.0).abs() < 1e-3);
        assert!((north[0] - (-122.0)).abs() < 1e-6 && north[1] > 47.0);
    }
}
