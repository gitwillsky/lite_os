//! virtio descriptor 引用的 DMA 缓冲。
//!
//! 1. 每个固定缓冲是一个 VMO，构造时以设备的 BTI 固定一次，得到设备地址区段（恒等映射下按物理
//!    连续性分段，IOMMU 下为一段连续 IOVA）；CPU 经服务地址空间中的映射访问同一页。
//! 2. 区段在构造时缓存，提交 descriptor 时只投影、不分配。
//! 3. 缓冲释放时撤销设备地址；adapter 必须先 reset 设备，保证设备不再访问。

use alloc::{boxed::Box, vec::Vec};
use core::ops::{Deref, DerefMut, Range};

#[cfg(not(test))]
use crate::memory::{DeviceBacking, DeviceExtent, PAGE_SIZE};
#[cfg(not(test))]
use native_runtime::Bti;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// fixed DMA mapping 在 descriptor publication 前的构造错误。
pub(in crate::drivers::virtio) enum DmaMappingError {
    /// VMO、映射或区段表无法分配。
    OutOfMemory,
    /// BTI 固定失败（IOVA 耗尽）。
    Unmapped,
    /// range 或长度无法形成有效 segment。
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
    device_address: u64,
    offset: usize,
    length: usize,
}

/// 把固定区段截成覆盖 `[0, length)` 的 segment 表。
#[cfg(not(test))]
fn segments_from_extents(extents: &[DeviceExtent], length: usize) -> Result<Box<[DmaSegment]>, DmaMappingError> {
    let mut segments = Vec::new();
    segments.try_reserve_exact(extents.len()).map_err(|_| DmaMappingError::OutOfMemory)?;
    let mut offset = 0usize;
    for extent in extents {
        if offset >= length {
            break;
        }
        let chunk = (extent.pages * PAGE_SIZE).min(length - offset);
        segments.push(DmaSegment {
            device_address: extent.device_address,
            offset,
            length: chunk,
        });
        offset += chunk;
    }
    Ok(segments.into_boxed_slice())
}

/// `SIZE` 字节的固定 DMA 存储：VMO、CPU 映射与设备区段。
#[cfg(not(test))]
struct FixedStorage {
    // OWNER: 持有 VMO 与固定记录：释放即撤销设备地址与 CPU 映射；`base`/`segments` 只在它存活期间有效。
    _backing: DeviceBacking,
    base: usize,
    segments: Box<[DmaSegment]>,
}

#[cfg(not(test))]
impl FixedStorage {
    fn allocate(size: usize, bti: &Bti) -> Result<Self, DmaMappingError> {
        if size == 0 {
            return Err(DmaMappingError::InvalidRange);
        }
        let backing = DeviceBacking::try_allocate(size.div_ceil(PAGE_SIZE))
            .ok_or(DmaMappingError::OutOfMemory)?;
        let extents = backing.pin(bti).map_err(|_| DmaMappingError::Unmapped)?;
        let segments = segments_from_extents(extents, size)?;
        // 映射只经本存储的拥有者访问，借用规则由 DmaBuffer 的 `&`/`&mut` 方法维持。
        let base = backing.cpu_address().ok_or(DmaMappingError::OutOfMemory)?;
        Ok(Self { _backing: backing, base, segments })
    }
}

/// 固定 bytes 与其设备区段的共同 owner。
///
/// 内容在 VMO 中，地址在本对象存活期间稳定。adapter 必须在 Drop 本对象前 reset device，
/// 并以 slot state 排除 CPU 与 device 对 writable range 的并发访问。
pub(in crate::drivers::virtio) struct DmaBuffer<const SIZE: usize> {
    #[cfg(not(test))]
    storage: FixedStorage,
    #[cfg(test)]
    bytes: Box<[u8; SIZE]>,
    #[cfg(test)]
    segments: Box<[DmaSegment]>,
}

/// 由 device 写入的 fixed DMA bytes 与设备区段的共同 owner。
///
/// adapter 只能在 used-ring 验证 descriptor identity 与 returned length 后投影 device 写入的前缀。
/// Drop 前必须 reset device，避免 device 写入已撤销的地址。
pub(in crate::drivers::virtio) struct DeviceWriteBuffer<const SIZE: usize> {
    #[cfg(not(test))]
    storage: FixedStorage,
}

impl<const SIZE: usize> DeviceWriteBuffer<SIZE> {
    #[cfg(not(test))]
    /// 分配 fixed bytes 并以设备 `bti` 固定。
    ///
    /// # Errors
    ///
    /// 分配、映射或固定失败时返回 `DmaMappingError`。
    pub(in crate::drivers::virtio) fn try_uninit(bti: &Bti) -> Result<Self, DmaMappingError> {
        Ok(Self {
            storage: FixedStorage::allocate(SIZE, bti)?,
        })
    }

    /// 投影已由 adapter 验证为非空且 bounded 的 device-write prefix。
    ///
    /// # Parameters
    ///
    /// - `length`: descriptor 允许 device 写入的 prefix 长度，必须在 `1..=SIZE`。
    ///
    /// # Returns
    ///
    /// 借用当前 mapping 的 writable descriptor segments。
    #[cfg(not(test))]
    pub(in crate::drivers::virtio) fn writable_prefix(&self, length: usize) -> DmaSlice<'_> {
        assert!(length != 0 && length <= SIZE);
        DmaSlice {
            source: DmaSource::Mapped(&self.storage.segments),
            range: 0..length,
            device_writable: true,
        }
    }

    /// 投影已由 device 写入的 prefix。
    ///
    /// # Parameters
    ///
    /// - `length`: used ring 已证明由 device 写入的 prefix 长度。
    ///
    /// # Safety
    ///
    /// Caller 必须已从 used ring 验证：当前 generation 的同一 descriptor 完成，returned
    /// length 至少为 `length`，且 CPU 已重新取得 descriptor ownership；否则读到的是设备仍可能写入的字节。
    #[cfg(not(test))]
    pub(in crate::drivers::virtio) unsafe fn initialized_prefix(&self, length: usize) -> &[u8] {
        assert!(length <= SIZE);
        // SAFETY: VMO 页零初始化且与本对象同寿命；caller 保证设备已交还该前缀。
        unsafe { core::slice::from_raw_parts(self.storage.base as *const u8, length) }
    }
}

impl<const SIZE: usize> DmaBuffer<SIZE> {
    /// 分配零内容 fixed bytes 并以设备 `bti` 固定。
    ///
    /// # Errors
    ///
    /// 分配、映射或固定失败时返回 `DmaMappingError`。
    #[cfg(not(test))]
    pub(in crate::drivers::virtio) fn try_zeroed(bti: &Bti) -> Result<Self, DmaMappingError> {
        Ok(Self {
            storage: FixedStorage::allocate(SIZE, bti)?,
        })
    }

    fn bytes(&self) -> &[u8; SIZE] {
        #[cfg(not(test))]
        // SAFETY: 映射至少 SIZE 字节、与本对象同寿命；`&self` 排除经本对象的并发可变访问。
        return unsafe { &*(self.storage.base as *const [u8; SIZE]) };
        #[cfg(test)]
        return &self.bytes;
    }

    fn bytes_mut(&mut self) -> &mut [u8; SIZE] {
        #[cfg(not(test))]
        // SAFETY: 同上，`&mut self` 保证独占。
        return unsafe { &mut *(self.storage.base as *mut [u8; SIZE]) };
        #[cfg(test)]
        return &mut self.bytes;
    }

    fn segments(&self) -> &[DmaSegment] {
        #[cfg(not(test))]
        return &self.storage.segments;
        #[cfg(test)]
        return &self.segments;
    }

    pub(in crate::drivers::virtio) fn as_slice(&self) -> &[u8] {
        &self.bytes()[..]
    }

    pub(in crate::drivers::virtio) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes_mut()[..]
    }

    pub(in crate::drivers::virtio) fn readable(
        &self,
        range: Range<usize>,
    ) -> Result<DmaSlice<'_>, DmaMappingError> {
        self.slice(range, false)
    }

    pub(in crate::drivers::virtio) fn readable_all(&self) -> DmaSlice<'_> {
        self.whole(false)
    }

    pub(in crate::drivers::virtio) fn writable_all(&self) -> DmaSlice<'_> {
        self.whole(true)
    }

    fn whole(&self, device_writable: bool) -> DmaSlice<'_> {
        assert!(SIZE != 0, "zero-sized DMA buffer has no descriptor");
        DmaSlice {
            source: DmaSource::Mapped(self.segments()),
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
            source: DmaSource::Mapped(self.segments()),
            range,
            device_writable,
        })
    }
}

impl<const SIZE: usize> AsRef<[u8; SIZE]> for DmaBuffer<SIZE> {
    fn as_ref(&self) -> &[u8; SIZE] {
        self.bytes()
    }
}

impl<const SIZE: usize> AsMut<[u8; SIZE]> for DmaBuffer<SIZE> {
    fn as_mut(&mut self) -> &mut [u8; SIZE] {
        self.bytes_mut()
    }
}

impl<const SIZE: usize> Deref for DmaBuffer<SIZE> {
    type Target = [u8; SIZE];

    fn deref(&self) -> &Self::Target {
        self.bytes()
    }
}

impl<const SIZE: usize> DerefMut for DmaBuffer<SIZE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.bytes_mut()
    }
}

#[derive(Clone)]
/// 从一个 live `DmaBuffer` 投影出的 descriptor range 与 device access direction。
///
/// 该 borrow 只证明 publication 时 backing/mapping 存活；adapter 的 completion/reset state 必须继续
/// 保持 owner，直到 device 不再拥有 descriptor。
pub(in crate::drivers::virtio) struct DmaSlice<'mapping> {
    source: DmaSource<'mapping>,
    range: Range<usize>,
    device_writable: bool,
}

/// descriptor segment 的来源：固定 buffer 的 cached 区段，或已固定的 device backing 区段。
#[derive(Clone)]
enum DmaSource<'mapping> {
    Mapped(&'mapping [DmaSegment]),
    /// 每个 extent 已是设备地址连续页；直接按 extent 生成 segment，提交锁内不分配 mapping 表。
    #[cfg(not(test))]
    Backing(&'mapping DeviceBacking),
}

impl DmaSource<'_> {
    fn for_each(&self, mut visit: impl FnMut(DmaSegment)) {
        match self {
            Self::Mapped(segments) => {
                for segment in *segments {
                    visit(*segment);
                }
            }
            #[cfg(not(test))]
            Self::Backing(backing) => {
                let mut offset = 0;
                for extent in (0..backing.extent_count()).filter_map(|index| backing.extent(index)) {
                    let length = extent.pages * PAGE_SIZE;
                    visit(DmaSegment {
                        device_address: extent.device_address,
                        offset,
                        length,
                    });
                    offset += length;
                }
            }
        }
    }
}

#[cfg(not(test))]
impl<'mapping> DmaSlice<'mapping> {
    /// 投影 device backing 的只读前缀；backing 必须已以本设备 BTI 固定，并由 slot 保活到 used ring
    /// 归还 descriptor。
    ///
    /// # Errors
    ///
    /// 空范围或超过 backing 容量返回 `InvalidRange`。
    pub(in crate::drivers::virtio) fn backing(
        backing: &'mapping DeviceBacking,
        length: usize,
    ) -> Result<Self, DmaMappingError> {
        if length == 0 || length > backing.pages().saturating_mul(PAGE_SIZE) {
            return Err(DmaMappingError::InvalidRange);
        }
        Ok(Self {
            source: DmaSource::Backing(backing),
            range: 0..length,
            device_writable: false,
        })
    }
}

impl DmaSlice<'_> {
    pub(super) fn segment_count(&self) -> usize {
        let mut count = 0;
        self.source.for_each(|segment| {
            count += usize::from(segment_overlaps(&segment, &self.range));
        });
        count
    }

    pub(super) fn for_each_segment(&self, mut visit: impl FnMut(u64, usize, bool)) {
        self.source.for_each(|segment| {
            if !segment_overlaps(&segment, &self.range) {
                return;
            }
            let start = segment.offset.max(self.range.start);
            let end = (segment.offset + segment.length).min(self.range.end);
            visit(
                segment.device_address + (start - segment.offset) as u64,
                end - start,
                self.device_writable,
            );
        });
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

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{DmaChainRequirement, DmaSegment, DmaSlice, DmaSource, descriptor_requirement};

    fn segments() -> [DmaSegment; 3] {
        [
            DmaSegment { device_address: 0x1ffe, offset: 0, length: 2 },
            DmaSegment { device_address: 0x5000, offset: 2, length: 4096 },
            DmaSegment { device_address: 0x9000, offset: 4098, length: 4098 },
        ]
    }

    #[test]
    fn subrange_projects_device_addresses_of_overlapping_segments() {
        let segments = segments();
        let slice = DmaSlice {
            source: DmaSource::Mapped(&segments),
            range: 1..4099,
            device_writable: true,
        };
        assert_eq!(slice.segment_count(), 3);
        let mut observed = Vec::new();
        slice.for_each_segment(|address, length, writable| observed.push((address, length, writable)));
        assert_eq!(observed, [(0x1fff, 1, true), (0x5000, 4096, true), (0x9000, 1, true)]);
    }

    #[test]
    fn descriptor_capacity_is_checked_from_cached_segments() {
        let segments = segments();
        let first = DmaSlice {
            source: DmaSource::Mapped(&segments),
            range: 0..2,
            device_writable: false,
        };
        let second = DmaSlice {
            source: DmaSource::Mapped(&segments),
            range: 2..8196,
            device_writable: true,
        };
        assert_eq!(
            descriptor_requirement(&[first.clone(), second.clone()], 3),
            DmaChainRequirement::Required(3)
        );
        assert_eq!(
            descriptor_requirement(&[first, second], 2),
            DmaChainRequirement::ExceedsCapacity
        );
        assert_eq!(descriptor_requirement(&[], 8), DmaChainRequirement::Empty);
    }
}
