//! Checked terminal A8 atlas (JetBrains Mono NL + Noto CJK, 16x32 physical cells).
//!
//! The atlas mirrors `scripts/generate_terminal_font.py`: a 32-byte header, a
//! sorted u32 codepoint table, then two tightly packed 32x32 A8 faces. A narrow
//! glyph uses the first 16 physical pixels; a wide glyph spans both terminal
//! cells, while the VT grid, client cursor math and resize divisor retain one
//! 16x32 physical cell geometry.

use std::{fs, io};

const PATH: &str = "/usr/share/liteos/liteos-terminal.a8";
const MAGIC: &[u8; 8] = b"LTA8\0\0\0\x03";
pub const GLYPH_COUNT: usize = 4470;
pub const FACE_COUNT: usize = 2;
/// Physical cell extent; one cell is one terminal grid column/row.
pub const CELL_WIDTH: usize = 16;
pub const CELL_HEIGHT: usize = 32;
pub const BITMAP_WIDTH: usize = 32;
pub const GLYPH_BYTES: usize = BITMAP_WIDTH * CELL_HEIGHT;

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

    /// Validates an in-memory atlas; used by [`TerminalFont::open`] and by tests.
    pub fn parse(bytes: Vec<u8>) -> io::Result<Self> {
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
            || read_u16(&bytes, 28) != Some(BITMAP_WIDTH as u16)
            || read_u16(&bytes, 30) != Some(0)
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

    /// Glyph index for `character`; unknown codepoints use the `U+FFFD` fallback.
    pub fn glyph(&self, character: char) -> usize {
        self.codepoints
            .binary_search(&(character as u32))
            .unwrap_or(self.fallback)
    }

    /// One `BITMAP_WIDTH x CELL_HEIGHT` A8 coverage bitmap; a narrow glyph uses the first
    /// `CELL_WIDTH` columns of each row, a wide glyph all `BITMAP_WIDTH`.
    pub fn bitmap(&self, glyph: usize, bold: bool) -> &[u8] {
        let start = self.faces + (usize::from(bold) * GLYPH_COUNT + glyph) * GLYPH_BYTES;
        &self.bytes[start..start + GLYPH_BYTES]
    }
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
        bytes[28..30].copy_from_slice(&(BITMAP_WIDTH as u16).to_le_bytes());
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
    fn checked_atlas_contains_real_chinese_glyphs_in_both_faces() {
        let font = TerminalFont::parse(
            include_bytes!("../../../assets/fonts/liteos-terminal.a8").to_vec(),
        )
        .expect("checked terminal atlas");
        for character in "中文乱码".chars() {
            let glyph = font
                .codepoints
                .binary_search(&(character as u32))
                .expect("Chinese codepoint");
            for face in 0..FACE_COUNT {
                let bitmap = font.faces + (face * GLYPH_COUNT + glyph) * GLYPH_BYTES;
                assert!(
                    font.bytes[bitmap..bitmap + GLYPH_BYTES]
                        .iter()
                        .any(|alpha| *alpha != 0),
                    "empty {character:?} glyph in face {face}"
                );
                assert!(
                    (0..CELL_HEIGHT).any(|row| font.bytes[bitmap + row * BITMAP_WIDTH + CELL_WIDTH
                        ..bitmap + (row + 1) * BITMAP_WIDTH]
                        .iter()
                        .any(|alpha| *alpha != 0)),
                    "narrow {character:?} glyph in face {face}"
                );
            }
        }
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
