//! Decodes the title image held in an XBE's `$$XTIMAGE` section.
//!
//! The section is an XPR0 bundle holding one texture in the console's own format: usually DXT1,
//! sometimes DXT3/DXT5 or a swizzled 32-bit format. This decodes those to RGBA and encodes PNG
//! for display.

use crate::XbeError;

const XPR_MAGIC: &[u8; 4] = b"XPR0";

// NV2A texture formats (the colour-format byte of the D3D format dword).
const FMT_A8R8G8B8: u32 = 0x06;
const FMT_X8R8G8B8: u32 = 0x07;
const FMT_DXT1: u32 = 0x0C;
const FMT_DXT3: u32 = 0x0E;
const FMT_DXT5: u32 = 0x0F;
const FMT_LIN_A8R8G8B8: u32 = 0x12;
const FMT_LIN_X8R8G8B8: u32 = 0x1E;

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("title image is not an XPR0 bundle")]
    NotXpr,
    #[error("title image uses texture format 0x{0:02X}, which this does not decode")]
    UnsupportedFormat(u32),
    #[error("title image data is truncated")]
    Truncated,
    #[error(transparent)]
    Xbe(#[from] XbeError),
    #[error("PNG encoding failed: {0}")]
    Png(String),
}

pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Decodes the XPR0 bundle from a `$$XTIMAGE` section to RGBA.
pub fn decode_xpr(data: &[u8]) -> Result<Rgba, ImageError> {
    if data.len() < 32 || &data[0..4] != XPR_MAGIC {
        return Err(ImageError::NotXpr);
    }
    let rd = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    let header_size = rd(8) as usize;
    let format = rd(12 + 12);
    let color = (format >> 8) & 0xFF;
    let width = 1u32 << ((format >> 20) & 0xF);
    let height = 1u32 << ((format >> 24) & 0xF);
    let pixels = data.get(header_size..).ok_or(ImageError::Truncated)?;

    let rgba = match color {
        FMT_DXT1 | FMT_DXT3 | FMT_DXT5 => decode_dxt(pixels, width, height, color)?,
        FMT_A8R8G8B8 | FMT_X8R8G8B8 => decode_argb(pixels, width, height, true, color == FMT_X8R8G8B8)?,
        FMT_LIN_A8R8G8B8 | FMT_LIN_X8R8G8B8 => {
            decode_argb(pixels, width, height, false, color == FMT_LIN_X8R8G8B8)?
        }
        other => return Err(ImageError::UnsupportedFormat(other)),
    };
    Ok(Rgba { width, height, pixels: rgba })
}

/// Encodes RGBA as PNG.
pub fn to_png(img: &Rgba) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| ImageError::Png(e.to_string()))?;
        w.write_image_data(&img.pixels).map_err(|e| ImageError::Png(e.to_string()))?;
    }
    Ok(out)
}

fn rgb565(c: u16) -> [u8; 3] {
    let r = ((c >> 11) & 0x1F) as u32;
    let g = ((c >> 5) & 0x3F) as u32;
    let b = (c & 0x1F) as u32;
    [((r * 255 + 15) / 31) as u8, ((g * 255 + 31) / 63) as u8, ((b * 255 + 15) / 31) as u8]
}

fn color_block(b: &[u8], four_colour: bool) -> [[u8; 4]; 16] {
    let c0 = u16::from_le_bytes([b[0], b[1]]);
    let c1 = u16::from_le_bytes([b[2], b[3]]);
    let p0 = rgb565(c0);
    let p1 = rgb565(c1);
    let mix = |a: u8, b: u8, wa: u32, wb: u32| ((a as u32 * wa + b as u32 * wb) / (wa + wb)) as u8;
    let mut pal = [[0u8; 4]; 4];
    pal[0] = [p0[0], p0[1], p0[2], 255];
    pal[1] = [p1[0], p1[1], p1[2], 255];
    if four_colour || c0 > c1 {
        pal[2] = [mix(p0[0], p1[0], 2, 1), mix(p0[1], p1[1], 2, 1), mix(p0[2], p1[2], 2, 1), 255];
        pal[3] = [mix(p0[0], p1[0], 1, 2), mix(p0[1], p1[1], 1, 2), mix(p0[2], p1[2], 1, 2), 255];
    } else {
        pal[2] = [mix(p0[0], p1[0], 1, 1), mix(p0[1], p1[1], 1, 1), mix(p0[2], p1[2], 1, 1), 255];
        pal[3] = [0, 0, 0, 0];
    }
    let idx = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let mut out = [[0u8; 4]; 16];
    for (i, px) in out.iter_mut().enumerate() {
        *px = pal[((idx >> (2 * i)) & 3) as usize];
    }
    out
}

fn decode_dxt(data: &[u8], w: u32, h: u32, fmt: u32) -> Result<Vec<u8>, ImageError> {
    let block_bytes = if fmt == FMT_DXT1 { 8 } else { 16 };
    let bw = w.div_ceil(4) as usize;
    let bh = h.div_ceil(4) as usize;
    if data.len() < bw * bh * block_bytes {
        return Err(ImageError::Truncated);
    }
    let mut out = vec![0u8; (w * h * 4) as usize];
    for by in 0..bh {
        for bx in 0..bw {
            let b = &data[(by * bw + bx) * block_bytes..][..block_bytes];
            let (alpha, colour) = match fmt {
                FMT_DXT1 => (None, color_block(b, false)),
                FMT_DXT3 => {
                    let mut a = [0u8; 16];
                    for (i, v) in a.iter_mut().enumerate() {
                        let nib = (b[i / 2] >> ((i % 2) * 4)) & 0xF;
                        *v = nib * 17;
                    }
                    (Some(a), color_block(&b[8..], true))
                }
                _ => {
                    let (a0, a1) = (b[0] as u32, b[1] as u32);
                    let mut bits = 0u64;
                    for i in 0..6 {
                        bits |= (b[2 + i] as u64) << (8 * i);
                    }
                    let mut a = [0u8; 16];
                    for (i, v) in a.iter_mut().enumerate() {
                        let k = ((bits >> (3 * i)) & 7) as u32;
                        *v = match (k, a0 > a1) {
                            (0, _) => a0 as u8,
                            (1, _) => a1 as u8,
                            (k, true) => (((8 - k) * a0 + (k - 1) * a1) / 7) as u8,
                            (6, false) => 0,
                            (7, false) => 255,
                            (k, false) => (((6 - k) * a0 + (k - 1) * a1) / 5) as u8,
                        };
                    }
                    (Some(a), color_block(&b[8..], true))
                }
            };
            for i in 0..16 {
                let x = bx as u32 * 4 + (i as u32 % 4);
                let y = by as u32 * 4 + (i as u32 / 4);
                if x >= w || y >= h {
                    continue;
                }
                let o = ((y * w + x) * 4) as usize;
                let mut px = colour[i];
                if let Some(a) = alpha {
                    px[3] = a[i];
                }
                out[o..o + 4].copy_from_slice(&px);
            }
        }
    }
    Ok(out)
}

/// Offset of texel (x, y) in a swizzled (Morton-ordered) power-of-two texture.
fn swizzle_offset(x: u32, y: u32, w: u32, h: u32) -> u32 {
    let (mut xb, mut yb, mut ww, mut hh) = (x, y, w, h);
    let (mut off, mut shift) = (0u32, 0u32);
    while ww > 1 || hh > 1 {
        if ww > 1 {
            off |= (xb & 1) << shift;
            xb >>= 1;
            shift += 1;
            ww >>= 1;
        }
        if hh > 1 {
            off |= (yb & 1) << shift;
            yb >>= 1;
            shift += 1;
            hh >>= 1;
        }
    }
    off
}

fn decode_argb(data: &[u8], w: u32, h: u32, swizzled: bool, opaque: bool) -> Result<Vec<u8>, ImageError> {
    if data.len() < (w * h * 4) as usize {
        return Err(ImageError::Truncated);
    }
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let src = if swizzled { swizzle_offset(x, y, w, h) } else { y * w + x } as usize * 4;
            let (b, g, r, a) = (data[src], data[src + 1], data[src + 2], data[src + 3]);
            let o = ((y * w + x) * 4) as usize;
            out[o..o + 4].copy_from_slice(&[r, g, b, if opaque { 255 } else { a }]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xpr(format_byte: u32, log_w: u32, log_h: u32, pixels: &[u8]) -> Vec<u8> {
        let header = 0x40u32;
        let mut d = vec![0u8; header as usize];
        d[0..4].copy_from_slice(XPR_MAGIC);
        d[4..8].copy_from_slice(&(header + pixels.len() as u32).to_le_bytes());
        d[8..12].copy_from_slice(&header.to_le_bytes());
        let format = (format_byte << 8) | (2 << 4) | (1 << 16) | (log_w << 20) | (log_h << 24);
        d[24..28].copy_from_slice(&format.to_le_bytes());
        d.extend_from_slice(pixels);
        d
    }

    #[test]
    fn dxt1_solid_red_block() {
        // c0 = pure red (0xF800), c1 = black, every index 0.
        let block = [0x00, 0xF8, 0x00, 0x00, 0, 0, 0, 0];
        let img = decode_xpr(&xpr(FMT_DXT1, 2, 2, &block)).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        assert!(img.pixels.chunks(4).all(|p| p == [255, 0, 0, 255]));
        assert!(to_png(&img).unwrap().starts_with(b"\x89PNG"));
    }

    #[test]
    fn swizzled_argb_round_trips_corners() {
        // 2x2 swizzled: order is (0,0) (1,0) (0,1) (1,1).
        let px = [
            0, 0, 255, 255, // red at (0,0)
            0, 255, 0, 255, // green at (1,0)
            255, 0, 0, 255, // blue at (0,1)
            255, 255, 255, 255,
        ];
        let img = decode_xpr(&xpr(FMT_A8R8G8B8, 1, 1, &px)).unwrap();
        assert_eq!(&img.pixels[0..4], &[255, 0, 0, 255]);
        assert_eq!(&img.pixels[4..8], &[0, 255, 0, 255]);
        assert_eq!(&img.pixels[8..12], &[0, 0, 255, 255]);
    }

    #[test]
    fn rejects_other_formats() {
        assert!(matches!(decode_xpr(&[0u8; 64]), Err(ImageError::NotXpr)));
        assert!(matches!(
            decode_xpr(&xpr(0x01, 1, 1, &[0; 16])),
            Err(ImageError::UnsupportedFormat(0x01))
        ));
    }
}
