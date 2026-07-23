//! Checked fixed-cell terminal A8 atlas (JetBrains Mono NL, 16x32 physical cells).
//!
//! The atlas mirrors `scripts/generate_terminal_font.py`: a 32-byte header, a
//! sorted u32 codepoint table, then two tightly packed 16x32 A8 faces. One cell
//! is exactly one terminal grid unit (8x16 CSS px at the display scale), so the
//! VT grid, the React cursor math and the resize divisor all share one geometry.

use std::{fs, io};

use linux_uapi::drm::SharedDumbBuffer;

use crate::{renderer::PhysicalRect, style::Computed};

const PATH: &str = "/usr/share/liteos/liteos-terminal.a8";
const MAGIC: &[u8; 8] = b"LTA8\0\0\0\x02";
const GLYPH_COUNT: usize = 468;
const FACE_COUNT: usize = 2;
/// Physical cell extent; one cell is one terminal grid column/row.
pub(crate) const CELL_WIDTH: usize = 16;
pub(crate) const CELL_HEIGHT: usize = 32;
const GLYPH_BYTES: usize = CELL_WIDTH * CELL_HEIGHT;

/// Fully validated fixed-cell terminal atlas.
pub struct TerminalFont {
    bytes: Vec<u8>,
    codepoints: Vec<u32>,
    faces: usize,
    fallback: usize,
}

impl TerminalFont {
    /// Opens and validates every atlas offset before rendering begins.
    pub fn open() -> io::Result<Self> {
        Self::parse(fs::read(PATH)?)
    }

    fn parse(bytes: Vec<u8>) -> io::Result<Self> {
        if bytes.get(..8) != Some(MAGIC) {
            return Err(invalid("terminal atlas header is invalid"));
        }
        if read_u32(&bytes, 8) != Some(GLYPH_COUNT as u32) {
            return Err(invalid("terminal atlas glyph count changed"));
        }
        let codepoints_offset = read_u32(&bytes, 12).unwrap_or_default() as usize;
        let faces = read_u32(&bytes, 16).unwrap_or_default() as usize;
        if codepoints_offset != 32
            || read_u16(&bytes, 20) != Some(CELL_WIDTH as u16)
            || read_u16(&bytes, 22) != Some(CELL_HEIGHT as u16)
            || read_u32(&bytes, 24) != Some(FACE_COUNT as u32)
            || faces != codepoints_offset + GLYPH_COUNT * 4
            || bytes.len() != faces + FACE_COUNT * GLYPH_COUNT * GLYPH_BYTES
        {
            return Err(invalid("terminal atlas geometry is invalid"));
        }
        let mut codepoints = Vec::with_capacity(GLYPH_COUNT);
        for index in 0..GLYPH_COUNT {
            codepoints.push(
                read_u32(&bytes, codepoints_offset + index * 4)
                    .ok_or_else(|| invalid("terminal atlas codepoint table is truncated"))?,
            );
        }
        if !codepoints.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(invalid("terminal atlas codepoints are not ordered"));
        }
        let fallback = codepoints
            .binary_search(&0xfffd)
            .map_err(|_| invalid("terminal atlas fallback glyph is missing"))?;
        Ok(Self {
            bytes,
            codepoints,
            faces,
            fallback,
        })
    }

    /// Draws one monospace text row cell by cell, clipped to its layout box.
    ///
    /// Cell `i` always lands at `bounds.x1 + i * CELL_WIDTH` regardless of the
    /// glyph's ink width: the terminal grid is the layout contract, unlike the
    /// proportional UI atlas whose pen advances per glyph.
    pub fn draw(
        &self,
        target: &mut SharedDumbBuffer,
        bounds: PhysicalRect,
        style: &Computed,
        text: &str,
    ) {
        let color = style.get("color").and_then(color).unwrap_or(0xff00_0000);
        for (index, character) in text.chars().enumerate() {
            let cell_x = bounds.x1 + index * CELL_WIDTH;
            if cell_x >= bounds.x2 {
                break;
            }
            let glyph = self
                .codepoints
                .binary_search(&(character as u32))
                .unwrap_or(self.fallback);
            let bitmap = self.faces + glyph * GLYPH_BYTES;
            for row in 0..CELL_HEIGHT {
                let target_y = bounds.y1 + row;
                if target_y >= bounds.y2 {
                    break;
                }
                let target_row = target.row_mut(target_y);
                for column in 0..CELL_WIDTH {
                    // The final cell can straddle the layout box edge: glyph
                    // cells blit whole, so clip per pixel instead of per cell.
                    let target_x = cell_x + column;
                    if target_x >= bounds.x2 {
                        break;
                    }
                    let alpha = self.bytes[bitmap + row * CELL_WIDTH + column];
                    if alpha != 0 {
                        let background = target_row[target_x];
                        target_row[target_x] = blend(background, color, alpha);
                    }
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

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one in-memory atlas with the production geometry: sorted
    /// codepoints `U+0020..=U+01F2` plus the `U+FFFD` fallback, all-blank cells.
    fn synthetic_atlas() -> Vec<u8> {
        let codepoints: Vec<u32> = (0x20..0x20 + GLYPH_COUNT as u32 - 1)
            .chain([0xfffd])
            .collect();
        let faces = 32 + GLYPH_COUNT * 4;
        let mut bytes = vec![0u8; faces + FACE_COUNT * GLYPH_COUNT * GLYPH_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&(GLYPH_COUNT as u32).to_le_bytes());
        bytes[12..16].copy_from_slice(&32u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&(faces as u32).to_le_bytes());
        bytes[20..22].copy_from_slice(&(CELL_WIDTH as u16).to_le_bytes());
        bytes[22..24].copy_from_slice(&(CELL_HEIGHT as u16).to_le_bytes());
        bytes[24..28].copy_from_slice(&(FACE_COUNT as u32).to_le_bytes());
        for (index, codepoint) in codepoints.iter().enumerate() {
            bytes[32 + index * 4..36 + index * 4].copy_from_slice(&codepoint.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn valid_atlas_parses_with_fallback_lookup() {
        let font = TerminalFont::parse(synthetic_atlas()).expect("valid atlas");
        assert_eq!(font.codepoints[font.fallback], 0xfffd);
    }

    #[test]
    fn wrong_magic_is_rejected() {
        let mut bytes = synthetic_atlas();
        bytes[0] = b'X';
        assert!(TerminalFont::parse(bytes).is_err());
    }

    #[test]
    fn unsorted_codepoints_are_rejected() {
        let mut bytes = synthetic_atlas();
        bytes[32..36].copy_from_slice(&0xffff_u32.to_le_bytes());
        assert!(TerminalFont::parse(bytes).is_err());
    }

    #[test]
    fn missing_fallback_glyph_is_rejected() {
        let mut bytes = synthetic_atlas();
        let last = 32 + (GLYPH_COUNT - 1) * 4;
        bytes[last..last + 4].copy_from_slice(&0xfffe_u32.to_le_bytes());
        assert!(TerminalFont::parse(bytes).is_err());
    }

    #[test]
    fn truncated_bitmap_is_rejected() {
        let bytes = synthetic_atlas();
        let truncated = bytes[..bytes.len() - 1].to_vec();
        assert!(TerminalFont::parse(truncated).is_err());
    }
}
