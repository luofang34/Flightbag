//! Premultiplied-alpha `2x2 -> 1` overview pyramid synthesizer for raster chart `PMTiles`.
//!
//! FAA AeroNav publishes `VFR_Sectional` Web Mercator tiles starting at `z8`, while `IFR_High`
//! and `IFR_AreaLow` publish oceanic/Alaska sheets at `z5..6` and continental `H`/`L` sheets
//! starting at `z7..8`. Stepping down from `base_zoom` (`z8`) to `z0` while preserving opaque
//! native parent tiles and alpha-underfilling transparent border parent tiles produces a
//! seamless whole-globe mosaic across all 50 states, Pacific/Atlantic routes, and the Caribbean.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

use image::{ImageFormat, Rgba, RgbaImage};

use crate::ArchiveError;

const TILE_DIM: u32 = 256;
const HALF_DIM: u32 = 128;

/// Synthesizes missing or partially transparent parent overview PNG tiles from `base_zoom` down
/// to `target_min_zoom` (`target_min_zoom < base_zoom`) using premultiplied-alpha `2x2` box
/// downsampling, inserting generated PNG payloads into `tiles` and returning the number of
/// newly inserted or alpha-underfilled tiles.
///
/// # Errors
///
/// Returns [`ArchiveError::Json`] if an encoded overview PNG cannot be serialized in memory.
pub fn synthesize_overview_pyramid(
    tiles: &mut BTreeMap<(u8, u32, u32), Vec<u8>>,
    base_zoom: u8,
    target_min_zoom: u8,
) -> Result<usize, ArchiveError> {
    if base_zoom == 0 || target_min_zoom >= base_zoom {
        return Ok(0);
    }

    let mut current_images = decode_zoom_level(tiles, base_zoom);
    let mut synthesized_count: usize = 0;
    let mut child_zoom = base_zoom;

    while child_zoom > target_min_zoom {
        let parent_zoom = child_zoom - 1;
        let mut native_parents = decode_zoom_level(tiles, parent_zoom);
        if current_images.is_empty() && native_parents.is_empty() {
            break;
        }

        let mut parent_coords = BTreeSet::new();
        for &(cx, cy) in current_images.keys() {
            parent_coords.insert((cx / 2, cy / 2));
        }

        let mut next_images: BTreeMap<(u32, u32), RgbaImage> = BTreeMap::new();
        for (px, py) in parent_coords {
            let Some(downsampled) = downsample_children(&current_images, px, py) else {
                continue;
            };
            if let Some(native_img) = native_parents.remove(&(px, py)) {
                if needs_underfill(&native_img, &downsampled) {
                    let merged = composite_over(&native_img, &downsampled);
                    tiles.insert((parent_zoom, px, py), encode_rgba_png(&merged)?);
                    synthesized_count = synthesized_count.wrapping_add(1);
                    next_images.insert((px, py), merged);
                } else {
                    next_images.insert((px, py), native_img);
                }
            } else {
                tiles.insert((parent_zoom, px, py), encode_rgba_png(&downsampled)?);
                synthesized_count = synthesized_count.wrapping_add(1);
                next_images.insert((px, py), downsampled);
            }
        }

        for (coord, native_img) in native_parents {
            next_images.insert(coord, native_img);
        }

        current_images = next_images;
        child_zoom = parent_zoom;
    }

    Ok(synthesized_count)
}

fn decode_zoom_level(
    tiles: &BTreeMap<(u8, u32, u32), Vec<u8>>,
    zoom: u8,
) -> BTreeMap<(u32, u32), RgbaImage> {
    let mut images = BTreeMap::new();
    for (&(z, x, y), bytes) in tiles.range((zoom, 0, 0)..=(zoom, u32::MAX, u32::MAX)) {
        if z == zoom
            && let Ok(decoded) = image::load_from_memory(bytes)
        {
            let rgba = decoded.to_rgba8();
            if rgba.width() == TILE_DIM && rgba.height() == TILE_DIM {
                images.insert((x, y), rgba);
            }
        }
    }
    images
}

fn downsample_children(
    current_images: &BTreeMap<(u32, u32), RgbaImage>,
    px: u32,
    py: u32,
) -> Option<RgbaImage> {
    let mut canvas = RgbaImage::from_pixel(TILE_DIM, TILE_DIM, Rgba([0, 0, 0, 0]));
    let mut any_child = false;
    for dy in 0_u32..2 {
        for dx in 0_u32..2 {
            let cx = px.saturating_mul(2).saturating_add(dx);
            let cy = py.saturating_mul(2).saturating_add(dy);
            if let Some(child_img) = current_images.get(&(cx, cy)) {
                blit_downsampled_quadrant(
                    child_img,
                    &mut canvas,
                    dx.saturating_mul(HALF_DIM),
                    dy.saturating_mul(HALF_DIM),
                );
                any_child = true;
            }
        }
    }
    any_child.then_some(canvas)
}

fn needs_underfill(fg: &RgbaImage, bg: &RgbaImage) -> bool {
    for y in 0..TILE_DIM {
        for x in 0..TILE_DIM {
            let Rgba([_, _, _, fg_a]) = *fg.get_pixel(x, y);
            let Rgba([_, _, _, bg_a]) = *bg.get_pixel(x, y);
            if fg_a < 255 && bg_a > 0 {
                return true;
            }
        }
    }
    false
}

fn composite_over(fg: &RgbaImage, bg: &RgbaImage) -> RgbaImage {
    let mut out = RgbaImage::new(TILE_DIM, TILE_DIM);
    for y in 0..TILE_DIM {
        for x in 0..TILE_DIM {
            let Rgba([fr, fg_c, fb, fa]) = *fg.get_pixel(x, y);
            if fa == 255 {
                out.put_pixel(x, y, Rgba([fr, fg_c, fb, 255]));
                continue;
            }
            let Rgba([br, bg_c, bb, ba]) = *bg.get_pixel(x, y);
            if fa == 0 {
                out.put_pixel(x, y, Rgba([br, bg_c, bb, ba]));
                continue;
            }
            let fa_u = u32::from(fa);
            let ba_u = u32::from(ba);
            let inv_fa = 255_u32.saturating_sub(fa_u);
            let out_a_num = fa_u
                .saturating_mul(255)
                .saturating_add(ba_u.saturating_mul(inv_fa));
            if out_a_num == 0 {
                out.put_pixel(x, y, Rgba([0, 0, 0, 0]));
                continue;
            }
            let r_num = u32::from(fr)
                .saturating_mul(fa_u)
                .saturating_mul(255)
                .saturating_add(u32::from(br).saturating_mul(ba_u).saturating_mul(inv_fa));
            let g_num = u32::from(fg_c)
                .saturating_mul(fa_u)
                .saturating_mul(255)
                .saturating_add(u32::from(bg_c).saturating_mul(ba_u).saturating_mul(inv_fa));
            let b_num = u32::from(fb)
                .saturating_mul(fa_u)
                .saturating_mul(255)
                .saturating_add(u32::from(bb).saturating_mul(ba_u).saturating_mul(inv_fa));
            let out_r = ((r_num + out_a_num / 2) / out_a_num).min(255) as u8;
            let out_g = ((g_num + out_a_num / 2) / out_a_num).min(255) as u8;
            let out_b = ((b_num + out_a_num / 2) / out_a_num).min(255) as u8;
            let out_a = ((out_a_num + 127) / 255).min(255) as u8;
            out.put_pixel(x, y, Rgba([out_r, out_g, out_b, out_a]));
        }
    }
    out
}

fn blit_downsampled_quadrant(src: &RgbaImage, dst: &mut RgbaImage, ox: u32, oy: u32) {
    for y in 0..HALF_DIM {
        let sy = y.saturating_mul(2);
        for x in 0..HALF_DIM {
            let sx = x.saturating_mul(2);
            let p00 = *src.get_pixel(sx, sy);
            let p10 = *src.get_pixel(sx + 1, sy);
            let p01 = *src.get_pixel(sx, sy + 1);
            let p11 = *src.get_pixel(sx + 1, sy + 1);
            dst.put_pixel(ox + x, oy + y, average_premultiplied([p00, p10, p01, p11]));
        }
    }
}

fn average_premultiplied(samples: [Rgba<u8>; 4]) -> Rgba<u8> {
    let mut sum_r: u32 = 0;
    let mut sum_g: u32 = 0;
    let mut sum_b: u32 = 0;
    let mut sum_a: u32 = 0;
    for Rgba([r, g, b, a]) in samples {
        let alpha = u32::from(a);
        sum_r = sum_r.saturating_add(u32::from(r).saturating_mul(alpha));
        sum_g = sum_g.saturating_add(u32::from(g).saturating_mul(alpha));
        sum_b = sum_b.saturating_add(u32::from(b).saturating_mul(alpha));
        sum_a = sum_a.saturating_add(alpha);
    }
    if sum_a == 0 {
        return Rgba([0, 0, 0, 0]);
    }
    let out_a = ((sum_a + 2) / 4).min(255) as u8;
    let out_r = ((sum_r + sum_a / 2) / sum_a).min(255) as u8;
    let out_g = ((sum_g + sum_a / 2) / sum_a).min(255) as u8;
    let out_b = ((sum_b + sum_a / 2) / sum_b_denom(sum_a)).min(255) as u8;
    Rgba([out_r, out_g, out_b, out_a])
}

const fn sum_b_denom(sum_a: u32) -> u32 {
    if sum_a == 0 { 1 } else { sum_a }
}

fn encode_rgba_png(img: &RgbaImage) -> Result<Vec<u8>, ArchiveError> {
    let mut cursor = Cursor::new(Vec::with_capacity(24 * 1024));
    img.write_to(&mut cursor, ImageFormat::Png)
        .map_err(|err| ArchiveError::Json(format!("overview PNG encode failed: {err}")))?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesizes_complete_overview_chain_down_to_z0() {
        let leaf = RgbaImage::from_pixel(TILE_DIM, TILE_DIM, Rgba([220, 190, 140, 255]));
        let png = encode_rgba_png(&leaf);
        assert!(png.is_ok());
        let mut tiles = BTreeMap::new();
        if let Ok(bytes) = png {
            tiles.insert((8, 75, 96), bytes);
        }
        let added = synthesize_overview_pyramid(&mut tiles, 8, 0);
        assert_eq!(added.ok(), Some(8));
        assert!(tiles.contains_key(&(7, 37, 48)));
        assert!(tiles.contains_key(&(0, 0, 0)));
    }

    #[test]
    fn merges_asymmetric_conus_z8_and_oceanic_z5_pyramids() {
        let conus = RgbaImage::from_pixel(TILE_DIM, TILE_DIM, Rgba([240, 240, 232, 255]));
        let oceanic = RgbaImage::from_pixel(TILE_DIM, TILE_DIM, Rgba([180, 210, 240, 255]));
        let mut tiles = BTreeMap::new();
        if let (Ok(conus_png), Ok(ocean_png)) = (encode_rgba_png(&conus), encode_rgba_png(&oceanic))
        {
            tiles.insert((8, 75, 96), conus_png);
            tiles.insert((5, 3, 12), ocean_png);
        }
        let res = synthesize_overview_pyramid(&mut tiles, 8, 0);
        assert!(res.is_ok());
        // CONUS missing intermediate levels (z7, z6, z5, z4) must be synthesized.
        assert!(tiles.contains_key(&(7, 37, 48)));
        assert!(tiles.contains_key(&(6, 18, 24)));
        assert!(tiles.contains_key(&(5, 9, 12)));
        assert!(tiles.contains_key(&(4, 4, 6)));
        // Oceanic z5 parent (4, 1, 6) must also propagate despite having no z8 children.
        assert!(tiles.contains_key(&(4, 1, 6)));
        assert!(tiles.contains_key(&(0, 0, 0)));
    }
}
