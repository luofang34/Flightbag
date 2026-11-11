//! Pure-Rust reader and 3D shelf normalizer for `Additional_Data/Shape_Files/Class_Airspace.{shp,shx,dbf}`.

use std::collections::BTreeMap;
use std::io::{Read, Seek};

use flightbag_core::{
    AirspaceClassification, AirspacePolygon, AirspaceVolume, AltitudeDatum,
    ControlledAirspaceClass, ControlledLocalType, VerticalLimit,
};

use crate::boolean2d::{
    intersect_polygons, normalize_raw_rings, polygons_bbox, subtract_polygons, union_polygons,
};
use crate::{GeoBbox, NasrCompileError, canonical_airspace_key};

const DBF_ENTRY: &str = "Additional_Data/Shape_Files/Class_Airspace.dbf";
const SHX_ENTRY: &str = "Additional_Data/Shape_Files/Class_Airspace.shx";
const SHP_ENTRY: &str = "Additional_Data/Shape_Files/Class_Airspace.shp";

#[derive(Clone, Debug)]
struct DbfField {
    name: String,
    len: usize,
}

#[derive(Clone, Debug)]
struct RawClassShelf {
    ident: String,
    name: String,
    sector: Option<String>,
    class: ControlledAirspaceClass,
    local_type: ControlledLocalType,
    lower: VerticalLimit,
    upper: VerticalLimit,
    hours: Option<String>,
    comm_name: Option<String>,
    exclusion: bool,
    polygons: Vec<AirspacePolygon>,
}

/// Compiles `Class B, C, D, E` 3D airspace shelves from an open NASR `ZipArchive`.
///
/// # Errors
///
/// Returns [`NasrCompileError`] if `Class_Airspace.{dbf,shx,shp}` cannot be decoded.
pub fn compile_class_airspace<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    bbox_filter: Option<GeoBbox>,
) -> Result<Vec<AirspaceVolume>, NasrCompileError> {
    let dbf = read_zip_entry(archive, DBF_ENTRY)?;
    let shx = read_zip_entry(archive, SHX_ENTRY)?;
    let shp = read_zip_entry(archive, SHP_ENTRY)?;

    let (num_records, header_len, record_len, fields) = parse_dbf_header(&dbf)?;
    let mut raw_shelves = Vec::with_capacity(num_records);

    for idx in 0..num_records {
        let rec_start = header_len.saturating_add(idx.saturating_mul(record_len));
        let rec_end = rec_start.saturating_add(record_len);
        let Some(rec) = dbf.get(rec_start..rec_end) else {
            return Err(NasrCompileError::Shapefile(format!(
                "DBF record {idx} out of bounds"
            )));
        };
        if rec.first().copied() == Some(b'*') {
            continue;
        }
        let attrs = parse_dbf_row(rec, &fields);
        let Some((class, local_type)) =
            classify_row(attr_str(&attrs, "CLASS"), attr_str(&attrs, "LOCAL_TYPE"))
        else {
            continue;
        };

        let Some((shape_bbox, raw_rings)) = read_shp_polygon(&shx, &shp, idx)? else {
            continue;
        };
        if let Some(filter) = bbox_filter
            && !filter.intersects(shape_bbox)
        {
            continue;
        }

        let lower = parse_lower_limit(
            attr_str(&attrs, "LOWER_VAL"),
            attr_str(&attrs, "LOWER_UOM"),
            attr_str(&attrs, "LOWER_CODE"),
        );
        let upper = parse_upper_limit(
            local_type,
            lower,
            attr_str(&attrs, "UPPER_VAL"),
            attr_str(&attrs, "UPPER_UOM"),
            attr_str(&attrs, "UPPER_CODE"),
            attr_str(&attrs, "UPPER_DESC"),
        );
        if upper.value_ft <= lower.value_ft && upper.datum == lower.datum {
            continue;
        }

        let polygons = normalize_raw_rings(&raw_rings);
        if polygons.is_empty() {
            continue;
        }

        let ident = attr_str(&attrs, "IDENT").to_owned();
        let name = non_empty(attr_str(&attrs, "NAME")).unwrap_or_else(|| {
            if ident.is_empty() {
                format!("CLASS {}", class.code())
            } else {
                format!("{ident} CLASS {}", class.code())
            }
        });
        let sector = non_empty(attr_str(&attrs, "SECTOR"));
        let hours = match (
            non_empty(attr_str(&attrs, "WKHR_CODE")),
            non_empty(attr_str(&attrs, "WKHR_RMK")),
        ) {
            (Some(code), Some(rmk)) if rmk != code => Some(format!("{code} ({rmk})")),
            (Some(code), _) => Some(code),
            (None, Some(rmk)) => Some(rmk),
            (None, None) => None,
        };
        let comm_name = non_empty(attr_str(&attrs, "COMM_NAME"));
        let exclusion = attr_str(&attrs, "EXCLUSION") == "1";

        raw_shelves.push(RawClassShelf {
            ident,
            name,
            sector,
            class,
            local_type,
            lower,
            upper,
            hours,
            comm_name,
            exclusion,
            polygons,
        });
    }

    let normalized = apply_regulatory_precedence(normalize_complex_shelves(raw_shelves));
    let mut out = Vec::with_capacity(normalized.len());
    for shelf in normalized {
        if let Some(filter) = bbox_filter {
            let Some(pb) = polygons_bbox(&shelf.polygons) else {
                continue;
            };
            if !filter.intersects(pb) {
                continue;
            }
        }
        let display_ident = if shelf.ident.is_empty() {
            shelf.name.clone()
        } else {
            shelf.ident.clone()
        };
        let key_seed = if shelf.ident.is_empty() {
            shelf.name.clone()
        } else {
            format!("{} {}", shelf.ident, shelf.class.code())
        };
        let airspace_key = canonical_airspace_key(&key_seed, shelf.class.code());
        out.push(AirspaceVolume {
            airspace_key,
            ident: display_ident,
            name: shelf.name,
            sector: shelf.sector,
            classification: AirspaceClassification::Controlled {
                class: shelf.class,
                local_type: shelf.local_type,
            },
            lower: shelf.lower,
            upper: shelf.upper,
            hours_of_use: shelf.hours,
            controlling_agency: shelf.comm_name,
            polygons: shelf.polygons,
        });
    }

    Ok(out)
}

fn normalize_complex_shelves(shelves: Vec<RawClassShelf>) -> Vec<RawClassShelf> {
    let mut groups: BTreeMap<(String, ControlledLocalType), Vec<RawClassShelf>> = BTreeMap::new();
    let mut ungrouped = Vec::new();

    for shelf in shelves {
        if shelf.ident.is_empty() {
            ungrouped.push(shelf);
        } else {
            groups
                .entry((shelf.ident.clone(), shelf.local_type))
                .or_default()
                .push(shelf);
        }
    }

    let mut out = Vec::with_capacity(ungrouped.len() + groups.len() * 4);
    for ((_ident, local_type), group) in groups {
        // 1. Where EXCLUSION='1' shelves exist (e.g. King Salmon 600..2600 or San Diego VFR corridor
        //    0..3300 + 4700..10000), subtract their lateral union from EXCLUSION='0' rows so the
        //    exclusion-modified shelves cleanly replace that footprint without overlapping.
        let mut excl_footprint: Vec<AirspacePolygon> = Vec::new();
        for item in group.iter().filter(|s| s.exclusion) {
            excl_footprint = union_polygons(&excl_footprint, &item.polygons);
        }

        let mut cleaned: Vec<RawClassShelf> = Vec::with_capacity(group.len());
        for mut item in group {
            if !item.exclusion && !excl_footprint.is_empty() {
                item.polygons = subtract_polygons(&item.polygons, &excl_footprint);
            }
            if !item.polygons.is_empty() {
                cleaned.push(item);
            }
        }

        // 2. Within Class B and Class C wedding-cake complexes, outer higher-floor shelves
        //    (e.g. SEA AREA H 3000..10000) sometimes cover the full rectangle over inner full-height
        //    cores (SEA AREA A 0..10000). Subtract any shelf whose vertical span strictly contains
        //    another shelf's vertical span so 3D translucent roofs/walls never double-blend.
        if matches!(
            local_type,
            ControlledLocalType::ClassB | ControlledLocalType::ClassC
        ) && cleaned.len() > 1
        {
            let snapshot = cleaned.clone();
            for (idx, item) in cleaned.iter_mut().enumerate() {
                for (other_idx, other) in snapshot.iter().enumerate() {
                    if idx == other_idx {
                        continue;
                    }
                    let other_low = other.lower.value_ft;
                    let other_high = other.upper.value_ft;
                    let item_low = item.lower.value_ft;
                    let item_high = item.upper.value_ft;
                    let strictly_contains_vertically = other_low <= item_low
                        && other_high >= item_high
                        && (other_low < item_low || other_high > item_high);
                    if strictly_contains_vertically {
                        item.polygons = subtract_polygons(&item.polygons, &other.polygons);
                    }
                }
            }
        }

        for item in cleaned {
            if !item.polygons.is_empty() {
                out.push(item);
            }
        }
    }

    out.extend(ungrouped);
    out
}

/// Applies `14 CFR § 71.9` overlapping controlled airspace precedence
/// (`Class B > Class C > Class D > Class E2/E3/E4`) so underlying Class C/D cylinders clip
/// cleanly beneath overlying Class B/C shelves (`[-25]`, `[-30]`) and part-time `CLASS_E2`
/// surface areas do not duplicate active `CLASS_D` footprints.
fn apply_regulatory_precedence(shelves: Vec<RawClassShelf>) -> Vec<RawClassShelf> {
    let mut tier_b = Vec::new();
    let mut tier_c = Vec::new();
    let mut tier_d = Vec::new();
    let mut tier_e_sfc = Vec::new();
    let mut unranked = Vec::new();

    for shelf in shelves {
        match shelf.class {
            ControlledAirspaceClass::ClassB => tier_b.push(shelf),
            ControlledAirspaceClass::ClassC => tier_c.push(shelf),
            ControlledAirspaceClass::ClassD => tier_d.push(shelf),
            ControlledAirspaceClass::ClassE
                if matches!(
                    shelf.local_type,
                    ControlledLocalType::ClassE2Surface
                        | ControlledLocalType::ClassE3Extension
                        | ControlledLocalType::ClassE4Extension
                ) =>
            {
                tier_e_sfc.push(shelf);
            }
            ControlledAirspaceClass::ClassE => unranked.push(shelf),
        }
    }

    let mut cutters: Vec<(GeoBbox, RawClassShelf)> = Vec::new();
    for shelf in &tier_b {
        if let Some(bbox) = polygons_bbox(&shelf.polygons) {
            cutters.push((bbox, shelf.clone()));
        }
    }

    let resolved_c = subtract_tier_against_cutters(tier_c, &cutters);
    for shelf in &resolved_c {
        if let Some(bbox) = polygons_bbox(&shelf.polygons) {
            cutters.push((bbox, shelf.clone()));
        }
    }

    let resolved_d = subtract_tier_against_cutters(tier_d, &cutters);
    for shelf in &resolved_d {
        if let Some(bbox) = polygons_bbox(&shelf.polygons) {
            cutters.push((bbox, shelf.clone()));
        }
    }

    let resolved_e_sfc = subtract_tier_against_cutters(tier_e_sfc, &cutters);

    let mut out = Vec::with_capacity(
        tier_b.len() + resolved_c.len() + resolved_d.len() + resolved_e_sfc.len() + unranked.len(),
    );
    out.extend(tier_b);
    out.extend(resolved_c);
    out.extend(resolved_d);
    out.extend(resolved_e_sfc);
    out.extend(unranked);
    out
}

fn subtract_tier_against_cutters(
    tier: Vec<RawClassShelf>,
    cutters: &[(GeoBbox, RawClassShelf)],
) -> Vec<RawClassShelf> {
    if tier.is_empty() || cutters.is_empty() {
        return tier;
    }
    let mut fragments = tier;
    for (cutter_bbox, cutter) in cutters {
        let mut next = Vec::with_capacity(fragments.len());
        for frag in fragments {
            next.extend(subtract_3d_precedence(frag, cutter, *cutter_bbox));
        }
        fragments = next;
        if fragments.is_empty() {
            break;
        }
    }
    fragments
}

fn is_msl_like(datum: AltitudeDatum) -> bool {
    matches!(datum, AltitudeDatum::Msl | AltitudeDatum::FlightLevel)
}

fn is_zero_or_msl(limit: VerticalLimit) -> bool {
    limit.value_ft == 0 || is_msl_like(limit.datum)
}

fn subtract_3d_precedence(
    subject: RawClassShelf,
    cutter: &RawClassShelf,
    cutter_bbox: GeoBbox,
) -> Vec<RawClassShelf> {
    let Some(subj_bbox) = polygons_bbox(&subject.polygons) else {
        return Vec::new();
    };
    if !subj_bbox.intersects(cutter_bbox) {
        return vec![subject];
    }

    // Surface Class E2/E3/E4 extending to the 700 ft AGL transition floor is completely superseded
    // wherever a surface-based Class B/C/D core (`0 SFC .. >= 1,200 ft MSL`) occupies the footprint.
    if subject.lower.value_ft == 0
        && subject.upper.datum == AltitudeDatum::Agl
        && subject.upper.value_ft <= 700
    {
        if cutter.lower.value_ft == 0
            && is_msl_like(cutter.upper.datum)
            && cutter.upper.value_ft >= 1_200
        {
            let outside = subtract_polygons(&subject.polygons, &cutter.polygons);
            if outside.is_empty() {
                return Vec::new();
            }
            let mut rem = subject;
            rem.polygons = outside;
            return vec![rem];
        }
        return vec![subject];
    }

    if !is_zero_or_msl(subject.lower)
        || !is_msl_like(subject.upper.datum)
        || !is_zero_or_msl(cutter.lower)
        || !is_msl_like(cutter.upper.datum)
    {
        return vec![subject];
    }

    let (s_low, s_high) = (subject.lower.value_ft, subject.upper.value_ft);
    let (c_low, c_high) = (cutter.lower.value_ft, cutter.upper.value_ft);
    if c_high <= s_low || c_low >= s_high {
        return vec![subject];
    }

    if c_low <= s_low && c_high >= s_high {
        let outside = subtract_polygons(&subject.polygons, &cutter.polygons);
        if outside.is_empty() {
            return Vec::new();
        }
        let mut rem = subject;
        rem.polygons = outside;
        return vec![rem];
    }

    let inside = intersect_polygons(&subject.polygons, &cutter.polygons);
    if inside.is_empty() {
        return vec![subject];
    }
    let outside = subtract_polygons(&subject.polygons, &cutter.polygons);
    let mut pieces = Vec::with_capacity(3);
    if !outside.is_empty() {
        let mut rem = subject.clone();
        rem.polygons = outside;
        pieces.push(rem);
    }
    if c_low > s_low {
        let mut below = subject.clone();
        below.upper = cutter.lower;
        below.polygons = inside.clone();
        pieces.push(below);
    }
    if c_high < s_high {
        let mut above = subject;
        above.lower = cutter.upper;
        above.polygons = inside;
        pieces.push(above);
    }
    pieces
}

fn classify_row(
    class_raw: &str,
    local_raw: &str,
) -> Option<(ControlledAirspaceClass, ControlledLocalType)> {
    match class_raw {
        "B" => Some((ControlledAirspaceClass::ClassB, ControlledLocalType::ClassB)),
        "C" => Some((ControlledAirspaceClass::ClassC, ControlledLocalType::ClassC)),
        "D" => Some((ControlledAirspaceClass::ClassD, ControlledLocalType::ClassD)),
        "E" => {
            let lt = match local_raw {
                "CLASS_E2" => ControlledLocalType::ClassE2Surface,
                "CLASS_E3" => ControlledLocalType::ClassE3Extension,
                "CLASS_E4" => ControlledLocalType::ClassE4Extension,
                "CLASS_E5" => ControlledLocalType::ClassE5Transition,
                "CLASS_E6" => ControlledLocalType::ClassE6Enroute,
                _ => ControlledLocalType::ClassEOther,
            };
            Some((ControlledAirspaceClass::ClassE, lt))
        }
        _ => None,
    }
}

fn parse_lower_limit(val_raw: &str, uom_raw: &str, code_raw: &str) -> VerticalLimit {
    if val_raw.eq_ignore_ascii_case("GND") || val_raw.eq_ignore_ascii_case("SFC") {
        return VerticalLimit::SURFACE;
    }
    let parsed_ft = parse_altitude_number(val_raw).unwrap_or(0);
    if code_raw.eq_ignore_ascii_case("SFC") || code_raw.eq_ignore_ascii_case("AGL") {
        VerticalLimit::agl_ft(parsed_ft)
    } else if uom_raw.eq_ignore_ascii_case("FL") || code_raw.eq_ignore_ascii_case("STD") {
        let fl = u16::try_from(parsed_ft.max(0)).unwrap_or(180);
        VerticalLimit::flight_level(fl)
    } else {
        VerticalLimit::msl_ft(parsed_ft.max(0))
    }
}

fn parse_upper_limit(
    local_type: ControlledLocalType,
    lower: VerticalLimit,
    val_raw: &str,
    uom_raw: &str,
    code_raw: &str,
    desc_raw: &str,
) -> VerticalLimit {
    if val_raw == "-9998" || desc_raw == "AA" {
        if matches!(
            local_type,
            ControlledLocalType::ClassE2Surface
                | ControlledLocalType::ClassE3Extension
                | ControlledLocalType::ClassE4Extension
        ) && lower.value_ft == 0
        {
            // Per 14 CFR § 71.71(b)-(d) and FAA JO 7400.11 ¶6002-6004, surface Class E2/E3/E4
            // without an explicit MSL ceiling extends from SFC up to the 700 ft AGL E5 floor.
            return VerticalLimit::agl_ft(700);
        }
        // Controlled airspace extending up to (but not including) 18,000 ft MSL (base of Class A).
        return VerticalLimit::msl_ft(18_000);
    }
    if val_raw == "-9999" || val_raw.eq_ignore_ascii_case("UNL") {
        return VerticalLimit::unlimited(60_000);
    }
    let parsed_ft = parse_altitude_number(val_raw).unwrap_or(18_000);
    if code_raw.eq_ignore_ascii_case("SFC") || code_raw.eq_ignore_ascii_case("AGL") {
        VerticalLimit::agl_ft(parsed_ft)
    } else if uom_raw.eq_ignore_ascii_case("FL") || code_raw.eq_ignore_ascii_case("STD") {
        let fl = u16::try_from(parsed_ft.max(0)).unwrap_or(180);
        VerticalLimit::flight_level(fl)
    } else {
        VerticalLimit::msl_ft(parsed_ft.max(0))
    }
}

fn parse_altitude_number(raw: &str) -> Option<i32> {
    let trimmed = raw.trim();
    if let Ok(int_val) = trimmed.parse::<i32>() {
        return Some(int_val);
    }
    trimmed.parse::<f64>().ok().map(|f| f.round() as i32)
}

fn read_zip_entry<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    entry_name: &str,
) -> Result<Vec<u8>, NasrCompileError> {
    let mut entry = archive
        .by_name(entry_name)
        .map_err(|source| NasrCompileError::Zip {
            context: entry_name.to_owned(),
            source,
        })?;
    let cap = usize::try_from(entry.size()).unwrap_or(0);
    let mut buf = Vec::with_capacity(cap);
    entry
        .read_to_end(&mut buf)
        .map_err(|err| NasrCompileError::Shapefile(format!("reading {entry_name}: {err}")))?;
    Ok(buf)
}

fn parse_dbf_header(dbf: &[u8]) -> Result<(usize, usize, usize, Vec<DbfField>), NasrCompileError> {
    let num_records = read_u32_le(dbf, 4)
        .ok_or_else(|| NasrCompileError::Shapefile("DBF header too short".to_owned()))?
        as usize;
    let header_len = usize::from(
        read_u16_le(dbf, 8)
            .ok_or_else(|| NasrCompileError::Shapefile("DBF header_len missing".to_owned()))?,
    );
    let record_len = usize::from(
        read_u16_le(dbf, 10)
            .ok_or_else(|| NasrCompileError::Shapefile("DBF record_len missing".to_owned()))?,
    );
    let mut fields = Vec::new();
    let mut pos: usize = 32;
    while pos.saturating_add(32) <= header_len {
        if dbf.get(pos).copied() == Some(0x0D) {
            break;
        }
        let Some(name_slice) = dbf.get(pos..pos + 11) else {
            break;
        };
        let end = name_slice
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_slice.len());
        let name = name_slice
            .get(..end)
            .map(|s| String::from_utf8_lossy(s).trim().to_ascii_uppercase())
            .unwrap_or_default();
        let len = usize::from(dbf.get(pos + 16).copied().unwrap_or(0));
        fields.push(DbfField { name, len });
        pos = pos.saturating_add(32);
    }
    Ok((num_records, header_len, record_len, fields))
}

fn parse_dbf_row<'a>(rec: &'a [u8], fields: &[DbfField]) -> BTreeMap<String, &'a str> {
    let mut out = BTreeMap::new();
    let mut offset: usize = 1;
    for field in fields {
        let next = offset.saturating_add(field.len);
        if let Some(raw_bytes) = rec.get(offset..next)
            && let Ok(s) = std::str::from_utf8(raw_bytes)
        {
            out.insert(field.name.clone(), s.trim());
        }
        offset = next;
    }
    out
}

fn attr_str<'a>(map: &'a BTreeMap<String, &'a str>, key: &str) -> &'a str {
    map.get(key).copied().unwrap_or("")
}

fn non_empty(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

type ShpPolygonRecord = (GeoBbox, Vec<Vec<[f64; 2]>>);

fn read_shp_polygon(
    shx: &[u8],
    shp: &[u8],
    record_idx: usize,
) -> Result<Option<ShpPolygonRecord>, NasrCompileError> {
    let shx_off = 100_usize.saturating_add(record_idx.saturating_mul(8));
    let offset_words = read_u32_be(shx, shx_off)
        .ok_or_else(|| NasrCompileError::Shapefile(format!("SHX index {record_idx} missing")))?;
    let content_off = (offset_words as usize).saturating_mul(2).saturating_add(8);
    let shape_type = read_u32_le(shp, content_off)
        .ok_or_else(|| NasrCompileError::Shapefile(format!("SHP record {record_idx} truncated")))?;
    if shape_type == 0 {
        return Ok(None);
    }
    // ESRI Polygon (5), PolygonZ (15), PolygonM (25) all share the identical 2D header & XY layout.
    if !matches!(shape_type, 5 | 15 | 25) {
        return Err(NasrCompileError::Shapefile(format!(
            "unexpected ESRI shape_type {shape_type} at record {record_idx}"
        )));
    }
    let min_lon = read_f64_le(shp, content_off + 4).unwrap_or(0.0);
    let min_lat = read_f64_le(shp, content_off + 12).unwrap_or(0.0);
    let max_lon = read_f64_le(shp, content_off + 20).unwrap_or(0.0);
    let max_lat = read_f64_le(shp, content_off + 28).unwrap_or(0.0);
    let num_parts = read_u32_le(shp, content_off + 36).unwrap_or(0) as usize;
    let num_points = read_u32_le(shp, content_off + 40).unwrap_or(0) as usize;
    if num_parts == 0 || num_points < 3 {
        return Ok(None);
    }

    let parts_off = content_off + 44;
    let mut part_starts = Vec::with_capacity(num_parts + 1);
    for p_idx in 0..num_parts {
        let start = read_u32_le(shp, parts_off + p_idx * 4).ok_or_else(|| {
            NasrCompileError::Shapefile(format!("SHP parts table truncated at {record_idx}"))
        })? as usize;
        part_starts.push(start);
    }
    part_starts.push(num_points);

    let pts_off = parts_off.saturating_add(num_parts.saturating_mul(4));
    let mut rings = Vec::with_capacity(num_parts);
    for window in part_starts.windows(2) {
        let Some(&start) = window.first() else {
            continue;
        };
        let Some(&end) = window.get(1) else {
            continue;
        };
        if end <= start {
            continue;
        }
        let mut ring = Vec::with_capacity(end.saturating_sub(start));
        for pt_idx in start..end {
            let xy_off = pts_off.saturating_add(pt_idx.saturating_mul(16));
            let Some(lon) = read_f64_le(shp, xy_off) else {
                return Err(NasrCompileError::Shapefile(format!(
                    "SHP point {pt_idx} truncated at record {record_idx}"
                )));
            };
            let Some(lat) = read_f64_le(shp, xy_off + 8) else {
                return Err(NasrCompileError::Shapefile(format!(
                    "SHP point {pt_idx} truncated at record {record_idx}"
                )));
            };
            ring.push([lon, lat]);
        }
        rings.push(ring);
    }

    Ok(Some((
        GeoBbox {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        },
        rings,
    )))
}

fn read_u16_le(buf: &[u8], offset: usize) -> Option<u16> {
    let bytes: [u8; 2] = buf.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn read_u32_le(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_u32_be(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn read_f64_le(buf: &[u8], offset: usize) -> Option<f64> {
    let bytes: [u8; 8] = buf.get(offset..offset + 8)?.try_into().ok()?;
    Some(f64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_poly(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> Vec<AirspacePolygon> {
        normalize_raw_rings(&[vec![
            [min_lon, min_lat],
            [max_lon, min_lat],
            [max_lon, max_lat],
            [min_lon, max_lat],
        ]])
    }

    #[test]
    fn normalizes_wedding_cake_donuts_and_vfr_corridor_exclusions() {
        // Outer shelf 3000..10000 covering [-122.6..-122.0] overlaps inner core 0..10000 [-122.4..-122.2],
        // plus an EXCLUSION='1' VFR corridor split into 0..3300 and 4700..10000 inside [-122.35..-122.25].
        let core = RawClassShelf {
            ident: "SAN".into(),
            name: "SAN DIEGO CLASS B".into(),
            sector: Some("AREA A".into()),
            class: ControlledAirspaceClass::ClassB,
            local_type: ControlledLocalType::ClassB,
            lower: VerticalLimit::SURFACE,
            upper: VerticalLimit::msl_ft(10_000),
            hours: None,
            comm_name: None,
            exclusion: false,
            polygons: rect_poly(-122.4, 32.6, -122.2, 32.8),
        };
        let outer = RawClassShelf {
            ident: "SAN".into(),
            name: "SAN DIEGO CLASS B".into(),
            sector: Some("AREA B".into()),
            class: ControlledAirspaceClass::ClassB,
            local_type: ControlledLocalType::ClassB,
            lower: VerticalLimit::msl_ft(3_000),
            upper: VerticalLimit::msl_ft(10_000),
            hours: None,
            comm_name: None,
            exclusion: false,
            polygons: rect_poly(-122.6, 32.5, -122.0, 32.9),
        };
        let corr_low = RawClassShelf {
            ident: "SAN".into(),
            name: "SAN DIEGO CLASS B".into(),
            sector: Some("VFR CORRIDOR".into()),
            class: ControlledAirspaceClass::ClassB,
            local_type: ControlledLocalType::ClassB,
            lower: VerticalLimit::SURFACE,
            upper: VerticalLimit::msl_ft(3_300),
            hours: None,
            comm_name: None,
            exclusion: true,
            polygons: rect_poly(-122.35, 32.65, -122.25, 32.75),
        };
        let corr_high = RawClassShelf {
            lower: VerticalLimit::msl_ft(4_700),
            upper: VerticalLimit::msl_ft(10_000),
            ..corr_low.clone()
        };

        let result = normalize_complex_shelves(vec![core, outer, corr_low, corr_high]);
        assert_eq!(result.len(), 4);

        // Outer shelf should now have an interior cutout where the full-height core sits!
        let norm_outer = result
            .iter()
            .find(|s| s.sector.as_deref() == Some("AREA B"));
        assert!(norm_outer.is_some_and(|s| s.polygons.iter().any(|p| !p.interiors.is_empty())));

        // Full-height core should have an interior cutout where the VFR corridor sits!
        let norm_core = result
            .iter()
            .find(|s| s.sector.as_deref() == Some("AREA A"));
        assert!(norm_core.is_some_and(|s| s.polygons.iter().any(|p| !p.interiors.is_empty())));
    }

    #[test]
    fn precedence_clips_class_d_beneath_class_b_and_deduplicates_part_time_e2() {
        let e2_ceil = parse_upper_limit(
            ControlledLocalType::ClassE2Surface,
            VerticalLimit::SURFACE,
            "-9998",
            "",
            "",
            "AA",
        );
        assert_eq!(e2_ceil, VerticalLimit::agl_ft(700));
        let e5_ceil = parse_upper_limit(
            ControlledLocalType::ClassE5Transition,
            VerticalLimit::agl_ft(700),
            "-9998",
            "",
            "",
            "AA",
        );
        assert_eq!(e5_ceil, VerticalLimit::msl_ft(18_000));

        // Class B: core 0..10000 on [-122.4..-122.3], shelf 1100..10000 on [-122.3..-122.2].
        let sea_a = RawClassShelf {
            ident: "SEA".into(),
            name: "SEATTLE CLASS B".into(),
            sector: Some("AREA A".into()),
            class: ControlledAirspaceClass::ClassB,
            local_type: ControlledLocalType::ClassB,
            lower: VerticalLimit::SURFACE,
            upper: VerticalLimit::msl_ft(10_000),
            hours: None,
            comm_name: None,
            exclusion: false,
            polygons: rect_poly(-122.4, 47.4, -122.3, 47.6),
        };
        let sea_b = RawClassShelf {
            sector: Some("AREA B".into()),
            lower: VerticalLimit::msl_ft(1_100),
            polygons: rect_poly(-122.3, 47.4, -122.2, 47.6),
            ..sea_a.clone()
        };
        // Boeing Field Class D 0..2500 spanning across AREA A, AREA B, and outside [-122.35..-122.15].
        let bfi_d = RawClassShelf {
            ident: "BFI".into(),
            name: "BOEING FIELD CLASS D".into(),
            sector: None,
            class: ControlledAirspaceClass::ClassD,
            local_type: ControlledLocalType::ClassD,
            lower: VerticalLimit::SURFACE,
            upper: VerticalLimit::msl_ft(2_500),
            hours: None,
            comm_name: None,
            exclusion: false,
            polygons: rect_poly(-122.35, 47.45, -122.15, 47.55),
        };
        // Part-time Tacoma Narrows Class D (0..2800) + identical Class E2 (0..700 AGL) + E4 tab.
        let tiw_d = RawClassShelf {
            ident: "TIW".into(),
            name: "TACOMA NARROWS CLASS D".into(),
            sector: None,
            class: ControlledAirspaceClass::ClassD,
            local_type: ControlledLocalType::ClassD,
            lower: VerticalLimit::SURFACE,
            upper: VerticalLimit::msl_ft(2_800),
            hours: None,
            comm_name: None,
            exclusion: false,
            polygons: rect_poly(-122.65, 47.2, -122.55, 47.3),
        };
        let tiw_e2 = RawClassShelf {
            class: ControlledAirspaceClass::ClassE,
            local_type: ControlledLocalType::ClassE2Surface,
            upper: VerticalLimit::agl_ft(700),
            ..tiw_d.clone()
        };
        let tiw_e4 = RawClassShelf {
            class: ControlledAirspaceClass::ClassE,
            local_type: ControlledLocalType::ClassE4Extension,
            upper: VerticalLimit::agl_ft(700),
            polygons: rect_poly(-122.60, 47.22, -122.50, 47.28),
            ..tiw_d.clone()
        };

        let resolved =
            apply_regulatory_precedence(vec![sea_a, sea_b, bfi_d, tiw_d, tiw_e2, tiw_e4]);
        let bfi_ceilings: Vec<i32> = resolved
            .iter()
            .filter(|s| s.ident == "BFI")
            .map(|s| s.upper.value_ft)
            .collect();
        // BFI under AREA A disappeared; BFI under AREA B clipped to 1100; BFI east of AREA B stayed 2500.
        assert_eq!(bfi_ceilings.len(), 2);
        assert!(bfi_ceilings.contains(&1_100));
        assert!(bfi_ceilings.contains(&2_500));
        // Duplicate TIW E2 vanished completely; TIW E4 retained only its approach tab outside TIW D.
        assert!(
            !resolved
                .iter()
                .any(|s| s.ident == "TIW" && s.local_type == ControlledLocalType::ClassE2Surface)
        );
        assert!(
            resolved
                .iter()
                .any(|s| s.ident == "TIW" && s.local_type == ControlledLocalType::ClassE4Extension)
        );
    }
}
