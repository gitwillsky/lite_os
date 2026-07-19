use alloc::{boxed::Box, vec::Vec};
use core::{
    mem::MaybeUninit,
    ops::{Deref, DerefMut, Range},
};

#[cfg(not(test))]
use crate::memory::{KERNEL_SPACE, PAGE_SIZE, VirtualAddress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// fixed DMA mapping 在 descriptor publication 前的构造错误。
pub(in crate::drivers) enum DmaMappingError {
    /// bytes 或 segment metadata 无法预留。
    OutOfMemory,
    /// kernel page table 不包含 buffer 的某一页。
    Unmapped,
    /// range、长度或 page size 无法形成有效 segment。
    InvalidRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// cached DMA chain 相对当前 free descriptor capacity 的完整判定。
pub(super) enum DmaChainRequirement {
    /// chain 非空且可由给定数量的 descriptor 表示。
    Required(usize),
    /// caller 未提供任何有效 segment。
    Empty,
    /// segment 总数溢出或超过当前 capacity。
    ExceedsCapacity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DmaSegment {
    physical: u64,
    offset: usize,
    length: usize,
}

/// 固定 bytes 与其 kernel-VA→physical segment proof 的共同 owner。
///
/// moving 只移动 Box handle；allocation 在本对象 Drop 前地址稳定。adapter 必须在 Drop 本对象
/// 前 reset device，并以 slot state 排除 CPU 与 device 对 writable range 的并发访问。
pub(in crate::drivers) struct DmaBuffer<const SIZE: usize> {
    bytes: Box<[u8; SIZE]>,
    segments: Box<[DmaSegment]>,
}

/// @description 由 device 完整初始化的 fixed DMA bytes 与 mapping proof 的共同 owner。
///
/// bytes 从未在提交前预零；adapter 只能在 used-ring 验证 descriptor identity 与 returned
/// length 后投影 initialized prefix。Drop 前必须 reset device，避免 device 写入已释放 allocation。
pub(in crate::drivers) struct DeviceWriteBuffer<const SIZE: usize> {
    bytes: Box<[MaybeUninit<u8>]>,
    segments: Box<[DmaSegment]>,
}

impl<const SIZE: usize> DeviceWriteBuffer<SIZE> {
    #[cfg(not(test))]
    /// @description 分配未初始化 fixed bytes 并缓存完整 kernel-VA→physical mapping。
    /// @return 成功时返回地址稳定的 DMA owner。
    /// @errors allocation、mapping、range 或 kernel translation 失败时返回 `DmaMappingError`。
    pub(in crate::drivers) fn try_uninit() -> Result<Self, DmaMappingError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(SIZE)
            .map_err(|_| DmaMappingError::OutOfMemory)?;
        // SAFETY: `MaybeUninit<u8>` has no initialization invariant. Capacity was reserved for
        // exactly SIZE elements and no byte is exposed as `u8` until device completion validation.
        // Omitting this length publication would leave the DMA allocation logically empty.
        unsafe { bytes.set_len(SIZE) };
        let bytes = bytes.into_boxed_slice();
        let start = bytes.as_ptr() as usize;
        let kernel_space = KERNEL_SPACE.wait().lock();
        let segments = map_segments_with(start, SIZE, PAGE_SIZE, |address| {
            kernel_space
                .translate_kernel_address(VirtualAddress::from(address))
                .map(|physical| physical.as_usize() as u64)
        })?;
        drop(kernel_space);
        Ok(Self {
            bytes,
            segments: segments.into_boxed_slice(),
        })
    }

    /// @description 投影已由 adapter 验证为非空且 bounded 的 device-write prefix。
    /// @param length descriptor 允许 device 写入的 prefix 长度，必须在 `1..=SIZE`。
    /// @return 借用当前 mapping 的 writable descriptor segments。
    pub(in crate::drivers) fn writable_prefix(&self, length: usize) -> DmaSlice<'_> {
        assert!(length != 0 && length <= SIZE);
        DmaSlice {
            segments: &self.segments,
            range: 0..length,
            device_writable: true,
        }
    }

    /// @description 投影已由 device 初始化的 prefix。
    /// @param length used ring 已证明由 device 写入的 prefix 长度。
    /// @return 与固定 DMA owner 共同存活的 initialized byte slice。
    ///
    /// # Safety
    ///
    /// Caller 必须已从 used ring 验证：当前 generation 的同一 descriptor 完成，returned
    /// length 至少为 `length`，且 CPU 已重新取得 descriptor ownership。若证明缺失，读取
    /// 未初始化内存会产生 undefined behavior。
    /// SAFETY: the caller contract above is the only route from device-writable storage to bytes.
    pub(in crate::drivers) unsafe fn initialized_prefix(&self, length: usize) -> &[u8] {
        assert!(length <= SIZE);
        // SAFETY: caller contract proves every projected byte initialized; MaybeUninit and u8 have
        // identical layout, and the borrow cannot outlive this stable boxed allocation.
        unsafe { core::slice::from_raw_parts(self.bytes.as_ptr().cast::<u8>(), length) }
    }
}

impl<const SIZE: usize> DmaBuffer<SIZE> {
    #[cfg(not(test))]
    pub(in crate::drivers) fn try_zeroed() -> Result<Self, DmaMappingError> {
        let bytes = Box::try_new([0; SIZE]).map_err(|_| DmaMappingError::OutOfMemory)?;
        Self::try_from_box(bytes)
    }

    #[cfg(not(test))]
    fn try_from_box(bytes: Box<[u8; SIZE]>) -> Result<Self, DmaMappingError> {
        let start = bytes.as_ptr() as usize;
        let kernel_space = KERNEL_SPACE.wait().lock();
        let segments = map_segments_with(start, SIZE, PAGE_SIZE, |address| {
            kernel_space
                .translate_kernel_address(VirtualAddress::from(address))
                .map(|physical| physical.as_usize() as u64)
        })?;
        drop(kernel_space);
        Ok(Self {
            bytes,
            segments: segments.into_boxed_slice(),
        })
    }

    pub(in crate::drivers) fn as_slice(&self) -> &[u8] {
        &self.bytes[..]
    }

    pub(in crate::drivers) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes[..]
    }

    pub(in crate::drivers) fn readable(
        &self,
        range: Range<usize>,
    ) -> Result<DmaSlice<'_>, DmaMappingError> {
        self.slice(range, false)
    }

    pub(in crate::drivers) fn readable_all(&self) -> DmaSlice<'_> {
        self.whole(false)
    }

    pub(in crate::drivers) fn writable_all(&self) -> DmaSlice<'_> {
        self.whole(true)
    }

    fn whole(&self, device_writable: bool) -> DmaSlice<'_> {
        assert!(SIZE != 0, "zero-sized DMA buffer has no descriptor");
        DmaSlice {
            segments: &self.segments,
            range: 0..SIZE,
            device_writable,
        }
    }

    fn slice(
        &self,
        range: Range<usize>,
        device_writable: bool,
    ) -> Result<DmaSlice<'_>, DmaMappingError> {
        if range.start >= range.end || range.end > SIZE {
            return Err(DmaMappingError::InvalidRange);
        }
        Ok(DmaSlice {
            segments: &self.segments,
            range,
            device_writable,
        })
    }
}

impl<const SIZE: usize> AsRef<[u8; SIZE]> for DmaBuffer<SIZE> {
    fn as_ref(&self) -> &[u8; SIZE] {
        &self.bytes
    }
}

impl<const SIZE: usize> AsMut<[u8; SIZE]> for DmaBuffer<SIZE> {
    fn as_mut(&mut self) -> &mut [u8; SIZE] {
        &mut self.bytes
    }
}

impl<const SIZE: usize> Deref for DmaBuffer<SIZE> {
    type Target = [u8; SIZE];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl<const SIZE: usize> DerefMut for DmaBuffer<SIZE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes
    }
}

#[derive(Clone)]
/// 从一个 live `DmaBuffer` 投影出的 descriptor range 与 device access direction。
///
/// 该 borrow 只证明 publication 时 backing/mapping 存活；adapter 的 completion/reset state 必须继续
/// 保持 owner，直到 device 不再拥有 descriptor。
pub(in crate::drivers) struct DmaSlice<'mapping> {
    segments: &'mapping [DmaSegment],
    range: Range<usize>,
    device_writable: bool,
}

impl DmaSlice<'_> {
    pub(super) fn segment_count(&self) -> usize {
        self.segments
            .iter()
            .filter(|segment| segment_overlaps(segment, &self.range))
            .count()
    }

    pub(super) fn for_each_segment(&self, mut visit: impl FnMut(u64, usize, bool)) {
        for segment in self
            .segments
            .iter()
            .filter(|segment| segment_overlaps(segment, &self.range))
        {
            let start = segment.offset.max(self.range.start);
            let end = (segment.offset + segment.length).min(self.range.end);
            visit(
                segment.physical + (start - segment.offset) as u64,
                end - start,
                self.device_writable,
            );
        }
    }
}

/// 以 cached segment 数验证一条 chain 是否能装入当前 free descriptor capacity。
pub(super) fn descriptor_requirement(
    buffers: &[DmaSlice<'_>],
    capacity: usize,
) -> DmaChainRequirement {
    let Some(required) = buffers.iter().try_fold(0usize, |count, buffer| {
        count.checked_add(buffer.segment_count())
    }) else {
        return DmaChainRequirement::ExceedsCapacity;
    };
    if required == 0 {
        DmaChainRequirement::Empty
    } else if required > capacity {
        DmaChainRequirement::ExceedsCapacity
    } else {
        DmaChainRequirement::Required(required)
    }
}

fn segment_overlaps(segment: &DmaSegment, range: &Range<usize>) -> bool {
    segment.offset < range.end && segment.offset + segment.length > range.start
}

fn map_segments_with(
    start: usize,
    length: usize,
    page_size: usize,
    mut translate: impl FnMut(usize) -> Option<u64>,
) -> Result<Vec<DmaSegment>, DmaMappingError> {
    if length == 0 || page_size == 0 || !page_size.is_power_of_two() {
        return Err(DmaMappingError::InvalidRange);
    }
    let pages = (start % page_size)
        .checked_add(length)
        .ok_or(DmaMappingError::InvalidRange)?
        .div_ceil(page_size);
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(pages)
        .map_err(|_| DmaMappingError::OutOfMemory)?;
    let mut offset = 0usize;
    while offset < length {
        let address = start
            .checked_add(offset)
            .ok_or(DmaMappingError::InvalidRange)?;
        let page_offset = address % page_size;
        let chunk = (length - offset).min(page_size - page_offset);
        let physical = translate(address).ok_or(DmaMappingError::Unmapped)?;
        segments.push(DmaSegment {
            physical,
            offset,
            length: chunk,
        });
        offset += chunk;
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::{
        DmaChainRequirement, DmaMappingError, DmaSlice, descriptor_requirement, map_segments_with,
    };

    #[test]
    fn cross_page_mapping_preserves_each_physical_segment() {
        let segments =
            map_segments_with(4094, 6, 4096, |address| Some(address as u64 + 0x1000)).unwrap();
        assert_eq!(segments.len(), 2);
        let slice = DmaSlice {
            segments: &segments,
            range: 0..6,
            device_writable: false,
        };
        let mut observed = Vec::new();
        slice.for_each_segment(|physical, length, writable| {
            observed.push((physical, length, writable));
        });
        assert_eq!(observed, [(0x1ffe, 2, false), (0x2000, 4, false)]);
    }

    #[test]
    fn translation_failure_discards_the_partial_mapping() {
        assert_eq!(
            map_segments_with(4094, 6, 4096, |address| {
                (address < 4096).then_some(address as u64)
            }),
            Err(DmaMappingError::Unmapped)
        );
    }

    #[test]
    fn subrange_counts_only_overlapping_segments() {
        let segments = map_segments_with(4094, 8196, 4096, |address| Some(address as u64)).unwrap();
        let slice = DmaSlice {
            segments: &segments,
            range: 1..4099,
            device_writable: true,
        };
        assert_eq!(slice.segment_count(), 3);
    }

    #[test]
    fn descriptor_capacity_is_checked_from_cached_segments() {
        let segments = map_segments_with(4094, 8196, 4096, |address| Some(address as u64)).unwrap();
        let first = DmaSlice {
            segments: &segments,
            range: 0..2,
            device_writable: false,
        };
        let second = DmaSlice {
            segments: &segments,
            range: 2..8196,
            device_writable: true,
        };
        assert_eq!(
            descriptor_requirement(&[first.clone(), second.clone()], 4),
            DmaChainRequirement::Required(4)
        );
        assert_eq!(
            descriptor_requirement(&[first, second], 3),
            DmaChainRequirement::ExceedsCapacity
        );
        assert_eq!(descriptor_requirement(&[], 8), DmaChainRequirement::Empty);
    }
}
