//! Checked XP arrow cursor composited as a damage overlay on the scanned-out buffer.
//!
//! The cursor is deliberately decoupled from scene composition. Painting it into
//! the buffer directly and tracking a backing store lets pointer motion refresh
//! only a 32x32 region through `DRM_IOCTL_MODE_DIRTYFB` instead of recompositing
//! and page-flipping the whole screen on every move.

use std::io;

use linux_uapi::drm::{Clip, DumbBuffer};

const PATH: &str = "/usr/share/liteos/cursor.lc1";
const MAGIC: &[u8; 8] = b"LCR1\0\0\0\x01";
const WIDTH: usize = 32;
const HEIGHT: usize = 32;
const HEADER: usize = 16;
const BITMAP_SIZE: usize = HEIGHT * (WIDTH / 8);

pub struct Cursor {
    bytes: Vec<u8>,
    /// Clean pixels captured under the arrow before it was rasterized.
    ///
    /// A relocate restores these into the buffer to erase the previous cursor
    /// without recompositing. Row-major over the last saved box, width `x2 - x1`.
    backing: Vec<u32>,
    /// Clamped box `(x1, y1, x2, y2)` the [`Self::backing`] pixels belong to.
    ///
    /// Invariant: it always describes the currently scanned-out (front) buffer,
    /// re-established by [`Self::overlay`] on every page flip and updated in
    /// place by [`Self::relocate`]. A degenerate box (x2<=x1) means "nothing
    /// painted yet" so the first relocate restores nothing.
    saved: (i32, i32, i32, i32),
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
        Ok(Self {
            bytes,
            backing: Vec::new(),
            saved: (0, 0, 0, 0),
        })
    }

    /// Rasterizes the cursor into a freshly composed back buffer before a flip.
    ///
    /// 1. Saves the clean scene pixels the arrow will cover.
    /// 2. Draws the arrow.
    ///
    /// It deliberately does not restore a previous backing: the back buffer was
    /// just fully recomposed, so no stale cursor exists there. After the flip the
    /// saved box describes the new front buffer, keeping the [`Self::saved`]
    /// invariant for subsequent [`Self::relocate`] calls.
    pub fn overlay(&mut self, target: &mut DumbBuffer, x: i32, y: i32) {
        self.save(target, x, y);
        self.paint(target, x, y);
    }

    /// Moves the cursor on the scanned-out front buffer, returning the previous
    /// and new damage boxes for `DIRTYFB`.
    ///
    /// 1. Restores the old backing to erase the previous arrow.
    /// 2. Saves the clean pixels at the new position.
    /// 3. Draws the arrow at the new position.
    ///
    /// Degenerate boxes (cursor fully off-screen) are returned as empty clips and
    /// filtered by the caller.
    pub fn relocate(&mut self, target: &mut DumbBuffer, x: i32, y: i32) -> [Clip; 2] {
        let old = self.saved;
        self.restore(target);
        self.save(target, x, y);
        self.paint(target, x, y);
        [clip(old), clip(self.saved)]
    }

    fn save(&mut self, target: &DumbBuffer, x: i32, y: i32) {
        let (x1, y1, x2, y2) = bounds(target, x, y);
        if x2 <= x1 || y2 <= y1 {
            self.saved = (0, 0, 0, 0);
            return;
        }
        let width = (x2 - x1) as usize;
        self.backing.resize(width * (y2 - y1) as usize, 0);
        for screen_y in y1..y2 {
            let offset = (screen_y - y1) as usize * width;
            self.backing[offset..offset + width]
                .copy_from_slice(&target.row(screen_y as usize)[x1 as usize..x2 as usize]);
        }
        self.saved = (x1, y1, x2, y2);
    }

    fn restore(&self, target: &mut DumbBuffer) {
        let (x1, y1, x2, y2) = self.saved;
        if x2 <= x1 || y2 <= y1 {
            return;
        }
        let width = (x2 - x1) as usize;
        for screen_y in y1..y2 {
            let offset = (screen_y - y1) as usize * width;
            target.row_mut(screen_y as usize)[x1 as usize..x2 as usize]
                .copy_from_slice(&self.backing[offset..offset + width]);
        }
    }

    fn paint(&self, target: &mut DumbBuffer, x: i32, y: i32) {
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

/// Clamps the arrow rectangle to the buffer, returning `(x1, y1, x2, y2)`.
fn bounds(target: &DumbBuffer, x: i32, y: i32) -> (i32, i32, i32, i32) {
    (
        x.max(0),
        y.max(0),
        (x + WIDTH as i32).min(target.width() as i32),
        (y + HEIGHT as i32).min(target.height() as i32),
    )
}

/// Converts a clamped box into a `DIRTYFB` clip, collapsing degenerate boxes to
/// an empty clip so the caller can drop them.
fn clip((x1, y1, x2, y2): (i32, i32, i32, i32)) -> Clip {
    if x2 <= x1 || y2 <= y1 {
        return Clip {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        };
    }
    Clip {
        x1: x1 as u16,
        y1: y1 as u16,
        x2: x2 as u16,
        y2: y2 as u16,
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
