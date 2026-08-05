//! One standard 2D DRM cursor buffer presented through the VirtIO-GPU hardware cursor queue.

use std::io;

use linux_uapi::drm::{DrmDevice, DumbBuffer, Topology};

const PATHS: [&str; 6] = [
    "/usr/share/liteos/cursor.lc2",
    "/usr/share/liteos/cursor-pointer.lc2",
    "/usr/share/liteos/cursor-resize-ns.lc2",
    "/usr/share/liteos/cursor-resize-ew.lc2",
    "/usr/share/liteos/cursor-resize-nesw.lc2",
    "/usr/share/liteos/cursor-resize-nwse.lc2",
];
const MAGIC: &[u8; 8] = b"LCR2\0\0\0\x02";
const ASSET_WIDTH: usize = 48;
const ASSET_HEIGHT: usize = 48;
const CURSOR_SIZE: usize = 64;
const HEADER: usize = 24;
const PIXEL_BYTES: usize = ASSET_WIDTH * ASSET_HEIGHT * 4;
const SHAPE_COUNT: usize = PATHS.len();

struct Shape {
    pixels: Box<[u8]>,
    hotspot: (u32, u32),
}

/// Selected cursor and its single mutable 2D hardware resource.
pub struct Cursor {
    shapes: [Shape; SHAPE_COUNT],
    buffer: DumbBuffer,
    active_shape: usize,
}

impl Cursor {
    /// Validates every system cursor and initializes the sole 2D cursor buffer.
    pub fn open(device: &DrmDevice) -> io::Result<Self> {
        let mut loaded = Vec::with_capacity(SHAPE_COUNT);
        for path in PATHS {
            loaded.push(load_shape(path)?);
        }
        let shapes: [Shape; SHAPE_COUNT] = loaded
            .try_into()
            .map_err(|_| io::Error::other("cursor shape count changed during upload"))?;
        let mut cursor = Self {
            shapes,
            buffer: device.create_dumb(CURSOR_SIZE as u32, CURSOR_SIZE as u32)?,
            active_shape: display_proto::CURSOR_DEFAULT as usize,
        };
        cursor.copy_active_shape();
        Ok(cursor)
    }

    /// Selects one protocol cursor shape, returning whether it changed.
    pub fn set_shape(&mut self, shape: u32) -> bool {
        let next = if shape <= display_proto::CURSOR_NONE {
            shape as usize
        } else {
            display_proto::CURSOR_DEFAULT as usize
        };
        let changed = next != self.active_shape;
        self.active_shape = next;
        if changed && next < SHAPE_COUNT {
            self.copy_active_shape();
        }
        changed
    }

    fn copy_active_shape(&mut self) {
        let pixels = &self.shapes[self.active_shape].pixels;
        self.buffer.bytes_mut()[..pixels.len()].copy_from_slice(pixels);
    }

    /// Publishes the selected image and hotspot on the hardware cursor plane.
    pub fn present(
        &self,
        device: &DrmDevice,
        topology: &Topology,
        position: (i32, i32),
    ) -> io::Result<()> {
        if self.active_shape >= SHAPE_COUNT {
            device.update_cursor(topology, None, position, (0, 0))?;
            return Ok(());
        }
        let shape = &self.shapes[self.active_shape];
        device.update_cursor(
            topology,
            Some(&self.buffer),
            position,
            shape.hotspot,
        )
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
fn decode_shape(bytes: &[u8]) -> io::Result<Shape> {
    let hot_x = read_u32(bytes, 16);
    let hot_y = read_u32(bytes, 20);
    let valid = bytes.len() == HEADER + PIXEL_BYTES
        && bytes.get(..8) == Some(MAGIC.as_slice())
        && read_u32(bytes, 8) == Some(ASSET_WIDTH as u32)
        && read_u32(bytes, 12) == Some(ASSET_HEIGHT as u32)
        && hot_x.is_some_and(|value| value < ASSET_WIDTH as u32)
        && hot_y.is_some_and(|value| value < ASSET_HEIGHT as u32);
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cursor asset identity invalid",
        ));
    }
    let mut pixels = vec![0; CURSOR_SIZE * CURSOR_SIZE * 4].into_boxed_slice();
    for row in 0..ASSET_HEIGHT {
        let source = HEADER + row * ASSET_WIDTH * 4;
        let target = row * CURSOR_SIZE * 4;
        pixels[target..target + ASSET_WIDTH * 4]
            .copy_from_slice(&bytes[source..source + ASSET_WIDTH * 4]);
    }
    Ok(Shape {
        pixels,
        hotspot: (
            hot_x.expect("validated cursor hot_x disappeared"),
            hot_y.expect("validated cursor hot_y disappeared"),
        ),
    })
}

fn load_shape(path: &str) -> io::Result<Shape> {
    decode_shape(&std::fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::decode_shape;

    #[test]
    fn checked_assets_preserve_semantic_hotspots() {
        let assets: &[(&[u8], (u32, u32))] = &[
            (include_bytes!("../../../assets/cursor.lc2"), (0, 0)),
            (
                include_bytes!("../../../assets/cursor-pointer.lc2"),
                (18, 0),
            ),
            (
                include_bytes!("../../../assets/cursor-resize-ns.lc2"),
                (24, 24),
            ),
            (
                include_bytes!("../../../assets/cursor-resize-ew.lc2"),
                (24, 24),
            ),
            (
                include_bytes!("../../../assets/cursor-resize-nesw.lc2"),
                (24, 24),
            ),
            (
                include_bytes!("../../../assets/cursor-resize-nwse.lc2"),
                (24, 24),
            ),
        ];
        for &(asset, hotspot) in assets {
            let shape = decode_shape(asset).unwrap();
            assert_eq!(shape.hotspot, hotspot);
            let alpha = shape.pixels
                [(hotspot.1 as usize * super::CURSOR_SIZE + hotspot.0 as usize) * 4 + 3];
            assert!(alpha >= 192, "cursor hotspot must land on visible ink");
        }
    }

    #[test]
    fn legacy_asset_without_hotspot_is_rejected() {
        let mut asset = include_bytes!("../../../assets/cursor.lc2").to_vec();
        asset[7] = 1;
        let error = match decode_shape(&asset) {
            Ok(_) => panic!("legacy cursor asset was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn protocol_visible_shape_count_matches_assets() {
        assert_eq!(
            super::SHAPE_COUNT,
            display_proto::CURSOR_RESIZE_NWSE as usize + 1
        );
    }
}
