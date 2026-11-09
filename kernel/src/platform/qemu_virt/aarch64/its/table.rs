//! ITS BASER 的直接/间接表；所有可失败存储在硬件 publication 前准备。
use crate::platform::{MessageInterruptAllocator, MessageInterruptMemory};
use alloc::vec::Vec;
use mmio::MmioBus;
use native_abi::{ERR_NO_MEMORY, ERR_NOT_SUPPORTED, ERR_OUT_OF_RANGE, Status};

/// 控制器持有的 DMA 存储；分配与释放策略由注入的 DmaFrame 实现拥有。
pub(super) struct Frame {
    memory: MessageInterruptMemory,
    pages: usize,
}
impl Frame {
    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
        let address = crate::arch::mmu::physical_to_virtual((physical(self)) as usize);
        // SAFETY: Frame 独占 pages 个驻留连续页；调用方只写自己的表/未发布的 command 槽，不访问硬件修改的记录。
        unsafe { core::slice::from_raw_parts_mut(address as *mut u8, self.pages * 4096) }
    }
}
pub(super) fn allocate(
    allocator: MessageInterruptAllocator,
    bytes: usize,
) -> Result<Frame, Status> {
    let pages = bytes
        .div_ceil(4096)
        .max(1)
        .checked_next_power_of_two()
        .ok_or(ERR_OUT_OF_RANGE)?;
    let memory = allocator(pages).ok_or(ERR_NO_MEMORY)?;
    let frame = Frame { memory, pages };
    // BASER 与 pending table 要求按硬件页粒度对齐，不能用 CPU 虚拟连续代替。
    if !(physical(&frame) as usize).is_multiple_of(pages * 4096) {
        return Err(ERR_NOT_SUPPORTED);
    }
    Ok(frame)
}
pub(super) fn physical(frame: &Frame) -> u64 {
    (frame.memory.physical_page() as u64) << 12
}

pub(super) struct Table {
    register: usize,
    value: u64,
    root: Frame,
    pages: Vec<Option<Frame>>,
    entries_per_page: usize,
    page_bytes: usize,
    indirect: bool,
    limit: u64,
}
impl Table {
    /// ITS 已禁用。选择控制器接受的 page size，并只在布局可表示时分配 backing。
    pub(super) fn prepare(
        allocator: MessageInterruptAllocator,
        bus: MmioBus,
        register: usize,
        bits: u32,
        indirect: bool,
    ) -> Result<Self, Status> {
        let original = bus
            .read_u64(register)
            .expect("ITS BASER outside register frame");
        let entry_bytes = ((original >> 48) & 31) as usize + 1;
        if !entry_bytes.is_power_of_two() {
            return Err(ERR_NOT_SUPPORTED);
        }
        let mut result = None;
        for (encoding, page_bytes) in [(0u64, 4096usize), (1, 16384), (2, 65536)] {
            bus.write_u64(
                register,
                (original & (7 << 56 | 31 << 48)) | encoding << 8 | u64::from(indirect) << 62,
            )
            .expect("ITS BASER probe");
            let accepted = bus.read_u64(register).expect("ITS BASER readback");
            if accepted >> 8 & 3 != encoding {
                continue;
            }
            let use_indirect = indirect && accepted & (1 << 62) != 0;
            let entries_per_page = page_bytes / entry_bytes;
            let entries = 1u64.checked_shl(bits).ok_or(ERR_OUT_OF_RANGE)?;
            let bytes = if use_indirect {
                entries.div_ceil(entries_per_page as u64) * 8
            } else {
                entries * entry_bytes as u64
            };
            let count = bytes.div_ceil(page_bytes as u64);
            if count > 256 {
                continue;
            }
            result = Some((
                encoding,
                page_bytes,
                entries_per_page,
                use_indirect,
                bytes,
                entries,
            ));
            break;
        }
        let (encoding, page_bytes, entries_per_page, indirect, bytes, limit) =
            result.ok_or(ERR_NOT_SUPPORTED)?;
        let root = allocate(allocator, (bytes as usize).max(page_bytes))?;
        let number = root.pages * 4096 / page_bytes;
        let value = physical(&root)
            | (1 << 63)
            | u64::from(indirect) << 62
            | 7 << 59
            | 1 << 10
            | encoding << 8
            | (number as u64 - 1);
        let mut pages = Vec::new();
        if indirect {
            let count = limit.div_ceil(entries_per_page as u64) as usize;
            pages.try_reserve_exact(count).map_err(|_| ERR_NO_MEMORY)?;
            pages.resize_with(count, || None);
        }
        Ok(Self {
            register,
            value,
            root,
            pages,
            entries_per_page,
            page_bytes,
            indirect,
            limit,
        })
    }
    pub(super) fn publish(&self, bus: MmioBus) {
        bus.write_u64(self.register, self.value)
            .expect("ITS BASER publication");
        let actual = bus.read_u64(self.register).expect("ITS BASER readback");
        assert_eq!(
            actual & (1 << 63 | 1 << 62 | 3 << 10 | 7 << 59 | 3 << 8 | 255),
            self.value & (1 << 63 | 1 << 62 | 3 << 10 | 7 << 59 | 3 << 8 | 255),
            "ITS requires coherent writable tables"
        );
    }
    /// 为 device ID 准备间接二级页；写入前若失败，旧映射完全不变。
    pub(super) fn ensure(
        &mut self,
        allocator: MessageInterruptAllocator,
        id: u32,
    ) -> Result<(), Status> {
        if u64::from(id) >= self.limit {
            return Err(ERR_OUT_OF_RANGE);
        }
        if !self.indirect {
            return Ok(());
        }
        let index = id as usize / self.entries_per_page;
        if self.pages[index].is_some() {
            return Ok(());
        }
        let page = allocate(allocator, self.page_bytes)?;
        let value = physical(&page) | 1 << 63;
        self.pages[index] = Some(page);
        let offset = index * 8;
        self.root.bytes_mut()[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        crate::arch::dma::complete_table_writes();
        Ok(())
    }
}
