//! 尺寸级别：每个二的幂区间内 8 个等差级别，内部碎片不超过 12.5%。
//!
//! 16–128 字节按 16 字节等差（8 级）；其后每个区间 `(base, 2·base]` 分 8 级，间距为 `base / 8`，
//! 直到 [`MAX_SMALL`]。间距过粗会让常见的 Vec/String 增长落在平均 25% 的浪费上。

/// 最小块大小，同时是所有块的对齐保证。
pub const MIN_BLOCK: usize = 16;
/// 小对象上限；更大的请求使用独立映射。
pub const MAX_SMALL: usize = 256 * 1024;
/// 级别总数：8 个线性级别加 (128 → 256 KiB) 共 11 个区间 × 8 级。
pub const CLASS_COUNT: usize = 8 + 11 * 8;

/// 级别 `class` 的块大小（字节）。
pub const fn block_size(class: usize) -> usize {
    if class < 8 {
        MIN_BLOCK * (class + 1)
    } else {
        let group = (class - 8) / 8;
        let step_index = (class - 8) % 8;
        let base = 128usize << group;
        base + (step_index + 1) * (base / 8)
    }
}

/// 能容纳 `size` 字节的最小级别；`size` 必须不超过 [`MAX_SMALL`]。
pub const fn class_of(size: usize) -> usize {
    if size <= 128 {
        let size = if size == 0 { 1 } else { size };
        return size.div_ceil(MIN_BLOCK) - 1;
    }
    let shifted = size - 1;
    let most_significant = (usize::BITS - 1 - shifted.leading_zeros()) as usize;
    let group = most_significant - 7;
    let base = 128usize << group;
    let step = base / 8;
    8 + group * 8 + (shifted - base) / step
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_maps_to_the_smallest_class_that_fits_within_the_waste_bound() {
        assert_eq!(block_size(CLASS_COUNT - 1), MAX_SMALL);
        for size in 1..=MAX_SMALL {
            let class = class_of(size);
            let block = block_size(class);
            assert!(block >= size, "size {size} class {class} block {block}");
            if class > 0 {
                assert!(block_size(class - 1) < size, "size {size} is not minimal");
            }
            // 内部碎片：浪费不超过块的 1/8（线性区间按 16 字节对齐，最小块除外）。
            if size > 128 {
                assert!((block - size) * 8 <= block, "size {size} wastes too much");
            }
        }
    }

    #[test]
    fn power_of_two_sizes_are_classes_so_blocks_keep_their_natural_alignment() {
        let mut size = MIN_BLOCK;
        while size <= MAX_SMALL {
            assert_eq!(block_size(class_of(size)), size);
            size *= 2;
        }
    }
}
