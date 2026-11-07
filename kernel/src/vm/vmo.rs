//! VMO：以页为单位的内存对象。
//!
//! # 种类
//!
//! - 匿名分页：首次访问时按需提交零页；可创建 COW 子对象。
//! - pager 支撑：缺页时向 pager 发送请求，等待 `supply_pages` 补页；记录脏页供回写。
//! - 连续：创建时一次分配物理连续页并常驻，供 DMA 使用。
//! - 物理：引用一段固定物理区间（MMIO），不拥有页框。
//!
//! # COW
//!
//! 1. snapshot 子对象：创建时把父对象的已提交页移入一个新的隐藏节点，父对象与子对象都成为它的
//!    子节点；隐藏节点的页从此不可变，任一方写入时复制。父对象现有映射同时被写保护。
//!    这样父子双方都看不到对方在创建之后的写入（`fork` 语义）。
//! 2. at-least-on-write 子对象：直接以父对象为父；未写入的页读到父对象的当前内容，写入时复制。
//!    父对象撤销某页时，子对象中映射该页的翻译一并撤销。
//! 3. slice 子对象：父对象一段区间的别名，读写都作用于父对象。
//! 4. 存活对象取得树锁时，把仅剩自己的隐藏父节点合并进来；页框只转移所有权，不改变翻译。
//!    Drop 不取树锁，因此树锁内的临时引用析构不会递归等待同一把锁。
//!
//! # 树锁
//!
//! 一棵 COW 树（根与全部子孙、隐藏节点）共享一把 [`Tree`] 锁；任何 VMO 状态访问都必须持有它，
//! 因此父子之间不需要锁序。持树锁时可以再取地址空间锁（见 `vm` 模块文档）。

mod creation;
mod fault_around;
pub(crate) use fault_around::FAULT_AROUND_PAGES;
mod page_list;
use page_list::PageList;
mod mapping;
mod pager;
use pager::PageRequestCompletion;
mod zero_page;
pub(crate) use zero_page::initialize as initialize_zero_page;

mod pin;
mod reclaim;
mod release;
pub(crate) use pin::PinnedRange;
mod attribution;
mod residency;
use super::VmError;
use super::address_space::{Mapping, ZapMode};
use crate::execution::sync::{
    TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation, WaitQueue, WaitResult,
};
use crate::memory::{
    FrameAllocationClass, FrameTracker, PAGE_SIZE, PhysicalPageNumber, frame_allocator,
};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
pub(super) use reclaim::AccessSample;
#[cfg(not(test))]
pub(crate) use reclaim::background_reclaim;
pub(crate) use reclaim::{evicted_pages, relieve_memory_pressure};
pub(crate) use residency::physical_page_statistics;
use residency::{Cleanliness, Page, PageContent};
use spin::Mutex;

/// 物理 VMO 的缓存属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CachePolicy {
    /// 普通可缓存内存。
    Cached,
    /// 设备内存（MMIO），映射为不可缓存、不可合并、不可重排。
    Device,
}

/// 创建选项。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VmoOptions {
    /// 允许 `set_size`。
    pub(crate) resizable: bool,
}

/// pager 请求：`[offset, offset + length)` 字节需要补页。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PagerRequest {
    /// pager 创建 VMO 时给出的 key。
    pub(crate) key: u64,
    /// 页对齐字节偏移。
    pub(crate) offset: u64,
    /// 页对齐字节长度。
    pub(crate) length: u64,
}

/// pager 的请求出口；由 IPC 层实现为向 port 投递 packet。
pub(crate) trait PagerSink: Send + Sync {
    /// 投递一个补页请求。
    ///
    /// # Errors
    /// 出口已关闭或分配失败时返回 `Err(())`；等待者随之以 `Io` 失败。
    fn request(&self, request: PagerRequest) -> Result<(), ()>;
}

/// pager 支撑 VMO 的请求出口与 key。
#[derive(Clone)]
pub(crate) struct PagerBinding {
    pub(crate) sink: Arc<dyn PagerSink>,
    pub(crate) key: u64,
}

struct Tree {
    // OWNER: 整棵 COW 树的唯一状态锁，见模块文档。
    lock: TaskMutex<()>,
}

enum Kind {
    /// 快照的不可变祖先；不能由句柄直接访问，最后一个子对象使用树时将其折叠。
    Hidden,
    Paged {
        resizable: bool,
        pager: Option<PagerBinding>,
        content: PageContent,
    },
    Contiguous(FrameTracker),
    Physical {
        base: PhysicalPageNumber,
        cache: CachePolicy,
    },
    Slice,
}

enum Relation {
    /// 写入时复制的父对象。limit 是本对象中可继承的页数；合并祖先或调整大小不能扩大此窗口，
    /// 否则缩短的中间快照会在折叠后暴露祖父对象原本不可见的页。
    Cow {
        parent: Arc<Vmo>,
        offset: usize,
        limit: usize,
    },
    /// 共享父对象页的别名。
    Slice { parent: Arc<Vmo>, offset: usize },
}

struct State {
    pages: usize,
    committed: PageList,
    relation: Option<Relation>,
    // OWNER: 直接 COW 与 slice 子对象；用于翻译撤销与判定隐藏节点是否仅剩一个子对象（隐藏节点没有
    // slice 子对象）。Weak 不延长寿命。缺失 slice 时撤销本对象的页（decommit、补页替换、淘汰）留下 slice
    // 映射指向已释放页框的翻译。
    children: Vec<Weak<Vmo>>,
    // OWNER: 映射本对象的地址空间映射；Weak 在映射销毁后失效并被惰性清理。
    mappings: Vec<Weak<Mapping>>,
    // OWNER: 未完成补页的唯一登记；等待者保活本次结果，不从页是否仍驻留推断成功/失败。
    requested: PageList<Arc<PageRequestCompletion>>,
    // FLAG: pager 已 detach（Zircon `pager_detach_vmo`）：之后的缺页不再请求而以 I/O 错误失败。
    // 缺失时内容 owner 退休后仍存活的 VMO（导出副本、COW 子对象）会把请求投给已不处理它的服务，等待者永久睡眠。
    detached: bool,
    // FLAG: 本对象有读缺页被解析到共享零页（只读翻译指向全局零页）。之后首次提交真实页必须撤销该下标的翻译，
    // 否则读者继续读到零页而看不到后来的写入。只在根匿名对象上置位（见 `shared_zero_page`）。
    zero_mapped: bool,
}

/// 内存对象。
pub(crate) struct Vmo {
    id: u64,
    kind: Kind,
    tree: Arc<Tree>,
    // OWNER: 只在持有 `tree.lock` 时访问；spin 锁仅为满足 Sync，不承担互斥语义。
    state: Mutex<State>,
    // OWNER: pager 补页或失败时唤醒缺页等待者。
    supplied: WaitQueue,
}

/// 本次解析是否等待了 pager；不把对象的已有驻留页归类为新补页。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultSource {
    /// 已驻留、零页提交或写时复制，不需要 pager 响应。
    Memory,
    /// 本次解析等待了 pager 请求完成。
    Pager,
}

/// 一次页解析的结果。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedPage {
    /// 本次解析的内容来源；只描述此次访问，不保存第二份驻留状态。
    pub(crate) source: FaultSource,
    pub(crate) ppn: PhysicalPageNumber,
    /// 解析出的页可以写入；为假时页属于祖先，只能只读映射。
    pub(crate) writable: bool,
    pub(crate) cache: CachePolicy,
}

impl Vmo {
    /// 全局唯一编号。
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    fn lock_tree(&self) -> Result<TaskMutexGuard<'_, ()>, VmError> {
        let guard = self.tree.lock.lock().map_err(|_| VmError::NoMemory)?;
        self.collapse_hidden_parents();
        Ok(guard)
    }

    /// 解析一页供缺页或复制使用。调用者持有树锁。
    ///
    /// # Errors
    /// 越界返回 `OutOfRange`；需要 pager 补页返回 `Err(NeedPage)`，由调用者释放锁后等待。
    pub(super) fn resolve_locked(
        self: &Arc<Self>,
        index: usize,
        write: bool,
    ) -> Result<ResolvedPage, Resolve> {
        match &self.kind {
            Kind::Physical { base, cache } => {
                if index >= self.state.lock().pages {
                    return Err(Resolve::Error(VmError::OutOfRange));
                }
                return Ok(ResolvedPage {
                    source: FaultSource::Memory,
                    ppn: PhysicalPageNumber::from(base.as_usize() + index),
                    writable: true,
                    cache: *cache,
                });
            }
            Kind::Contiguous(frames) => {
                if index >= self.state.lock().pages {
                    return Err(Resolve::Error(VmError::OutOfRange));
                }
                return Ok(ResolvedPage {
                    source: FaultSource::Memory,
                    ppn: PhysicalPageNumber::from(frames.ppn.as_usize() + index),
                    writable: true,
                    cache: CachePolicy::Cached,
                });
            }
            Kind::Slice => {
                let (parent, offset) = {
                    let state = self.state.lock();
                    if index >= state.pages {
                        return Err(Resolve::Error(VmError::OutOfRange));
                    }
                    match &state.relation {
                        Some(Relation::Slice { parent, offset }) => (parent.clone(), *offset),
                        _ => unreachable!("slice VMO without slice parent"),
                    }
                };
                return parent.resolve_locked(index + offset, write);
            }
            Kind::Paged { .. } | Kind::Hidden => {}
        }
        let mut state = self.state.lock();
        if index >= state.pages {
            return Err(Resolve::Error(VmError::OutOfRange));
        }
        if let Some(page) = state.committed.with_mut(&index, |page| {
            page.note_access();
            if write {
                page.set_cleanliness(Cleanliness::Dirty);
            }
            ResolvedPage {
                source: FaultSource::Memory,
                ppn: page.ppn(),
                // 干净 pager 页只读安装，硬件写须先经历置脏。
                writable: page.cleanliness() == Cleanliness::Dirty
                    || !matches!(self.kind, Kind::Paged { pager: Some(_), .. }),
                cache: CachePolicy::Cached,
            }
        }) {
            return Ok(page);
        }
        // 祖先链：找到页则只读共享，写入时复制到本对象。
        let mut ancestor = match &state.relation {
            Some(Relation::Cow {
                parent,
                offset,
                limit,
            }) if index < *limit => Some((parent.clone(), index + offset)),
            _ => None,
        };
        while let Some((vmo, ancestor_index)) = ancestor {
            let mut ancestor_state = vmo.state.lock();
            if ancestor_index >= ancestor_state.pages {
                break;
            }
            let inherited = ancestor_state.committed.with_mut(&ancestor_index, |page| {
                page.note_access();
                let frame = if write {
                    frame_allocator::alloc_copy(page.bytes())
                } else {
                    None
                };
                (page.ppn(), frame)
            });
            if let Some((ppn, frame)) = inherited {
                if !write {
                    return Ok(ResolvedPage {
                        source: FaultSource::Memory,
                        ppn,
                        writable: false,
                        cache: CachePolicy::Cached,
                    });
                }
                let frame = frame.ok_or(Resolve::Error(VmError::NoMemory))?;
                drop(ancestor_state);
                let page = commit_page(&mut state, index, frame, self.page_content())?;
                drop(state);
                // 本对象与子孙的映射可能仍只读指向祖先页框；不撤销时它们在复制后继续读到旧内容
                // （缺页路径只替换发生缺页的那一条翻译，VMO 写入与调试写入不经缺页）。
                self.zap(index..index + 1, ZapMode::Unmap)
                    .map_err(Resolve::Error)?;
                return Ok(page);
            }
            let next = match &ancestor_state.relation {
                Some(Relation::Cow {
                    parent,
                    offset,
                    limit,
                }) if ancestor_index < *limit => Some((parent.clone(), ancestor_index + offset)),
                _ => {
                    if let Kind::Paged { pager: Some(_), .. } = vmo.kind {
                        drop(ancestor_state);
                        return Err(Resolve::NeedPage {
                            vmo: vmo.clone(),
                            index: ancestor_index,
                        });
                    }
                    None
                }
            };
            drop(ancestor_state);
            ancestor = next;
        }
        if let Kind::Paged { pager: Some(_), .. } = self.kind {
            return Err(Resolve::NeedPage {
                vmo: self.clone(),
                index,
            });
        }
        if let Some(page) = self.shared_zero_locked(&mut state, write) {
            return Ok(page);
        }
        let frame = frame_allocator::alloc().ok_or(Resolve::Error(VmError::NoMemory))?;
        let page = commit_page(&mut state, index, frame, self.page_content())?;
        self.retire_zero_translation(state, index)?;
        Ok(page)
    }

    /// 解析一页，必要时等待 pager 补页；调用者不持有树锁。
    ///
    /// `with_page` 在持树锁时执行，可安全使用页框（例如安装 PTE 或复制内容）。
    ///
    /// # Errors
    /// 越界、分配失败、pager 失败或等待被打断。
    pub(crate) fn with_resolved<R>(
        self: &Arc<Self>,
        index: usize,
        write: bool,
        with_page: impl FnMut(ResolvedPage) -> Result<R, VmError>,
    ) -> Result<R, VmError> {
        self.with_resolved_around(index, write, None, with_page, |_, _| ())
    }

    /// 读取字节；未提交页按零（或从祖先/pager）解析。
    ///
    /// # Errors
    /// 越界、分配失败或 pager 失败。
    pub(crate) fn read(self: &Arc<Self>, offset: u64, output: &mut [u8]) -> Result<(), VmError> {
        self.for_each_chunk(offset, output.len(), false, |frame, page_offset, range| {
            frame.read(page_offset, &mut output[range]);
        })
    }

    /// 写入字节（必要时写时复制）。
    ///
    /// # Errors
    /// 同 [`Self::read`]。
    pub(crate) fn write(self: &Arc<Self>, offset: u64, input: &[u8]) -> Result<(), VmError> {
        self.for_each_chunk(offset, input.len(), true, |frame, page_offset, range| {
            frame.write(page_offset, &input[range]);
        })
    }

    fn for_each_chunk(
        self: &Arc<Self>,
        offset: u64,
        length: usize,
        write: bool,
        mut operation: impl FnMut(PageAccess, usize, core::ops::Range<usize>),
    ) -> Result<(), VmError> {
        let offset = usize::try_from(offset).map_err(|_| VmError::OutOfRange)?;
        let end = offset.checked_add(length).ok_or(VmError::OutOfRange)?;
        let mut cursor = offset;
        while cursor < end {
            let page_offset = cursor % PAGE_SIZE;
            let chunk = (PAGE_SIZE - page_offset).min(end - cursor);
            let start = cursor - offset;
            self.with_resolved(cursor / PAGE_SIZE, write, |page| {
                operation(PageAccess(page.ppn), page_offset, start..start + chunk);
                Ok(())
            })?;
            cursor += chunk;
        }
        Ok(())
    }

    /// 提交、撤销或清零一段页。
    ///
    /// # Errors
    /// 参数非法、越界、页被固定或类型不支持。
    pub(crate) fn op_range(
        self: &Arc<Self>,
        op: RangeOp,
        offset: u64,
        length: u64,
    ) -> Result<(), VmError> {
        match op {
            RangeOp::Commit => {
                let (first, count) = page_range(offset, length)?;
                for index in first..first + count {
                    self.with_resolved(index, true, |_| Ok(()))?;
                }
                Ok(())
            }
            RangeOp::Decommit => {
                let (first, count) = page_range(offset, length)?;
                if !matches!(self.kind, Kind::Paged { .. }) {
                    return Err(VmError::NotSupported);
                }
                let _tree = self.lock_tree()?;
                self.release_pages(first..first + count)
            }
            RangeOp::Zero => {
                // 字节精确：首尾部分页只清零区间内的字节。
                let zero = [0u8; PAGE_SIZE];
                let end = offset.checked_add(length).ok_or(VmError::OutOfRange)?;
                let mut cursor = offset;
                while cursor < end {
                    let within = (cursor % PAGE_SIZE as u64) as usize;
                    let chunk = ((PAGE_SIZE - within) as u64).min(end - cursor) as usize;
                    self.write(cursor, &zero[..chunk])?;
                    cursor += chunk as u64;
                }
                Ok(())
            }
        }
    }

    /// 调用者持有树锁：撤销映射与子对象翻译后释放区间内本对象拥有的页。
    fn release_pages(&self, range: core::ops::Range<usize>) -> Result<(), VmError> {
        if self
            .state
            .lock()
            .committed
            .iter_from(&range.start)
            .take_while(|(index, _)| *index < range.end)
            .any(|(_, page)| page.pins() != 0)
        {
            return Err(VmError::BadState);
        }
        self.zap(range.clone(), ZapMode::Unmap)?;
        let mut state = self.state.lock();
        // fence 已在 zap 中完成，页框可以直接释放（释放只取 frame allocator 的 IRQ 锁，与淘汰路径一致）。
        while let Some((index, _)) = state.committed.ceiling(&range.start) {
            if index >= range.end {
                break;
            }
            drop(state.committed.remove(&index));
        }
        drop(state);
        Ok(())
    }

    /// 改变大小；缩小时撤销并释放越界页。
    ///
    /// # Errors
    /// 不可调整大小或有越界的固定页。
    pub(crate) fn set_size(self: &Arc<Self>, size: u64) -> Result<(), VmError> {
        let Kind::Paged {
            resizable: true, ..
        } = self.kind
        else {
            return Err(VmError::NotSupported);
        };
        let pages = pages_for(size)?;
        let _tree = self.lock_tree()?;
        let old = self.state.lock().pages;
        if pages < old {
            self.release_pages(pages..old)?;
            if let Some(Relation::Cow { limit, .. }) = &mut self.state.lock().relation {
                *limit = (*limit).min(pages);
            }
        }
        self.state.lock().pages = pages;
        if pages < old {
            self.invalidate_read_requests_locked(pages..old);
        }
        Ok(())
    }
}

/// 子对象语义，见模块文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildKind {
    Snapshot,
    AtLeastOnWrite,
    Slice,
}

/// `op_range` 的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RangeOp {
    Commit,
    Decommit,
    Zero,
}

/// 页解析失败的原因。
pub(super) enum Resolve {
    Error(VmError),
    /// 需要 pager 为 `vmo` 的第 `index` 页补页。
    NeedPage {
        vmo: Arc<Vmo>,
        index: usize,
    },
}

/// 持树锁期间对一个已解析物理页的字节访问。
pub(crate) struct PageAccess(PhysicalPageNumber);

impl PageAccess {
    fn read(&self, offset: usize, output: &mut [u8]) {
        // SAFETY: 调用者持有树锁，页框在此期间不会被释放或替换；direct map 覆盖全部 RAM。
        let page = unsafe { core::slice::from_raw_parts(self.0.as_page_ptr(), PAGE_SIZE) };
        output.copy_from_slice(&page[offset..offset + output.len()]);
    }

    fn write(&self, offset: usize, input: &[u8]) {
        // SAFETY: 同 `read`；写解析保证页属于本对象（不是共享祖先页）。
        let page = unsafe { core::slice::from_raw_parts_mut(self.0.as_page_mut_ptr(), PAGE_SIZE) };
        page[offset..offset + input.len()].copy_from_slice(input);
    }
}

fn commit_page(
    state: &mut State,
    index: usize,
    frame: FrameTracker,
    content: PageContent,
) -> Result<ResolvedPage, Resolve> {
    let ppn = frame.ppn;
    state
        .committed
        .try_insert(index, Page::new(frame, content))
        .map_err(|_| Resolve::Error(VmError::NoMemory))?;
    Ok(ResolvedPage {
        source: FaultSource::Memory,
        ppn,
        writable: true,
        cache: CachePolicy::Cached,
    })
}

fn pages_for(size: u64) -> Result<usize, VmError> {
    let size = usize::try_from(size).map_err(|_| VmError::InvalidArgs)?;
    size.checked_add(PAGE_SIZE - 1)
        .map(|size| size / PAGE_SIZE)
        .ok_or(VmError::InvalidArgs)
}

fn page_range(offset: u64, length: u64) -> Result<(usize, usize), VmError> {
    if !offset.is_multiple_of(PAGE_SIZE as u64) || !length.is_multiple_of(PAGE_SIZE as u64) {
        return Err(VmError::InvalidArgs);
    }
    Ok((
        usize::try_from(offset / PAGE_SIZE as u64).map_err(|_| VmError::OutOfRange)?,
        usize::try_from(length / PAGE_SIZE as u64).map_err(|_| VmError::OutOfRange)?,
    ))
}

#[cfg(test)]
mod tests;
