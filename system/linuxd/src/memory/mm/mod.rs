//! 一个 Linux 地址空间。
//!
//! # 布局（私有区 `[base, end)`）
//!
//! ```text
//! end - PAGE            信号返回跳板（全部地址空间共享的只读可执行页）
//! stack_top             主线程栈顶（下方 `stack_size` 字节为栈区，页按需提交）
//! mmap_top              mmap 自上而下分配的上界（栈下方保留间隙）
//! ...                   动态库、匿名映射
//! heap                  brk 区（紧接主程序最高段之后）
//! 主程序 / 解释器          固定基址装载
//! ```
//!
//! # 并发
//!
//! `MemorySet` 由所属 `AddressSpace` 的锁保护：所有修改与需要 VMA 稳定的用户访问都在锁内完成，
//! 因此 VMA 索引与 VMAR 中的实际映射始终一致。

mod access;
mod area;
mod fault;
mod fork;
mod mapping;
mod observation;
mod request;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use crate::sync::Once;
use fallible_tree::FallibleMap;
use native_runtime::{Process, Vmar};

pub(crate) use area::{AnonymousRole, Area, AreaFlags, Backing, MappingBacking, MappingSnapshot};
pub(crate) use fault::{PageFaultAccess, PageFaultOutcome, SegmentationCause};
pub(crate) use request::{
    DeviceMappingSource, FileMappingError, FileMappingSource, MappingPlacement,
    MappingResourceLimits, MemoryAdvice, RemapError, RemapPlacement, RemapRequest,
    UserMappingRequest, UserMappingSource,
};

use super::{
    MapPermission, MappedFileOwner, MappingStatistics, MemoryError, MemoryObject, PAGE_SIZE,
};

/// `/proc/<pid>/status` 与 `statm` 使用的页统计。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UserPageStatistics {
    pub(crate) virtual_pages: usize,
    pub(crate) resident_pages: usize,
    pub(crate) shared_pages: usize,
    pub(crate) text_pages: usize,
    pub(crate) data_pages: usize,
    pub(crate) stack_pages: usize,
}

/// `/proc/meminfo` 的系统级映射分类（页数）；后备对象跨进程去重，共享对象只计一次。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MappedMemory {
    /// 私有匿名与私有文件映射写时复制出的页（AnonPages）。
    pub(crate) anonymous_pages: usize,
    /// 共享映射的驻留页（Mapped）；私有文件映射未复制的页属于文件对象，不在此计入。
    pub(crate) mapped_pages: usize,
    /// 共享匿名与 System V 共享内存段的驻留页（Shmem 的映射部分）。
    pub(crate) shmem_pages: usize,
}

/// brk 状态：`start` 固定，`brk` 是程序看到的结尾，`mapped` 是已映射的页对齐结尾。
#[derive(Debug, Clone)]
struct Heap {
    start: usize,
    brk: usize,
    mapped: usize,
    object: Arc<MemoryObject>,
}

/// 一个 Linux 地址空间。
pub(crate) struct MemorySet {
    process: Arc<Process>,
    root: Vmar,
    base: usize,
    pub(super) end: usize,
    // OWNER: 唯一的 VMA 索引（起点 → VMA）；与 VMAR 中的实际映射在同一锁内一致修改。
    areas: FallibleMap<usize, Area>,
    pub(super) executable_owner: Option<Arc<dyn MappedFileOwner>>,
    // OWNER: 动态解释器的打开生命周期（Linux 解释器 VMA 的 `vm_file`）；与主程序租约同样随 mm 存活、
    // fork 继承。缺失时解释器映射的后续缺页在有服务端句柄的 filesystem 上找不到打开而失败。
    pub(super) interpreter_owner: Option<Arc<dyn MappedFileOwner>>,
    heap: Option<Heap>,
    pub(super) stack: Range<usize>,
    pub(super) mmap_top: usize,
    pub(super) arguments: Range<usize>,
}

impl core::fmt::Debug for MemorySet {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("MemorySet")
            .field("range", &(self.base..self.end))
            .finish_non_exhaustive()
    }
}

/// 全部地址空间共享的跳板对象。
fn trampoline() -> Result<Arc<MemoryObject>, MemoryError> {
    // OWNER: 只读可执行的跳板页；首次创建地址空间时写入，之后不变。
    static TRAMPOLINE: Once<Arc<MemoryObject>> = Once::new();
    if let Some(object) = TRAMPOLINE.get() {
        return Ok(object.clone());
    }
    let object = MemoryObject::anonymous(PAGE_SIZE, false)?;
    let mut code = [0u8; 8];
    for (chunk, word) in code
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(crate::arch::user::SIGRETURN_TRAMPOLINE)
    {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    object
        .vmo()
        .write(&code, 0)
        .map_err(MemoryError::from_status)?;
    Ok(TRAMPOLINE.call_once(|| object).clone())
}

impl MemorySet {
    /// 创建空地址空间：共享进程的一个新成员。
    ///
    /// # Errors
    ///
    /// 成员进程创建失败。
    pub(crate) fn try_new(name: &[u8]) -> Result<Self, MemoryError> {
        let (process, root) = crate::handles::shared_process()
            .create_shared(name)
            .map_err(MemoryError::from_status)?;
        let region = root.region().map_err(MemoryError::from_status)?;
        let base = region.base as usize;
        let end = base + region.size as usize;
        super::publish_user_range(base.max(PAGE_SIZE)..end);
        let process = Arc::try_new(process).map_err(|_| MemoryError::OutOfMemory)?;
        let mut memory = Self {
            process,
            root,
            base,
            end,
            areas: FallibleMap::new(),
            executable_owner: None,
            interpreter_owner: None,
            heap: None,
            stack: end..end,
            mmap_top: end,
            arguments: 0..0,
        };
        memory.insert(Area {
            start: end - PAGE_SIZE,
            end,
            permission: MapPermission::U | MapPermission::R | MapPermission::X,
            backing: Backing::Trampoline {
                object: trampoline()?,
            },
            flags: AreaFlags::DONT_DUMP,
            name: None,
        })?;
        Ok(memory)
    }

    /// 承载本地址空间的成员进程（Linux 线程的 native 线程在其中创建）。
    pub(crate) fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// 信号返回跳板地址。
    pub(crate) fn trampoline(&self) -> usize {
        self.end - PAGE_SIZE
    }

    /// 主程序映射（`/proc/<pid>/exe`）。
    pub(crate) fn executable_owner(&self) -> Option<Arc<dyn MappedFileOwner>> {
        self.executable_owner.clone()
    }

    /// argv 字符串区间（`/proc/<pid>/cmdline`）。
    pub(crate) fn argument_range(&self) -> Range<usize> {
        self.arguments.clone()
    }

    /// 覆盖 `address` 的 VMA。
    pub(crate) fn area_at(&self, address: usize) -> Option<&Area> {
        self.areas
            .floor(&address)
            .map(|(_, area)| area)
            .filter(|area| address < area.end)
    }

    /// 第一个结束于 `address` 之后的 VMA。
    fn area_at_or_after(&self, address: usize) -> Option<&Area> {
        if let Some(area) = self.area_at(address) {
            return Some(area);
        }
        self.areas.successor(&address).map(|(_, area)| area)
    }

    /// `[start, end)` 是否完全未映射。
    fn range_is_free(&self, start: usize, end: usize) -> bool {
        start >= self.base.max(PAGE_SIZE)
            && end <= self.end
            && self
                .area_at_or_after(start)
                .is_none_or(|area| area.start >= end)
    }

    /// 自上而下在 `[floor, top)` 中寻找 `length` 字节空隙。
    fn find_free(&self, length: usize, top: usize) -> Option<usize> {
        let floor = self.heap.as_ref().map_or_else(
            || self.base.max(PAGE_SIZE),
            |heap| heap.mapped.max(heap.start),
        );
        let mut ceiling = top;
        let mut areas: Vec<(usize, usize)> = self
            .areas
            .iter()
            .map(|(_, area)| (area.start, area.end))
            .filter(|&(start, _)| start < top)
            .collect();
        areas.reverse();
        for (start, end) in areas {
            if end <= ceiling && ceiling - end >= length && ceiling - length >= floor {
                return Some(ceiling - length);
            }
            ceiling = ceiling.min(start);
        }
        (ceiling >= floor && ceiling - floor >= length).then(|| ceiling - length)
    }

    /// 为新映射选择地址。
    fn place(&self, hint: usize, length: usize) -> Result<usize, MemoryError> {
        if hint != 0 {
            let hint = hint & !(PAGE_SIZE - 1);
            if hint
                .checked_add(length)
                .is_some_and(|end| self.range_is_free(hint, end))
            {
                return Ok(hint);
            }
        }
        self.find_free(length, self.mmap_top)
            .or_else(|| self.find_free(length, self.stack.start))
            .ok_or(MemoryError::OutOfMemory)
    }

    /// 安装并登记一个 VMA；调用者保证区间未被占用。
    pub(super) fn insert(&mut self, area: Area) -> Result<(), MemoryError> {
        let entry = FallibleMap::try_prepare(area.start, area.clone())
            .map_err(|_| MemoryError::OutOfMemory)?;
        area.install(&self.root, self.base)?;
        self.areas.commit_vacant(entry);
        Ok(())
    }

    /// 使 `address` 成为 VMA 边界（必要时切分覆盖它的 VMA）。
    fn split_at(&mut self, address: usize) -> Result<(), MemoryError> {
        let Some(start) = self
            .area_at(address)
            .filter(|area| area.start != address)
            .map(|area| area.start)
        else {
            return Ok(());
        };
        let slot =
            FallibleMap::<usize, Area>::try_reserve_node().map_err(|_| MemoryError::OutOfMemory)?;
        let area = self.areas.get_mut(&start).expect("area found above");
        let right = area.split_off(address);
        self.areas.commit_vacant(slot.fill(address, right));
        Ok(())
    }

    /// `[start, end)` 内的全部 VMA 起点（调用前已在两端切分）。
    fn starts_within(&self, start: usize, end: usize) -> Vec<usize> {
        let mut starts = Vec::new();
        if let Some(area) = self.area_at(start) {
            starts.push(area.start);
        }
        for (&area_start, _) in self.areas.iter_after(&start) {
            if area_start >= end {
                break;
            }
            starts.push(area_start);
        }
        starts
    }

    /// 解除 `[start, end)` 内的全部映射。
    pub(super) fn remove_range(&mut self, start: usize, end: usize) -> Result<(), MemoryError> {
        self.split_at(start)?;
        self.split_at(end)?;
        let starts = self.starts_within(start, end);
        if starts.is_empty() {
            return Ok(());
        }
        // SAFETY: 区间内的用户映射即将从 VMA 索引移除，服务自身从不持有指向 Linux 私有区的引用。
        unsafe { self.root.unmap(start, end - start) }.map_err(MemoryError::from_status)?;
        for area_start in starts {
            let area = self.areas.remove(&area_start).expect("area listed above");
            if let Backing::Anonymous {
                role: AnonymousRole::Heap,
                ..
            } = area.backing
                && let Some(heap) = &mut self.heap
                && area.end == heap.mapped
            {
                heap.mapped = area.start.max(heap.start);
                heap.brk = heap.brk.min(heap.mapped);
            }
        }
        Ok(())
    }

    /// 全部 VMA 的虚拟字节数。
    pub(crate) fn virtual_bytes(&self) -> u64 {
        self.areas
            .iter()
            .filter(|(_, area)| !matches!(area.backing, Backing::Trampoline { .. }))
            .map(|(_, area)| area.len() as u64)
            .sum()
    }

    /// 计入 `RLIMIT_DATA` 的字节数。
    pub(crate) fn data_bytes(&self) -> u64 {
        self.areas
            .iter()
            .filter(|(_, area)| area.counts_as_data())
            .map(|(_, area)| area.len() as u64)
            .sum()
    }

    /// 是否存在起点不低于 `address` 的用户 VMA（`mremap`/`munmap` 的探测）。
    pub(crate) fn has_user_vma_at_or_after(&self, address: usize) -> bool {
        self.area_at_or_after(address)
            .is_some_and(|area| !matches!(area.backing, Backing::Trampoline { .. }))
    }

    /// `[start, end)` 被 VMA 连续覆盖。
    fn require_mapped(&self, start: usize, end: usize) -> Result<(), MemoryError> {
        let mut cursor = start;
        while cursor < end {
            let area = self.area_at(cursor).ok_or(MemoryError::OutOfMemory)?;
            cursor = area.end;
        }
        Ok(())
    }
}
