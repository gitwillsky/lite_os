//! High-resolution RGBA cursor shapes composited as a damage overlay on the
//! scanned-out buffer.
//!
//! The cursor is deliberately decoupled from scene composition. Painting it into
//! the buffer directly and tracking a backing store lets pointer motion refresh
//! only a 48x48 region through `DRM_IOCTL_MODE_DIRTYFB` instead of recompositing
//! and page-flipping the whole screen on every move.
//!
//! Shapes load from `.lc2` assets: an 8-byte magic `LCR2\0\0\0\x01`, u32-LE
//! width and height, then `width*height*4` bytes of **premultiplied** ARGB8888,
//! row-major top-to-bottom. Each pixel is 4 little-endian bytes `[B, G, R, A]`,
//! so `u32::from_le_bytes` yields `0xAARRGGBB` — exactly the premultiplied form
//! [`crate::scanout::over`] expects. 48x48 physical pixels = 24x24 logical under
//! the 2x device scale, sized to read crisply without dominating desktop chrome.

use std::io;

use linux_uapi::drm::{Clip, DumbBuffer};

use crate::scanout::over;

const PATH: &str = "/usr/share/liteos/cursor.lc2";
const POINTER_PATH: &str = "/usr/share/liteos/cursor-pointer.lc2";
const RESIZE_NS_PATH: &str = "/usr/share/liteos/cursor-resize-ns.lc2";
const RESIZE_EW_PATH: &str = "/usr/share/liteos/cursor-resize-ew.lc2";
const RESIZE_NESW_PATH: &str = "/usr/share/liteos/cursor-resize-nesw.lc2";
const RESIZE_NWSE_PATH: &str = "/usr/share/liteos/cursor-resize-nwse.lc2";
const MAGIC: &[u8; 8] = b"LCR2\0\0\0\x01";
const WIDTH: usize = 48;
const HEIGHT: usize = 48;
const HEADER: usize = 16;
/// Premultiplied ARGB8888 payload size following the header.
const PIXEL_BYTES: usize = WIDTH * HEIGHT * 4;
const SHAPE_COUNT: usize = display_proto::CURSOR_RESIZE_NWSE as usize + 1;

struct Shape {
    bytes: Vec<u8>,
    /// Physical bitmap pixel placed at the logical pointer position.
    ///
    /// Resize cursors require a centered hotspot; without it the visible
    /// double arrow would sit entirely below/right of the resize boundary.
    hotspot: (i32, i32),
}

pub struct Cursor {
    /// Validated bitmaps in the visible `display_proto::CURSOR_*` wire-value order.
    shapes: [Shape; SHAPE_COUNT],
    /// Selected wire value; `CURSOR_NONE` is the only value without a bitmap.
    active_shape: usize,
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
        Ok(Self {
            shapes: [
                Shape {
                    bytes: load_shape(PATH)?,
                    hotspot: (0, 0),
                },
                Shape {
                    bytes: load_shape(POINTER_PATH)?,
                    hotspot: (0, 0),
                },
                Shape {
                    bytes: load_shape(RESIZE_NS_PATH)?,
                    hotspot: (24, 24),
                },
                Shape {
                    bytes: load_shape(RESIZE_EW_PATH)?,
                    hotspot: (24, 24),
                },
                Shape {
                    bytes: load_shape(RESIZE_NESW_PATH)?,
                    hotspot: (24, 24),
                },
                Shape {
                    bytes: load_shape(RESIZE_NWSE_PATH)?,
                    hotspot: (24, 24),
                },
            ],
            active_shape: 0,
            backing: Vec::new(),
            saved: (0, 0, 0, 0),
        })
    }

    /// Selects the cursor shape to draw. Returns whether the shape changed, so
    /// the caller can trigger a redraw only on a real transition. Out-of-range
    /// shapes select the default arrow.
    pub fn set_shape(&mut self, shape: u32) -> bool {
        let next = if shape <= display_proto::CURSOR_NONE {
            shape as usize
        } else {
            0
        };
        let changed = next != self.active_shape;
        self.active_shape = next;
        changed
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
    pub fn overlay(&mut self, target: &mut DumbBuffer, x: i32, y: i32) -> Clip {
        if self.hidden() {
            self.saved = (0, 0, 0, 0);
            return clip(self.saved);
        }
        self.save(target, x, y);
        self.paint(target, x, y);
        clip(self.saved)
    }

    /// Removes the cursor before an in-place scene damage repaint.
    ///
    /// The returned clip must join the final DIRTYFB update. Without it, moving
    /// scene pixels beneath a stationary cursor can leave the old arrow cached
    /// in the host resource.
    pub fn remove(&mut self, target: &mut DumbBuffer) -> Clip {
        let old = self.saved;
        self.restore(target);
        self.saved = (0, 0, 0, 0);
        clip(old)
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
        if self.hidden() {
            self.saved = (0, 0, 0, 0);
            return [clip(old), clip(self.saved)];
        }
        self.save(target, x, y);
        self.paint(target, x, y);
        [clip(old), clip(self.saved)]
    }

    fn save(&mut self, target: &DumbBuffer, x: i32, y: i32) {
        let (x, y) = self.origin(x, y);
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
        let (x, y) = self.origin(x, y);
        let bytes = &self.shapes[self.active_shape].bytes;
        let x1 = x.max(0);
        let y1 = y.max(0);
        let x2 = (x + WIDTH as i32).min(target.width() as i32);
        let y2 = (y + HEIGHT as i32).min(target.height() as i32);
        for screen_y in y1..y2 {
            let local_y = (screen_y - y) as usize;
            let row = target.row_mut(screen_y as usize);
            for screen_x in x1..x2 {
                let local_x = (screen_x - x) as usize;
                // Each pixel is 4 LE bytes [B,G,R,A] → premultiplied 0xAARRGGBB.
                let index = HEADER + (local_y * WIDTH + local_x) * 4;
                let source = u32::from_le_bytes([
                    bytes[index],
                    bytes[index + 1],
                    bytes[index + 2],
                    bytes[index + 3],
                ]);
                row[screen_x as usize] = over(source, row[screen_x as usize]);
            }
        }
    }

    fn origin(&self, x: i32, y: i32) -> (i32, i32) {
        let hotspot = self.shapes[self.active_shape].hotspot;
        (x - hotspot.0, y - hotspot.1)
    }

    fn hidden(&self) -> bool {
        self.active_shape == display_proto::CURSOR_NONE as usize
    }
}

/// Clamps one cursor rectangle to the buffer, returning `(x1, y1, x2, y2)`.
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

/// Reads and validates one cursor asset's identity header and RGBA payload size.
fn load_shape(path: &str) -> io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let valid = bytes.len() == HEADER + PIXEL_BYTES
        && bytes.get(..8) == Some(MAGIC.as_slice())
        && read_u32(&bytes, 8) == Some(WIDTH as u32)
        && read_u32(&bytes, 12) == Some(HEIGHT as u32);
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cursor asset identity invalid",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{Cursor, HEADER, SHAPE_COUNT, Shape, WIDTH};
    use crate::scanout::over;

    fn cursor() -> Cursor {
        Cursor {
            shapes: std::array::from_fn(|index| Shape {
                bytes: Vec::new(),
                hotspot: if index >= display_proto::CURSOR_RESIZE_NS as usize {
                    (24, 24)
                } else {
                    (0, 0)
                },
            }),
            active_shape: 0,
            backing: Vec::new(),
            saved: (0, 0, 0, 0),
        }
    }

    #[test]
    fn resize_shape_centers_its_hotspot_on_the_pointer() {
        let mut cursor = cursor();

        assert!(cursor.set_shape(display_proto::CURSOR_RESIZE_NS));
        // 48px shape centered on the pointer: origin = pointer - (24, 24).
        assert_eq!(cursor.origin(100, 80), (76, 56));
    }

    #[test]
    fn unknown_shape_returns_to_the_default_arrow() {
        let mut cursor = cursor();
        assert_eq!(SHAPE_COUNT, 6);
        assert!(cursor.set_shape(display_proto::CURSOR_RESIZE_NWSE));

        assert!(cursor.set_shape(u32::MAX));
        assert_eq!(cursor.origin(100, 80), (100, 80));
    }

    #[test]
    fn hidden_shape_uses_no_bitmap_slot() {
        let mut cursor = cursor();

        assert!(cursor.set_shape(display_proto::CURSOR_NONE));
        assert!(cursor.hidden());
        assert_eq!(cursor.active_shape, SHAPE_COUNT);
    }

    /// Guards the two `.lc2` decode invariants `paint()` relies on: the 4 disk
    /// bytes are little-endian `[B, G, R, A]` (so `from_le_bytes` yields
    /// `0xAARRGGBB`), and pixels are premultiplied so they feed `over()` directly.
    /// `DumbBuffer` wraps a real DRM mmap with no test constructor, so this
    /// exercises the exact index/unpack/blend path over a raw asset byte slice.
    #[test]
    fn lc2_pixel_decode_matches_premultiplied_over() {
        let index = |x: usize, y: usize| HEADER + (y * WIDTH + x) * 4;
        // Row 0: opaque red, transparent, and a 50%-premultiplied red.
        let mut bytes = vec![0u8; HEADER + WIDTH * super::HEIGHT * 4];
        // opaque red (a=255, premultiplied r=255): [B,G,R,A] = [0,0,255,255]
        bytes[index(0, 0)..index(0, 0) + 4].copy_from_slice(&[0, 0, 255, 255]);
        // fully transparent: all zero (already)
        // 50% red premultiplied: a=128, r=128 → [B,G,R,A] = [0,0,128,128]
        bytes[index(2, 0)..index(2, 0) + 4].copy_from_slice(&[0, 0, 128, 128]);

        let unpack = |x: usize| {
            let i = index(x, 0);
            u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
        };
        let backdrop = 0x0000_2040; // some scene blue

        // Opaque source replaces the destination color (alpha 255 fast path).
        assert_eq!(over(unpack(0), backdrop), 0x00ff_0000);
        // Transparent source leaves the destination untouched.
        assert_eq!(over(unpack(1), backdrop), backdrop);
        // 50% premultiplied red over the backdrop blends both channels.
        assert_eq!(over(unpack(2), backdrop), over(0x8080_0000, backdrop));
    }
}
