//! Pure-Rust Mapbox Vector Tile 2.1 encoder (`Polygons` and `Lines`).
//!
//! Follows the MVT 2.1 specification:
//! - Polygon exterior rings are wound **clockwise** in tile-local coordinates (`x` right, `y` down,
//!   signed shoelace area `> 0`), and interior hole rings are wound **counter-clockwise** (`area < 0`).
//! - Frequently repeated attribute values receive the smallest dictionary indices for compact tiles.

use std::collections::BTreeMap;

/// A single polygon component (`exterior` + `interiors`) in integer tile coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TilePolygon {
    /// Exterior ring vertices (without duplicate closing vertex; `len >= 3`).
    pub exterior: Vec<(i32, i32)>,
    /// Optional interior hole rings (each without duplicate closing vertex; `len >= 3`).
    pub interiors: Vec<Vec<(i32, i32)>>,
}

/// Geometry payload of an [`EncodedFeature`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureGeometry {
    /// Multi-polygon payload (`geom_type = 3`).
    Polygons(Vec<TilePolygon>),
    /// Multi-linestring payload (`geom_type = 2`).
    Lines(Vec<Vec<(i32, i32)>>),
}

/// Typed MVT property value.
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    /// UTF-8 string value.
    String(String),
    /// Floating-point value (stored as `float` when lossless, else `double`).
    Double(f64),
    /// Signed integer value.
    Int(i64),
    /// Boolean value.
    Bool(bool),
}

/// One feature ready for MVT 2.1 layer serialization.
#[derive(Clone, Debug)]
pub struct EncodedFeature {
    /// Stable feature ID (`>= 1`).
    pub id: u64,
    /// Ordered key-value attributes.
    pub attributes: Vec<(String, AttributeValue)>,
    /// Tile-local geometry.
    pub geometry: FeatureGeometry,
}

/// Named MVT layer ready for tile encoding.
pub struct MvtLayerInput {
    /// Layer name (e.g. `"airspace"`, `"airspace_boundary"`).
    pub name: String,
    /// Tile coordinate extent (`4096`).
    pub extent: u32,
    /// Encoded features in draw/query order.
    pub features: Vec<EncodedFeature>,
}

/// Encodes one complete Mapbox Vector Tile `.mvt` protobuf byte stream from `layers`.
pub fn encode_tile(layers: &[MvtLayerInput]) -> Vec<u8> {
    let mut tile = Vec::new();
    for layer in layers {
        if layer.features.is_empty() {
            continue;
        }
        let encoded_layer = encode_layer(&layer.name, layer.extent, &layer.features);
        if encoded_layer.is_empty() {
            continue;
        }
        write_tag(&mut tile, 3, WireType::Length);
        write_varint(&mut tile, encoded_layer.len() as u64);
        tile.extend_from_slice(&encoded_layer);
    }
    tile
}

#[derive(Clone, Copy)]
enum WireType {
    Varint,
    Length,
}

fn write_tag(buf: &mut Vec<u8>, field: u32, wire: WireType) {
    let wire_bits = match wire {
        WireType::Varint => 0,
        WireType::Length => 2,
    };
    write_varint(buf, u64::from((field << 3) | wire_bits));
}

fn write_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn zigzag(value: i32) -> u64 {
    u64::from(((value << 1) ^ (value >> 31)) as u32)
}

fn write_string(buf: &mut Vec<u8>, field: u32, text: &str) {
    write_tag(buf, field, WireType::Length);
    write_varint(buf, text.len() as u64);
    buf.extend_from_slice(text.as_bytes());
}

fn encode_value(value: &AttributeValue) -> Vec<u8> {
    let mut buf = Vec::new();
    match value {
        AttributeValue::String(s) => write_string(&mut buf, 1, s),
        AttributeValue::Double(num) => {
            let as_f32 = *num as f32;
            if num.is_finite() && f64::from(as_f32).to_bits() == num.to_bits() {
                write_varint(&mut buf, u64::from((2_u32 << 3) | 5));
                buf.extend_from_slice(&as_f32.to_le_bytes());
            } else {
                write_varint(&mut buf, u64::from((3_u32 << 3) | 1));
                buf.extend_from_slice(&num.to_le_bytes());
            }
        }
        AttributeValue::Int(n) if *n < 0 => {
            write_tag(&mut buf, 6, WireType::Varint);
            write_varint(&mut buf, ((*n as u64) << 1) ^ ((*n >> 63) as u64));
        }
        AttributeValue::Int(n) => {
            write_tag(&mut buf, 4, WireType::Varint);
            write_varint(&mut buf, *n as u64);
        }
        AttributeValue::Bool(flag) => {
            write_tag(&mut buf, 7, WireType::Varint);
            write_varint(&mut buf, u64::from(*flag));
        }
    }
    buf
}

fn encode_layer(name: &str, extent: u32, features: &[EncodedFeature]) -> Vec<u8> {
    let mut keys: Vec<String> = Vec::new();
    let mut key_map: BTreeMap<String, u32> = BTreeMap::new();
    let (values, val_map) = build_value_dictionary(features);

    let mut encoded_features = Vec::with_capacity(features.len());
    for feat in features {
        let Some((geom_type_id, commands)) = encode_geometry(&feat.geometry) else {
            continue;
        };
        let mut tags = Vec::with_capacity(feat.attributes.len() * 2);
        for (k, v) in &feat.attributes {
            let k_idx = *key_map.entry(k.clone()).or_insert_with(|| {
                let idx = keys.len() as u32;
                keys.push(k.clone());
                idx
            });
            let v_bytes = encode_value(v);
            let Some(&v_idx) = val_map.get(&v_bytes) else {
                continue;
            };
            tags.push(u64::from(k_idx));
            tags.push(u64::from(v_idx));
        }
        encoded_features.push(serialize_feature(feat.id, geom_type_id, &tags, &commands));
    }

    if encoded_features.is_empty() {
        return Vec::new();
    }

    let mut layer = Vec::new();
    write_tag(&mut layer, 15, WireType::Varint);
    write_varint(&mut layer, 2);
    write_string(&mut layer, 1, name);
    for feat_bytes in &encoded_features {
        write_tag(&mut layer, 2, WireType::Length);
        write_varint(&mut layer, feat_bytes.len() as u64);
        layer.extend_from_slice(feat_bytes);
    }
    for k in &keys {
        write_string(&mut layer, 3, k);
    }
    for v in &values {
        write_tag(&mut layer, 4, WireType::Length);
        write_varint(&mut layer, v.len() as u64);
        layer.extend_from_slice(v);
    }
    write_tag(&mut layer, 5, WireType::Varint);
    write_varint(&mut layer, u64::from(extent));
    layer
}

fn build_value_dictionary(features: &[EncodedFeature]) -> (Vec<Vec<u8>>, BTreeMap<Vec<u8>, u32>) {
    let mut freq = BTreeMap::<Vec<u8>, usize>::new();
    for feat in features {
        for (_, val) in &feat.attributes {
            let count = freq.entry(encode_value(val)).or_default();
            *count = count.saturating_add(1);
        }
    }
    let mut entries: Vec<(Vec<u8>, usize)> = freq.into_iter().collect();
    entries.sort_by(|(l_bytes, l_cnt), (r_bytes, r_cnt)| {
        r_cnt.cmp(l_cnt).then_with(|| l_bytes.cmp(r_bytes))
    });
    let values: Vec<Vec<u8>> = entries.into_iter().map(|(b, _)| b).collect();
    let index = values
        .iter()
        .enumerate()
        .map(|(i, b)| (b.clone(), i as u32))
        .collect();
    (values, index)
}

fn encode_geometry(geom: &FeatureGeometry) -> Option<(u64, Vec<u64>)> {
    let mut cmds = Vec::new();
    let (mut cx, mut cy) = (0_i32, 0_i32);

    match geom {
        FeatureGeometry::Lines(parts) => {
            for part in parts {
                let dedup = dedup_consecutive_line(part);
                let Some((&(fx, fy), rest)) = dedup.split_first() else {
                    continue;
                };
                if rest.is_empty() {
                    continue;
                }
                cmds.push(u64::from(1_u32 | (1_u32 << 3)));
                cmds.push(zigzag(fx - cx));
                cmds.push(zigzag(fy - cy));
                (cx, cy) = (fx, fy);

                cmds.push(u64::from(2_u32 | ((rest.len() as u32) << 3)));
                for &(px, py) in rest {
                    cmds.push(zigzag(px - cx));
                    cmds.push(zigzag(py - cy));
                    (cx, cy) = (px, py);
                }
            }
            if cmds.is_empty() {
                None
            } else {
                Some((2, cmds))
            }
        }
        FeatureGeometry::Polygons(polys) => {
            for poly in polys {
                let Some(ext) = orient_ring(&poly.exterior, true) else {
                    continue;
                };
                emit_ring(&ext, &mut cx, &mut cy, &mut cmds);
                for hole in &poly.interiors {
                    if let Some(oriented_hole) = orient_ring(hole, false) {
                        emit_ring(&oriented_hole, &mut cx, &mut cy, &mut cmds);
                    }
                }
            }
            if cmds.is_empty() {
                None
            } else {
                Some((3, cmds))
            }
        }
    }
}

fn emit_ring(ring: &[(i32, i32)], cx: &mut i32, cy: &mut i32, cmds: &mut Vec<u64>) {
    let Some((&(fx, fy), rest)) = ring.split_first() else {
        return;
    };
    if rest.len() < 2 {
        return;
    }
    // MoveTo(1)
    cmds.push(u64::from(1_u32 | (1_u32 << 3)));
    cmds.push(zigzag(fx - *cx));
    cmds.push(zigzag(fy - *cy));
    *cx = fx;
    *cy = fy;

    // LineTo(n - 1)
    cmds.push(u64::from(2_u32 | ((rest.len() as u32) << 3)));
    for &(px, py) in rest {
        cmds.push(zigzag(px - *cx));
        cmds.push(zigzag(py - *cy));
        *cx = px;
        *cy = py;
    }

    // ClosePath(1)
    cmds.push(u64::from(7_u32 | (1_u32 << 3)));
}

/// Ensures `ring` has no duplicate adjacent/closing vertices, non-zero area, and MVT winding
/// (`clockwise = true` -> signed shoelace area `> 0` in tile-down coordinates).
fn orient_ring(raw: &[(i32, i32)], clockwise: bool) -> Option<Vec<(i32, i32)>> {
    let mut ring = dedup_consecutive_line(raw);
    if let (Some(&first), Some(&last)) = (ring.first(), ring.last())
        && ring.len() > 3
        && first == last
    {
        ring.pop();
    }
    if ring.len() < 3 {
        return None;
    }
    let area2 = signed_twice_area(&ring);
    if area2 == 0 {
        return None;
    }
    let is_cw = area2 > 0;
    if is_cw != clockwise {
        ring.reverse();
    }
    Some(ring)
}

fn signed_twice_area(ring: &[(i32, i32)]) -> i64 {
    let Some(&(mut px, mut py)) = ring.last() else {
        return 0;
    };
    let mut sum: i64 = 0;
    for &(qx, qy) in ring {
        sum += i64::from(px) * i64::from(qy) - i64::from(qx) * i64::from(py);
        px = qx;
        py = qy;
    }
    sum
}

fn dedup_consecutive_line(pts: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut out = Vec::with_capacity(pts.len());
    for &pt in pts {
        if out.last().copied() != Some(pt) {
            out.push(pt);
        }
    }
    out
}

fn serialize_feature(id: u64, geom_type: u64, tags: &[u64], commands: &[u64]) -> Vec<u8> {
    let mut buf = Vec::new();
    write_tag(&mut buf, 1, WireType::Varint);
    write_varint(&mut buf, id);

    write_tag(&mut buf, 2, WireType::Length);
    let mut packed_tags = Vec::new();
    for &t in tags {
        write_varint(&mut packed_tags, t);
    }
    write_varint(&mut buf, packed_tags.len() as u64);
    buf.extend_from_slice(&packed_tags);

    write_tag(&mut buf, 3, WireType::Varint);
    write_varint(&mut buf, geom_type);

    write_tag(&mut buf, 4, WireType::Length);
    let mut packed_geom = Vec::new();
    for &cmd in commands {
        write_varint(&mut packed_geom, cmd);
    }
    write_varint(&mut buf, packed_geom.len() as u64);
    buf.extend_from_slice(&packed_geom);
    buf
}
