//! `LCS1` 光标集：解码 `scripts/pack_cursors.py` 生成的数据，并把当前帧写进 64×64 硬件光标缓冲。

use std::io;

use linux_uapi::drm::DumbBuffer;

const MAGIC: &[u8; 4] = b"LCS1";
const NAME_BYTES: usize = 16;
const SIZES: [usize; 2] = [32, 64];
/// DRM 硬件光标缓冲的边长。
pub const HARDWARE_EDGE: usize = 64;

/// 光标集数据非法。
#[derive(Debug, PartialEq, Eq)]
pub enum CursorError {
    BadMagic,
    Truncated,
    BadName,
    NoFrames,
    TrailingBytes,
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BadMagic => "not an LCS1 cursor set",
            Self::Truncated => "cursor set is truncated",
            Self::BadName => "cursor name is not ASCII",
            Self::NoFrames => "cursor has no frames",
            Self::TrailingBytes => "cursor set has trailing bytes",
        })
    }
}

impl std::error::Error for CursorError {}

/// 一个光标：两种尺寸的预乘 ARGB 帧序列与热点。
pub struct Cursor {
    frames: usize,
    hotspots: [(u32, u32); 2],
    pixels: [Vec<u32>; 2],
}

impl Cursor {
    fn slot(scale: u32) -> usize {
        usize::from(scale >= 2)
    }

    /// `scale` 对应的热点：1x 用 32 像素素材，2x 及以上用 64 像素素材。
    pub fn hotspot(&self, scale: u32) -> (u32, u32) {
        self.hotspots[Self::slot(scale)]
    }

    /// 取 `scale` 对应尺寸的第 `frame` 帧（超出帧数则回绕），返回 `(边长, 像素)`。
    pub fn frame(&self, scale: u32, frame: usize) -> (usize, &[u32]) {
        let slot = Self::slot(scale);
        let edge = SIZES[slot];
        let start = (frame % self.frames) * edge * edge;
        (edge, &self.pixels[slot][start..start + edge * edge])
    }
}

/// 全部光标，按名字查找。
pub struct CursorSet {
    cursors: Vec<(String, Cursor)>,
}

fn read<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N], CursorError> {
    let slice = bytes
        .get(*offset..*offset + N)
        .ok_or(CursorError::Truncated)?;
    *offset += N;
    Ok(slice.try_into().expect("slice has length N"))
}

fn read_u16(bytes: &[u8], offset: &mut usize) -> Result<u16, CursorError> {
    read::<2>(bytes, offset).map(u16::from_le_bytes)
}

impl CursorSet {
    /// 解码并校验整个光标集；任何长度不符都报错，不会读到越界数据。
    ///
    /// # Errors
    ///
    /// 魔数、名字、帧数或长度不符合 `LCS1`。
    pub fn parse(bytes: &[u8]) -> Result<Self, CursorError> {
        let mut offset = 0;
        if &read::<4>(bytes, &mut offset)? != MAGIC {
            return Err(CursorError::BadMagic);
        }
        let count = read_u16(bytes, &mut offset)?;
        read_u16(bytes, &mut offset)?;
        let mut cursors = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let name = read::<NAME_BYTES>(bytes, &mut offset)?;
            let length = name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(NAME_BYTES);
            let name = std::str::from_utf8(&name[..length])
                .ok()
                .filter(|name| name.is_ascii())
                .ok_or(CursorError::BadName)?
                .to_owned();
            let frames = usize::from(read_u16(bytes, &mut offset)?);
            // 帧间隔（毫秒）随素材提供，等忙碌光标动画接入时再使用；这里只确认字段存在。
            read_u16(bytes, &mut offset)?;
            if frames == 0 {
                return Err(CursorError::NoFrames);
            }
            let mut hotspots = [(0, 0); 2];
            for hotspot in &mut hotspots {
                *hotspot = (
                    u32::from(read_u16(bytes, &mut offset)?),
                    u32::from(read_u16(bytes, &mut offset)?),
                );
            }
            let mut pixels = [Vec::new(), Vec::new()];
            for (slot, edge) in SIZES.into_iter().enumerate() {
                let length = edge * edge * frames * 4;
                let raw = bytes
                    .get(offset..offset + length)
                    .ok_or(CursorError::Truncated)?;
                offset += length;
                pixels[slot] = raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| u32::from_le_bytes(*chunk))
                    .collect();
            }
            cursors.push((
                name,
                Cursor {
                    frames,
                    hotspots,
                    pixels,
                },
            ));
        }
        if offset != bytes.len() {
            return Err(CursorError::TrailingBytes);
        }
        Ok(Self { cursors })
    }

    pub fn get(&self, name: &str) -> Option<&Cursor> {
        self.cursors
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, cursor)| cursor)
    }
}

/// 缩放抓取时显示的方向箭头：按被抓住的边选择光标名。
pub fn resize_cursor_name(edges: crate::interaction::Edges) -> &'static str {
    match (edges.left, edges.right, edges.top, edges.bottom) {
        (true, _, true, _) | (_, true, _, true) => "resize-nwse",
        (_, true, true, _) | (true, _, _, true) => "resize-nesw",
        (true, _, _, _) | (_, true, _, _) => "resize-ew",
        _ => "resize-ns",
    }
}

/// 把客户端光标图像按整数倍 `factor` 放大后写入 64×64 硬件光标缓冲，其余全透明。
///
/// # Errors
///
/// 缓冲不是 64 × 64，或放大后超出 64 × 64（调用者应先回退到默认光标）。
pub fn blit_scaled(
    target: &mut DumbBuffer,
    pixels: &[u32],
    width: usize,
    height: usize,
    factor: usize,
) -> io::Result<()> {
    let (scaled_width, scaled_height) = (width * factor, height * factor);
    if target.width() != HARDWARE_EDGE
        || target.height() != HARDWARE_EDGE
        || scaled_width > HARDWARE_EDGE
        || scaled_height > HARDWARE_EDGE
        || pixels.len() != width * height
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cursor image does not fit the 64x64 hardware cursor",
        ));
    }
    for row in 0..HARDWARE_EDGE {
        let line = target.row_mut(row);
        line.fill(0);
        if row < scaled_height {
            let source = &pixels[(row / factor) * width..][..width];
            for (column, pixel) in line[..scaled_width].iter_mut().enumerate() {
                *pixel = source[column / factor];
            }
        }
    }
    Ok(())
}

/// 把 `frame` 写入 64×64 硬件光标缓冲：素材放在左上角，其余全透明。
///
/// # Errors
///
/// 缓冲不是 64 × 64。
pub fn blit_to_hardware(target: &mut DumbBuffer, edge: usize, frame: &[u32]) -> io::Result<()> {
    if target.width() != HARDWARE_EDGE || target.height() != HARDWARE_EDGE || edge > HARDWARE_EDGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hardware cursor buffer must be 64x64",
        ));
    }
    for row in 0..HARDWARE_EDGE {
        let line = target.row_mut(row);
        line.fill(0);
        if row < edge {
            line[..edge].copy_from_slice(&frame[row * edge..(row + 1) * edge]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
