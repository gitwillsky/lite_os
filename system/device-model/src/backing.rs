//! 设备与用户共享的缓冲（GEM 对象、音频环、io_uring 区域）：一个 VMO 同时供用户映射、
//! 服务 CPU 访问与设备 DMA。
//!
//! 1. 用户映射直接映射该 VMO（`DeviceMappingSource`）。
//! 2. 服务需要 CPU 访问时，VMO 惰性映射到服务地址空间一次，之后 `read`/`write`/`atomic_u32` 直接访问。
//! 3. 设备 DMA 前由驱动以自己的 BTI [`DeviceBacking::pin`] 一次，取得按设备地址连续性合并的区段；
//!    固定记录与缓冲同寿命，缓冲释放时随之撤销设备地址。

use alloc::vec::Vec;
use core::sync::atomic::AtomicU32;

use native_runtime::native_abi::{self, PAGE_SIZE, Status, bti};
use native_runtime::sync::Once;
use native_runtime::{Bti, Pmt, Vmo};

/// 设备地址连续的一段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceExtent {
    /// 设备看到的起始地址。
    pub device_address: u64,
    /// 页数。
    pub pages: usize,
}

struct Pinned {
    extents: Vec<DeviceExtent>,
    _pmt: Pmt,
}

/// 设备与用户共享的缓冲。
pub struct DeviceBacking {
    vmo: Vmo,
    pages: usize,
    // OWNER: 服务地址空间中的映射；首次 CPU 访问时建立，与缓冲同寿命。
    mapping: Once<usize>,
    // OWNER: 唯一的 DMA 固定记录；缺失时设备地址不存在，驱动不能发布描述符。
    pinned: Once<Result<Pinned, Status>>,
}

impl core::fmt::Debug for DeviceBacking {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DeviceBacking")
            .field("pages", &self.pages)
            .finish_non_exhaustive()
    }
}

impl DeviceBacking {
    /// 分配 `pages` 页零内容缓冲（物理上可不连续）。
    pub fn try_allocate(pages: usize) -> Option<Self> {
        if pages == 0 {
            return None;
        }
        let vmo = Vmo::create(pages.checked_mul(PAGE_SIZE)? as u64, 0).ok()?;
        Some(Self {
            vmo,
            pages,
            mapping: Once::new(),
            pinned: Once::new(),
        })
    }

    /// 分配物理连续的缓冲（设备要求单一区段时）。
    pub fn try_allocate_contiguous(pages: usize, bti: &Bti) -> Option<Self> {
        if pages == 0 {
            return None;
        }
        let vmo = Vmo::create_contiguous(bti, pages.checked_mul(PAGE_SIZE)? as u64).ok()?;
        Some(Self {
            vmo,
            pages,
            mapping: Once::new(),
            pinned: Once::new(),
        })
    }

    /// 接管另一进程交来的缓冲 VMO（驱动宿主收到 GEM/DMA 缓冲时）；大小按页向下取整。
    ///
    /// # Errors
    ///
    /// 读取 VMO 大小失败。
    pub fn from_vmo(vmo: Vmo) -> Result<Self, Status> {
        let pages = (vmo.size()? as usize) / PAGE_SIZE;
        Ok(Self {
            vmo,
            pages,
            mapping: Once::new(),
            pinned: Once::new(),
        })
    }

    /// 页数。
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// 后备 VMO（用户映射与跨进程传递）。
    pub fn vmo(&self) -> &Vmo {
        &self.vmo
    }

    /// 以 `bti` 固定缓冲并取得设备地址区段；只能固定一次，之后返回同一结果。
    ///
    /// # Errors
    ///
    /// 固定失败（IOVA 耗尽或内存不足）。
    pub fn pin(&self, bti: &Bti) -> Result<&[DeviceExtent], Status> {
        let pinned = self.pinned.call_once(|| {
            let (addresses, pmt) = bti
                .pin(
                    bti::READ | bti::WRITE,
                    &self.vmo,
                    0,
                    (self.pages * PAGE_SIZE) as u64,
                )?;
            let mut extents: Vec<DeviceExtent> = Vec::new();
            for address in addresses {
                match extents.last_mut() {
                    Some(last) if last.device_address + (last.pages * PAGE_SIZE) as u64 == address => {
                        last.pages += 1;
                    }
                    _ => extents.push(DeviceExtent {
                        device_address: address,
                        pages: 1,
                    }),
                }
            }
            Ok(Pinned { extents, _pmt: pmt })
        });
        pinned.as_ref().map(|pinned| pinned.extents.as_slice()).map_err(|error| *error)
    }

    /// 已固定的区段数。
    ///
    /// # Panics
    ///
    /// 尚未成功固定。
    pub fn extent_count(&self) -> usize {
        self.pinned_extents().len()
    }

    /// 第 `index` 个已固定区段。
    pub fn extent(&self, index: usize) -> Option<DeviceExtent> {
        self.pinned_extents().get(index).copied()
    }

    fn pinned_extents(&self) -> &[DeviceExtent] {
        match self.pinned.get() {
            Some(Ok(pinned)) => &pinned.extents,
            _ => panic!("device backing used for DMA before a successful pin"),
        }
    }

    fn mapped(&self) -> Result<usize, ()> {
        if let Some(address) = self.mapping.get() {
            return Ok(*address);
        }
        let address = native_runtime::heap::vmar()
            .map(
                native_abi::vm::READ | native_abi::vm::WRITE,
                0,
                &self.vmo,
                0,
                self.pages * PAGE_SIZE,
            )
            .map_err(|_| ())?;
        let published = *self.mapping.call_once(|| address);
        if published != address {
            // 并发首访者已发布映射；撤销本线程多余的一份。
            // SAFETY: 这份映射刚建立，从未交给任何访问者。
            let _ = unsafe { native_runtime::heap::vmar().unmap(address, self.pages * PAGE_SIZE) };
        }
        Ok(published)
    }

    /// 从缓冲复制到 `output`。
    ///
    /// # Errors
    ///
    /// 越界。
    pub fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), Status> {
        self.vmo.read(output, offset as u64)
    }

    /// 把 `input` 写入缓冲。
    ///
    /// # Errors
    ///
    /// 越界。
    pub fn write(&self, offset: usize, input: &[u8]) -> Result<(), Status> {
        self.vmo.write(input, offset as u64)
    }

    /// 缓冲内 4 字节对齐位置的原子字（与用户映射共享同一物理页）。
    pub fn atomic_u32(&self, offset: usize) -> Option<&AtomicU32> {
        if !offset.is_multiple_of(4) || offset.checked_add(4)? > self.pages * PAGE_SIZE {
            return None;
        }
        let base = self.mapped().ok()?;
        // SAFETY: 映射与缓冲同寿命、可读写，偏移已校验对齐与边界。
        Some(unsafe { &*((base + offset) as *const AtomicU32) })
    }

    /// 服务地址空间中整个缓冲的 CPU 映射基址（驱动直接填写内容）；映射与缓冲同寿命，长度为
    /// `pages() * PAGE_SIZE`。经该地址访问时，调用者负责排除别名并遵守设备与用户映射的并发协议。
    pub fn cpu_address(&self) -> Option<usize> {
        self.mapped().ok()
    }
}

impl Drop for DeviceBacking {
    fn drop(&mut self) {
        if let Some(address) = self.mapping.get() {
            // SAFETY: 缓冲正在销毁，不再有通过它的访问。
            let _ = unsafe { native_runtime::heap::vmar().unmap(*address, self.pages * PAGE_SIZE) };
        }
    }
}
