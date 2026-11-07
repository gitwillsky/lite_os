//! 整窗重绘的损伤推导：对上一次提交画面的分块哈希，不依赖任何旧缓冲的内容。
//!
//! 窗口直接在已释放的共享缓冲里整幅绘制时，缓冲里没有可比较的上一帧（可能是更早的缓冲，也可能
//! 刚被覆盖）。每次提交后只保留每块一个 64 位哈希，下一帧绘制完成后逐块比较即可得到变化区域，
//! 内存只有画面的约 1/1000，而不是再保留一份整窗像素。

use canvas::Rect;

/// 分块宽度（像素）；4 KiB 的块内容既能让哈希摊销循环开销，又不会把小改动放大成大矩形。
const TILE_WIDTH: usize = 64;
/// 分块高度（行）。
const TILE_HEIGHT: usize = 16;
/// 单次提交最多声明的损伤矩形数；超过后合并成外接矩形，只会多传、不会漏传。
const DAMAGE_LIMIT: usize = 16;

/// 一幅画面按固定分块得到的哈希。
pub(super) struct Tiles {
    /// 画面的像素宽高，也是哈希表的几何；几何不同的两个表不可比较。
    size: (usize, usize),
    hashes: Vec<u64>,
}

impl Tiles {
    /// 计算 `pixels`（行距等于 `width`）的分块哈希。
    ///
    /// # Errors
    /// 哈希表分配失败返回 `None`，调用者按整幅损伤处理。
    pub(super) fn of(pixels: &[u32], width: usize, height: usize) -> Option<Self> {
        let columns = width.div_ceil(TILE_WIDTH);
        let rows = height.div_ceil(TILE_HEIGHT);
        let mut hashes = Vec::new();
        hashes.try_reserve_exact(columns * rows).ok()?;
        for row in 0..rows {
            let top = row * TILE_HEIGHT;
            let bottom = (top + TILE_HEIGHT).min(height);
            for column in 0..columns {
                let left = column * TILE_WIDTH;
                let right = (left + TILE_WIDTH).min(width);
                hashes.push(hash_tile(pixels, width, left..right, top..bottom));
            }
        }
        Some(Self {
            size: (width, height),
            hashes,
        })
    }

    /// 相对 `previous` 变化的区域；几何不同返回 `None`（整幅都视为变化）。
    pub(super) fn changed_from(&self, previous: &Self) -> Option<Vec<Rect>> {
        if self.size != previous.size {
            return None;
        }
        let (width, height) = self.size;
        let columns = width.div_ceil(TILE_WIDTH);
        let mut rects: Vec<Rect> = Vec::new();
        for (row, (now, before)) in self
            .hashes
            .chunks_exact(columns)
            .zip(previous.hashes.chunks_exact(columns))
            .enumerate()
        {
            let top = row * TILE_HEIGHT;
            let bottom = (top + TILE_HEIGHT).min(height);
            let mut column = 0;
            while column < columns {
                if now[column] == before[column] {
                    column += 1;
                    continue;
                }
                let start = column;
                while column < columns && now[column] != before[column] {
                    column += 1;
                }
                let left = start * TILE_WIDTH;
                let right = (column * TILE_WIDTH).min(width);
                let (x, y) = (left as i32, top as i32);
                let (w, h) = ((right - left) as i32, (bottom - top) as i32);
                // 上一行条带与本行条带横向范围相同时纵向并入，整块变化不会产生一排矩形。
                match rects.last_mut() {
                    Some(last) if last.x == x && last.width == w && last.bottom() == y => {
                        last.height += h;
                    }
                    _ => rects.push(Rect::new(x, y, w, h)),
                }
            }
        }
        if rects.len() > DAMAGE_LIMIT {
            let bounds = rects.iter().skip(1).fold(rects[0], |bounds, rect| {
                let (left, top) = (bounds.x.min(rect.x), bounds.y.min(rect.y));
                let right = bounds.right().max(rect.right());
                let bottom = bounds.bottom().max(rect.bottom());
                Rect::new(left, top, right - left, bottom - top)
            });
            rects = vec![bounds];
        }
        Some(rects)
    }
}

/// 一个分块的 64 位哈希：逐像素对折入乘法混合；仅用于比较同一窗口相邻两帧的变化。
fn hash_tile(
    pixels: &[u32],
    width: usize,
    columns: core::ops::Range<usize>,
    rows: core::ops::Range<usize>,
) -> u64 {
    const MIX: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for row in rows {
        let line = &pixels[row * width + columns.start..row * width + columns.end];
        let (pairs, tail) = line.as_chunks::<2>();
        for pair in pairs {
            let word = u64::from(pair[0]) | u64::from(pair[1]) << 32;
            hash = (hash ^ word).wrapping_mul(MIX).rotate_left(27);
        }
        for &pixel in tail {
            hash = (hash ^ u64::from(pixel)).wrapping_mul(MIX).rotate_left(27);
        }
        // 行尾混入长度：不同宽度的相同像素序列不会撞出同一哈希。
        hash = (hash ^ line.len() as u64).wrapping_mul(MIX);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{TILE_HEIGHT, TILE_WIDTH, Tiles};
    use canvas::Rect;

    #[test]
    fn an_identical_frame_has_no_damage_and_a_changed_pixel_damages_only_its_tile() {
        let (width, height) = (TILE_WIDTH * 3 + 5, TILE_HEIGHT * 2 + 3);
        let before = vec![7u32; width * height];
        let mut after = before.clone();
        let previous = Tiles::of(&before, width, height).unwrap();
        assert_eq!(
            Tiles::of(&after, width, height)
                .unwrap()
                .changed_from(&previous),
            Some(Vec::new())
        );
        // 最后一列的残余分块（宽 5）内一个像素变化：只损伤该列、该行的分块。
        after[(TILE_HEIGHT + 1) * width + TILE_WIDTH * 3 + 2] = 9;
        let damage = Tiles::of(&after, width, height)
            .unwrap()
            .changed_from(&previous)
            .unwrap();
        assert_eq!(
            damage,
            [Rect::new(
                (TILE_WIDTH * 3) as i32,
                TILE_HEIGHT as i32,
                5,
                TILE_HEIGHT as i32
            )]
        );
    }

    #[test]
    fn adjacent_changed_tiles_merge_and_a_size_change_has_no_baseline() {
        let (width, height) = (TILE_WIDTH * 2, TILE_HEIGHT * 2);
        let before = vec![0u32; width * height];
        let after = vec![1u32; width * height];
        let previous = Tiles::of(&before, width, height).unwrap();
        assert_eq!(
            Tiles::of(&after, width, height)
                .unwrap()
                .changed_from(&previous),
            Some(vec![Rect::new(0, 0, width as i32, height as i32)]),
            "a fully changed frame is one rectangle, not one per tile"
        );
        let other = Tiles::of(&after[..width * TILE_HEIGHT], width, TILE_HEIGHT).unwrap();
        assert!(other.changed_from(&previous).is_none());
    }
}
