const BYTES: &[u8] = include_bytes!("../../../assets/fonts/liteos-terminal.a8");
const MAGIC: &[u8; 8] = b"LTA8\0\0\0\x02";
const GLYPH_COUNT: usize = 468;
const FACE_COUNT: usize = 2;

#[derive(Clone, Copy)]
pub struct Atlas {
    codepoints_offset: usize,
    bitmap_offset: usize,
}

impl Atlas {
    pub fn checked() -> Option<Self> {
        if BYTES.get(..8)? != MAGIC
            || read_u32(8)? as usize != GLYPH_COUNT
            || read_u32(12)? as usize != 32
            || read_u32(16)? as usize != 32 + GLYPH_COUNT * 4
            || read_u16(20)? != 16
            || read_u16(22)? != 32
            || read_u32(24)? as usize != FACE_COUNT
            || BYTES.get(28..32)?.iter().any(|byte| *byte != 0)
            || BYTES.len() != 32 + GLYPH_COUNT * 4 + GLYPH_COUNT * 16 * 32 * FACE_COUNT
        {
            return None;
        }
        let atlas = Self {
            codepoints_offset: 32,
            bitmap_offset: 32 + GLYPH_COUNT * 4,
        };
        let mut previous = None;
        for index in 0..GLYPH_COUNT {
            let codepoint = atlas.codepoint(index)?;
            if previous.is_some_and(|value| value >= codepoint) {
                return None;
            }
            previous = Some(codepoint);
        }
        atlas.find(0xfffd)?;
        Some(atlas)
    }

    pub fn glyph(self, codepoint: u32, bold: bool) -> &'static [u8] {
        let index = self
            .find(codepoint)
            .or_else(|| self.find(0xfffd))
            .unwrap_or(0);
        let glyph_bytes = 16 * 32;
        let face = usize::from(bold) * GLYPH_COUNT * glyph_bytes;
        let start = self.bitmap_offset + face + index * glyph_bytes;
        &BYTES[start..start + glyph_bytes]
    }

    fn find(self, codepoint: u32) -> Option<usize> {
        let mut low = 0;
        let mut high = GLYPH_COUNT;
        while low < high {
            let middle = low + (high - low) / 2;
            match self.codepoint(middle)?.cmp(&codepoint) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    fn codepoint(self, index: usize) -> Option<u32> {
        read_u32(self.codepoints_offset + index * 4)
    }
}

pub fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let red = ((foreground >> 16 & 0xff) * alpha + (background >> 16 & 0xff) * inverse) / 255;
    let green = ((foreground >> 8 & 0xff) * alpha + (background >> 8 & 0xff) * inverse) / 255;
    let blue = ((foreground & 0xff) * alpha + (background & 0xff) * inverse) / 255;
    red << 16 | green << 8 | blue
}

fn read_u32(offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        BYTES.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u16(offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        BYTES.get(offset..offset + 2)?.try_into().ok()?,
    ))
}
