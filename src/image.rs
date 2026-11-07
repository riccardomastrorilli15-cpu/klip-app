//! Image helpers. Full copies are stored as PNG (small in RAM); the picker only ever
//! receives a tiny RGBA thumbnail.

use std::io::Cursor;

pub const THUMB_W: u32 = 160;
pub const THUMB_H: u32 = 100;
/// Images larger than this are ignored: decoding them would spike memory.
pub const MAX_PIXELS: u64 = 12_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Thumb {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

pub fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if rgba.len() as u64 != u64::from(w) * u64::from(h) * 4 {
        return None;
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().ok()?;
    writer.write_image_data(rgba).ok()?;
    writer.finish().ok()?;
    Some(out)
}

pub fn decode_png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut reader = png::Decoder::new(Cursor::new(data)).read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some((info.width, info.height, buf))
}

/// Box-filter downscale to fit THUMB_W x THUMB_H (never upscales).
pub fn thumbnail(w: u32, h: u32, rgba: &[u8]) -> Thumb {
    let scale = (THUMB_W as f32 / w as f32)
        .min(THUMB_H as f32 / h as f32)
        .min(1.0);
    let tw = ((w as f32 * scale).round() as u32).clamp(1, w);
    let th = ((h as f32 * scale).round() as u32).clamp(1, h);
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1).min(w);
            let mut acc = [0u32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * w + x) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += u32::from(rgba[i + c]);
                    }
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            for a in acc {
                out.push((a / n) as u8);
            }
        }
    }
    Thumb {
        w: tw,
        h: th,
        rgba: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| [(i % w) as u8, (i / w) as u8, 128, 255])
            .collect()
    }

    #[test]
    fn png_roundtrip_is_lossless() {
        let px = gradient(37, 23);
        let png = encode_png(37, 23, &px).unwrap();
        let (w, h, back) = decode_png(&png).unwrap();
        assert_eq!((w, h), (37, 23));
        assert_eq!(back, px);
    }

    #[test]
    fn rejects_wrong_buffer_size_and_garbage() {
        assert!(encode_png(4, 4, &[0; 10]).is_none());
        assert!(decode_png(b"not a png").is_none());
    }

    #[test]
    fn thumbnail_keeps_aspect_and_never_upscales() {
        let big = gradient(1920, 1080);
        let t = thumbnail(1920, 1080, &big);
        assert!(t.w <= THUMB_W && t.h <= THUMB_H);
        assert_eq!(t.rgba.len(), (t.w * t.h * 4) as usize);
        let small = gradient(10, 6);
        let t = thumbnail(10, 6, &small);
        assert_eq!((t.w, t.h), (10, 6));
        assert_eq!(t.rgba, small);
    }

    #[test]
    fn thumbnail_of_flat_colour_stays_flat() {
        let px = [10u8, 20, 30, 255].repeat(500 * 300);
        let t = thumbnail(500, 300, &px);
        assert!(t.rgba.chunks(4).all(|p| p == [10, 20, 30, 255]));
    }
}
