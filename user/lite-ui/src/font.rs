//! Checked proportional UI atlas and premultiplied A8 glyph raster.

use std::{fs, io};

use linux_uapi::drm::SharedDumbBuffer;

use crate::{
    renderer::{PhysicalRect, SCALE},
    style::Computed,
};

const PATH: &str = "/usr/share/liteos/liteos-ui.a8p";
const MAGIC: &[u8; 8] = b"LUP8\0\0\0\x01";
const GLYPH_COUNT: usize = 4111;

#[derive(Clone, Copy)]
struct Glyph {
    advance: i16,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    bitmap: usize,
}

struct Face {
    ascent: i32,
    descent: i32,
    glyphs: Vec<Glyph>,
}

/// Fully validated checked UI atlas.
pub struct Font {
    bytes: Vec<u8>,
    codepoints: Vec<u32>,
    faces: Vec<Face>,
    fallback: usize,
}

impl Font {
    /// Opens and validates every atlas offset before rendering begins.
    pub fn open() -> io::Result<Self> {
        let bytes = fs::read(PATH)?;
        if bytes.get(..8) != Some(MAGIC) || read_u32(&bytes, 8) != Some(3) {
            return Err(invalid("UI atlas header is invalid"));
        }
        let glyph_count = read_u32(&bytes, 12).unwrap_or_default() as usize;
        if glyph_count != GLYPH_COUNT {
            return Err(invalid("UI atlas glyph count changed"));
        }
        let mut codepoints = Vec::with_capacity(glyph_count);
        for index in 0..glyph_count {
            codepoints.push(
                read_u32(&bytes, 16 + index * 4)
                    .ok_or_else(|| invalid("UI atlas codepoint table is truncated"))?,
            );
        }
        if !codepoints.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(invalid("UI atlas codepoints are not ordered"));
        }
        let fallback = codepoints
            .binary_search(&0xfffd)
            .map_err(|_| invalid("UI atlas fallback glyph is missing"))?;
        let mut offset = 16 + glyph_count * 4;
        let mut faces = Vec::with_capacity(3);
        for (expected_kind, expected_size) in [(0, 22), (1, 24), (1, 28)] {
            if read_u32(&bytes, offset) != Some(expected_kind)
                || read_u32(&bytes, offset + 4) != Some(expected_size)
            {
                return Err(invalid("UI atlas face identity is invalid"));
            }
            let ascent = read_i32(&bytes, offset + 8)
                .ok_or_else(|| invalid("UI atlas face header is truncated"))?;
            let descent = read_i32(&bytes, offset + 12)
                .ok_or_else(|| invalid("UI atlas face header is truncated"))?;
            offset += 16;
            let mut glyphs = Vec::with_capacity(glyph_count);
            for _ in 0..glyph_count {
                let glyph = Glyph {
                    advance: read_i16(&bytes, offset)
                        .ok_or_else(|| invalid("UI atlas glyph is truncated"))?,
                    x: read_i16(&bytes, offset + 2)
                        .ok_or_else(|| invalid("UI atlas glyph is truncated"))?,
                    y: read_i16(&bytes, offset + 4)
                        .ok_or_else(|| invalid("UI atlas glyph is truncated"))?,
                    width: read_u16(&bytes, offset + 6)
                        .ok_or_else(|| invalid("UI atlas glyph is truncated"))?,
                    height: read_u16(&bytes, offset + 8)
                        .ok_or_else(|| invalid("UI atlas glyph is truncated"))?,
                    bitmap: offset + 10,
                };
                offset = glyph
                    .bitmap
                    .checked_add(usize::from(glyph.width) * usize::from(glyph.height))
                    .ok_or_else(|| invalid("UI atlas glyph size overflow"))?;
                if offset > bytes.len() {
                    return Err(invalid("UI atlas glyph bitmap is truncated"));
                }
                glyphs.push(glyph);
            }
            faces.push(Face {
                ascent,
                descent,
                glyphs,
            });
        }
        if offset != bytes.len() {
            return Err(invalid("UI atlas contains trailing bytes"));
        }
        Ok(Self {
            bytes,
            codepoints,
            faces,
            fallback,
        })
    }

    /// Draws one CSS text node clipped to its physical layout box.
    ///
    /// The vertical clip extends to the font descent below the baseline: a line
    /// box is only `line-height` tall, but CSS text overflows it downward, so
    /// descenders (y/g/j/p/q) must not be cut off. The horizontal clip stays at
    /// the layout box so long runs still truncate at their container.
    pub fn draw(
        &self,
        target: &mut SharedDumbBuffer,
        bounds: PhysicalRect,
        style: &Computed,
        text: &str,
    ) {
        let face = self.face(style);
        let color = style.get("color").and_then(color).unwrap_or(0xff00_0000);
        let pen = bounds.x1 as i32;
        let baseline = bounds.y1 as i32 + face.ascent;
        let clip = PhysicalRect {
            y2: (baseline + face.descent)
                .max(bounds.y2 as i32)
                .min(target.height() as i32) as usize,
            ..bounds
        };
        // 1. `text-shadow` paints a solid offset copy of the run first; the clip
        //    box grows in the offset direction so the shadow is not cut short.
        if let Some((dx, dy, shadow_color)) = style.get("text-shadow").and_then(text_shadow) {
            let shadow_clip = PhysicalRect {
                x1: (clip.x1 as i32 + dx.min(0)).max(0) as usize,
                y1: (clip.y1 as i32 + dy.min(0)).max(0) as usize,
                x2: (clip.x2 as i32 + dx.max(0)).min(target.width() as i32) as usize,
                y2: (clip.y2 as i32 + dy.max(0)).min(target.height() as i32) as usize,
            };
            self.pass(target, shadow_clip, face, text, pen + dx, baseline + dy, shadow_color);
        }
        self.pass(target, clip, face, text, pen, baseline, color);
    }

    /// Draws one text run in a single color at the given pen origin.
    fn pass(
        &self,
        target: &mut SharedDumbBuffer,
        clip: PhysicalRect,
        face: &Face,
        text: &str,
        pen: i32,
        baseline: i32,
        color: u32,
    ) {
        let mut pen = pen;
        for character in text.chars() {
            let index = self
                .codepoints
                .binary_search(&(character as u32))
                .unwrap_or(self.fallback);
            let glyph = face.glyphs[index];
            self.glyph(target, clip, glyph, pen, baseline, color);
            pen += i32::from(glyph.advance);
            if pen >= clip.x2 as i32 {
                break;
            }
        }
    }

    fn face(&self, style: &Computed) -> &Face {
        let bold = style.get("font-weight") == Some("bold")
            || style
                .get("font-weight")
                .and_then(|value| value.parse::<u32>().ok())
                .is_some_and(|weight| weight >= 600);
        let size = style.px("font-size", 11.0);
        &self.faces[if bold && size > 12.0 {
            2
        } else if bold {
            1
        } else {
            0
        }]
    }

    fn glyph(
        &self,
        target: &mut SharedDumbBuffer,
        clip: PhysicalRect,
        glyph: Glyph,
        pen: i32,
        baseline: i32,
        color: u32,
    ) {
        let x = pen + i32::from(glyph.x);
        let y = baseline + i32::from(glyph.y);
        for row in 0..i32::from(glyph.height) {
            let target_y = y + row;
            if target_y < clip.y1 as i32 || target_y >= clip.y2 as i32 {
                continue;
            }
            let target_row = target.row_mut(target_y as usize);
            for column in 0..i32::from(glyph.width) {
                let target_x = x + column;
                if target_x < clip.x1 as i32 || target_x >= clip.x2 as i32 {
                    continue;
                }
                let alpha = self.bytes
                    [glyph.bitmap + row as usize * usize::from(glyph.width) + column as usize];
                if alpha != 0 {
                    let background = target_row[target_x as usize];
                    target_row[target_x as usize] = blend(background, color, alpha);
                }
            }
        }
    }
}

fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let channel = |shift: u32| {
        (((foreground >> shift) & 0xffu32) * alpha + ((background >> shift) & 0xffu32) * inverse)
            / 255
    };
    0xff00_0000 | channel(16) << 16 | channel(8) << 8 | channel(0)
}

fn color(value: &str) -> Option<u32> {
    let hex = value.trim().strip_prefix('#')?;
    (hex.len() == 6)
        .then(|| {
            u32::from_str_radix(hex, 16)
                .ok()
                .map(|value| 0xff00_0000 | value)
        })
        .flatten()
}

/// Parses `text-shadow: <dx> <dy> [blur] <#rrggbb>` into physical offsets and
/// a solid shadow color.
///
/// The atlas raster has no blur pass, so an optional blur radius is accepted
/// and ignored; XP-style labels only use a hard 1px drop shadow.
fn text_shadow(value: &str) -> Option<(i32, i32, u32)> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let mut numbers = parts.iter().filter_map(|part| {
        part.strip_suffix("px")
            .unwrap_or(part)
            .trim()
            .parse::<f32>()
            .ok()
    });
    let dx = numbers.next()?;
    let dy = numbers.next()?;
    let color = parts.iter().rev().find_map(|part| color(part))?;
    Some((
        (dx * SCALE).round() as i32,
        (dy * SCALE).round() as i32,
        color,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(read_u32(bytes, offset)? as i32)
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_i16(bytes: &[u8], offset: usize) -> Option<i16> {
    Some(read_u16(bytes, offset)? as i16)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_shadow_parses_offsets_and_color() {
        assert_eq!(
            text_shadow("1px 1px #123b66"),
            Some((SCALE as i32, SCALE as i32, 0xff12_3b66))
        );
    }

    #[test]
    fn text_shadow_accepts_and_ignores_blur() {
        assert_eq!(
            text_shadow("0px 1px 2px #000000"),
            Some((0, SCALE as i32, 0xff00_0000))
        );
    }

    #[test]
    fn text_shadow_rejects_missing_parts() {
        assert_eq!(text_shadow("1px #123b66"), None);
        assert_eq!(text_shadow("1px 1px"), None);
    }
}
