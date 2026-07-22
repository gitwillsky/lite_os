//! Checked XP arrow cursor rasterized as the final compositor layer.

use std::io;

use linux_uapi::drm::DumbBuffer;

const PATH: &str = "/usr/share/liteos/cursor.lc1";
const MAGIC: &[u8; 8] = b"LCR1\0\0\0\x01";
const WIDTH: usize = 32;
const HEIGHT: usize = 32;
const HEADER: usize = 16;
const BITMAP_SIZE: usize = HEIGHT * (WIDTH / 8);

pub struct Cursor {
    bytes: Vec<u8>,
}

impl Cursor {
    pub fn open() -> io::Result<Self> {
        let bytes = std::fs::read(PATH)?;
        let valid = bytes.len() == HEADER + 2 * BITMAP_SIZE
            && bytes.get(..8) == Some(MAGIC.as_slice())
            && read_u32(&bytes, 8) == Some(WIDTH as u32)
            && read_u32(&bytes, 12) == Some(HEIGHT as u32);
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cursor asset identity invalid",
            ));
        }
        Ok(Self { bytes })
    }

    pub fn paint(&self, target: &mut DumbBuffer, x: i32, y: i32) {
        let x1 = x.max(0);
        let y1 = y.max(0);
        let x2 = (x + WIDTH as i32).min(target.width() as i32);
        let y2 = (y + HEIGHT as i32).min(target.height() as i32);
        for screen_y in y1..y2 {
            let local_y = (screen_y - y) as usize;
            let row = target.row_mut(screen_y as usize);
            for screen_x in x1..x2 {
                let local_x = (screen_x - x) as usize;
                let index = local_y * (WIDTH / 8) + local_x / 8;
                let bit = 0x80 >> (local_x & 7);
                if self.bytes[HEADER + index] & bit != 0 {
                    row[screen_x as usize] = 0xff00_0000;
                } else if self.bytes[HEADER + BITMAP_SIZE + index] & bit != 0 {
                    row[screen_x as usize] = 0xffff_ffff;
                }
            }
        }
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
