//! GPU cursor textures owned by the compositor.

use std::io;

use display_proto::{Rect, TextureRect};
use linux_uapi::drm::{VirglContext, VirglResource};

use crate::gpu::TextureLayer;

const PATHS: [&str; 6] = [
    "/usr/share/liteos/cursor.lc2",
    "/usr/share/liteos/cursor-pointer.lc2",
    "/usr/share/liteos/cursor-resize-ns.lc2",
    "/usr/share/liteos/cursor-resize-ew.lc2",
    "/usr/share/liteos/cursor-resize-nesw.lc2",
    "/usr/share/liteos/cursor-resize-nwse.lc2",
];
const MAGIC: &[u8; 8] = b"LCR2\0\0\0\x01";
const WIDTH: usize = 48;
const HEIGHT: usize = 48;
const HEADER: usize = 16;
const PIXEL_BYTES: usize = WIDTH * HEIGHT * 4;
const SHAPE_COUNT: usize = PATHS.len();

struct Shape {
    texture: VirglResource,
    hotspot: (i32, i32),
}

/// Selected cursor and its immutable GPU textures.
pub struct Cursor {
    shapes: [Shape; SHAPE_COUNT],
    active_shape: usize,
}

impl Cursor {
    /// Validates and uploads every system cursor once.
    pub fn open(graphics: &VirglContext) -> io::Result<Self> {
        let mut loaded = Vec::with_capacity(SHAPE_COUNT);
        for (index, path) in PATHS.iter().enumerate() {
            let bytes = load_shape(path)?;
            let mut texture = graphics.create_texture(WIDTH as u32, HEIGHT as u32)?;
            texture.bytes_mut().copy_from_slice(&bytes[HEADER..]);
            texture.transfer_to_host(0, 0, WIDTH as u32, HEIGHT as u32)?;
            loaded.push(Shape {
                texture,
                hotspot: if index >= display_proto::CURSOR_RESIZE_NS as usize {
                    (24, 24)
                } else {
                    (0, 0)
                },
            });
        }
        let shapes: [Shape; SHAPE_COUNT] = loaded
            .try_into()
            .map_err(|_| io::Error::other("cursor shape count changed during upload"))?;
        Ok(Self {
            shapes,
            active_shape: display_proto::CURSOR_DEFAULT as usize,
        })
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
        changed
    }

    /// Returns the cursor as the final GPU layer, or none for `cursor: none`.
    pub fn layer(&self, x: i32, y: i32, screen: Rect) -> Option<TextureLayer<'_>> {
        if self.active_shape >= SHAPE_COUNT {
            return None;
        }
        let shape = &self.shapes[self.active_shape];
        Some(TextureLayer {
            texture: &shape.texture,
            source: TextureRect {
                x: 0.0,
                y: 0.0,
                width: WIDTH as f32,
                height: HEIGHT as f32,
            },
            bounds: Rect {
                x: x - shape.hotspot.0,
                y: y - shape.hotspot.1,
                width: WIDTH as u32,
                height: HEIGHT as u32,
            },
            clip: screen,
            clip_masks: &[],
            clip_offset: (0, 0),
            color: [1.0; 4],
            mode: crate::gpu::TextureMode::Color,
            sampling: crate::gpu::TextureSampling::Linear,
            wrap: crate::gpu::TextureWrap::Edge,
        })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

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
    #[test]
    fn resize_cursor_hotspot_is_centered() {
        assert_eq!(
            (24, 24),
            (super::WIDTH as i32 / 2, super::HEIGHT as i32 / 2)
        );
    }

    #[test]
    fn protocol_visible_shape_count_matches_assets() {
        assert_eq!(
            super::SHAPE_COUNT,
            display_proto::CURSOR_RESIZE_NWSE as usize + 1
        );
    }
}
