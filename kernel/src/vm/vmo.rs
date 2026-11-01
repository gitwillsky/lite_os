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
//!
//! # 树锁
//!
//! 一棵 COW 树（根与全部子孙、隐藏节点）共享一把 [`Tree`] 锁；任何 VMO 状态访问都必须持有它，
//! 因此父子之间不需要锁序。持树锁时可以再取地址空间锁（见 `vm` 模块文档）。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use spin::Mutex;

use super::VmError;
use super::address_space::{Mapping, ZapMode};
use crate::execution::sync::{
    TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation, WaitQueue, WaitResult,
};
use fallible_tree::FallibleMap;
use crate::memory::{
    FrameAllocationClass, FrameTracker, PAGE_SIZE, PhysicalPageNumber, frame_allocator,
};

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

/// 一段已固定的页；固定期间页不会被撤销、替换或释放（DMA）。
pub(crate) struct PinnedRange {
    vmo: Arc<Vmo>,
    first_page: usize,
    pages: Vec<PhysicalPageNumber>,
    // OWNER: 固定时预先准备的树锁 waiter；解除固定发生在 Drop 中，不能因 waiter 分配失败而遗漏，
    // 否则页永远保持固定、无法撤销。
    unpin_wait: Option<TaskMutexWaitPreparation>,
}

impl PinnedRange {
    /// 固定页的物理页号，按 VMO 页序。
    pub(crate) fn pages(&self) -> &[PhysicalPageNumber] {
        &self.pages
    }
}

impl Drop for PinnedRange {
    fn drop(&mut self) {
        let mut wait = self
            .unpin_wait
            .take()
            .expect("pinned range released twice");
        let _tree = self.vmo.tree.lock.lock_prepared(&mut wait);
        self.vmo.unpin(self.first_page, self.pages.len());
    }
}

struct Tree {
    // OWNER: 整棵 COW 树的唯一状态锁，见模块文档。
    lock: TaskMutex<()>,
}

enum Kind {
    Paged {
        resizable: bool,
        pager: Option<PagerBinding>,
    },
    Contiguous(FrameTracker),
    Physical {
        base: PhysicalPageNumber,
        cache: CachePolicy,
    },
    Slice,
}

struct Page {
    frame: FrameTracker,
    pins: u32,
    // FLAG: pager 页在补入后被写过；缺失时回写会遗漏修改，只对 pager 支撑的 VMO 有意义。
    dirty: bool,
}

enum Relation {
    /// 写入时复制的父对象；`snapshot` 为真表示父对象是不可变的隐藏节点。
    Cow { parent: Arc<Vmo>, offset: usize },
    /// 共享父对象页的别名。
    Slice { parent: Arc<Vmo>, offset: usize },
}

struct State {
    pages: usize,
    committed: FallibleMap<usize, Page>,
    relation: Option<Relation>,
    // OWNER: at-least-on-write 子对象可能映射本对象的页；撤销页时据此传播。Weak 不延长子对象寿命。
    children: Vec<Weak<Vmo>>,
    // OWNER: 映射本对象的地址空间映射；Weak 在映射销毁后失效并被惰性清理。
    mappings: Vec<Weak<Mapping>>,
    // OWNER: 已发出、尚未补入的 pager 请求页；避免同一页重复请求。pager 报告失败时撤销对应项，
    // 等待者据“请求已撤销而页仍缺失”判定失败；缺失该判定时等待者会无限等待永不到来的补页。
    requested: FallibleMap<usize, ()>,
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

/// 一次页解析的结果。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedPage {
    pub(crate) ppn: PhysicalPageNumber,
    /// 解析出的页可以写入；为假时页属于祖先，只能只读映射。
    pub(crate) writable: bool,
    pub(crate) cache: CachePolicy,
}

impl Vmo {
    fn new_in(tree: Arc<Tree>, kind: Kind, pages: usize, relation: Option<Relation>) -> Result<Arc<Self>, VmError> {
        DIAG_VMOS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Arc::try_new(Self {
            id: crate::id::next_koid(),
            kind,
            tree,
            state: Mutex::new(State {
                pages,
                committed: FallibleMap::new(),
                relation,
                children: Vec::new(),
                mappings: Vec::new(),
                requested: FallibleMap::new(),
            }),
            supplied: WaitQueue::new(),
        })
        .map_err(|_| VmError::NoMemory)
    }

    fn new_tree() -> Result<Arc<Tree>, VmError> {
        Arc::try_new(Tree {
            lock: TaskMutex::new(()),
        })
        .map_err(|_| VmError::NoMemory)
    }

    /// 创建 `size` 字节（向上取整到页）的匿名零内容 VMO。
    ///
    /// # Errors
    /// 大小溢出返回 `InvalidArgs`，分配失败返回 `NoMemory`。
    pub(crate) fn create(size: u64, options: VmoOptions) -> Result<Arc<Self>, VmError> {
        Self::new_in(
            Self::new_tree()?,
            Kind::Paged {
                resizable: options.resizable,
                pager: None,
            },
            pages_for(size)?,
            None,
        )
    }

    /// 创建 pager 支撑的 VMO。
    ///
    /// # Errors
    /// 同 [`Self::create`]。
    pub(crate) fn create_paged(
        size: u64,
        options: VmoOptions,
        pager: PagerBinding,
    ) -> Result<Arc<Self>, VmError> {
        Self::new_in(
            Self::new_tree()?,
            Kind::Paged {
                resizable: options.resizable,
                pager: Some(pager),
            },
            pages_for(size)?,
            None,
        )
    }

    /// 创建物理连续、常驻的 VMO。
    ///
    /// # Errors
    /// 连续页不足返回 `NoMemory`。
    pub(crate) fn create_contiguous(size: u64) -> Result<Arc<Self>, VmError> {
        let pages = pages_for(size)?;
        if pages == 0 {
            return Err(VmError::InvalidArgs);
        }
        let frames = crate::memory::alloc_contiguous(pages, FrameAllocationClass::Reclaimable)
            .ok_or(VmError::NoMemory)?;
        Self::new_in(Self::new_tree()?, Kind::Contiguous(frames), pages, None)
    }

    /// 创建引用物理区间的 VMO；调用者已验证该区间属于授予的 MMIO resource。
    ///
    /// # Errors
    /// 地址未对齐返回 `InvalidArgs`。
    pub(crate) fn create_physical(
        physical: u64,
        size: u64,
        cache: CachePolicy,
    ) -> Result<Arc<Self>, VmError> {
        if !physical.is_multiple_of(PAGE_SIZE as u64) {
            return Err(VmError::InvalidArgs);
        }
        let base = PhysicalPageNumber::from(usize::try_from(physical / PAGE_SIZE as u64).map_err(|_| VmError::InvalidArgs)?);
        Self::new_in(
            Self::new_tree()?,
            Kind::Physical { base, cache },
            pages_for(size)?,
            None,
        )
    }

    /// 全局唯一编号。
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    fn lock_tree(&self) -> Result<TaskMutexGuard<'_, ()>, VmError> {
        self.tree.lock.lock().map_err(|_| VmError::NoMemory)
    }

    /// 字节大小。
    pub(crate) fn size(&self) -> u64 {
        (self.state.lock().pages * PAGE_SIZE) as u64
    }

    /// 本对象自己持有的已提交字节（不含从父对象共享的页）。
    ///
    /// # Errors
    /// 等待者分配失败返回 `NoMemory`。
    pub(crate) fn committed_bytes(&self) -> Result<u64, VmError> {
        let _tree = self.lock_tree()?;
        let state = self.state.lock();
        let pages = match &self.kind {
            Kind::Paged { .. } => state.committed.iter().count(),
            Kind::Contiguous(_) | Kind::Physical { .. } => state.pages,
            Kind::Slice => 0,
        };
        Ok((pages * PAGE_SIZE) as u64)
    }

    /// 创建子对象。
    ///
    /// # Parameters
    /// - `offset`/`size`: 页对齐的字节区间（slice 必须落在父对象内）。
    /// - `kind`: 子对象语义。
    ///
    /// # Errors
    /// 参数未对齐或越界返回 `InvalidArgs`/`OutOfRange`；连续/物理对象不支持 COW 子对象。
    pub(crate) fn create_child(
        self: &Arc<Self>,
        offset: u64,
        size: u64,
        kind: ChildKind,
    ) -> Result<Arc<Self>, VmError> {
        if !offset.is_multiple_of(PAGE_SIZE as u64) {
            return Err(VmError::InvalidArgs);
        }
        let offset_pages = usize::try_from(offset / PAGE_SIZE as u64).map_err(|_| VmError::InvalidArgs)?;
        let pages = pages_for(size)?;
        let _tree = self.lock_tree()?;
        let parent_pages = self.state.lock().pages;
        match kind {
            ChildKind::Slice => {
                if offset_pages
                    .checked_add(pages)
                    .is_none_or(|end| end > parent_pages)
                {
                    return Err(VmError::OutOfRange);
                }
                Self::new_in(
                    self.tree.clone(),
                    Kind::Slice,
                    pages,
                    Some(Relation::Slice {
                        parent: self.clone(),
                        offset: offset_pages,
                    }),
                )
            }
            ChildKind::AtLeastOnWrite => {
                self.require_cow_capable()?;
                let child = Self::new_in(
                    self.tree.clone(),
                    Kind::Paged {
                        resizable: false,
                        pager: None,
                    },
                    pages,
                    Some(Relation::Cow {
                        parent: self.clone(),
                        offset: offset_pages,
                    }),
                )?;
                let mut state = self.state.lock();
                if state.children.try_reserve(1).is_err() {
                    return Err(VmError::NoMemory);
                }
                state.children.retain(|child| child.strong_count() != 0);
                state.children.push(Arc::downgrade(&child));
                Ok(child)
            }
            ChildKind::Snapshot => {
                self.require_cow_capable()?;
                if matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
                    // 被 pager 支撑的页可能被 pager 改写，无法冻结为不可变快照。
                    return Err(VmError::NotSupported);
                }
                self.snapshot(offset_pages, pages)
            }
        }
    }

    fn require_cow_capable(&self) -> Result<(), VmError> {
        match self.kind {
            Kind::Paged { .. } => Ok(()),
            Kind::Contiguous(_) | Kind::Physical { .. } | Kind::Slice => Err(VmError::NotSupported),
        }
    }

    /// 调用者持有树锁。
    fn snapshot(self: &Arc<Self>, offset: usize, pages: usize) -> Result<Arc<Self>, VmError> {
        let hidden = Self::new_in(
            self.tree.clone(),
            Kind::Paged {
                resizable: false,
                pager: None,
            },
            0,
            None,
        )?;
        let child = Self::new_in(
            self.tree.clone(),
            Kind::Paged {
                resizable: false,
                pager: None,
            },
            pages,
            Some(Relation::Cow {
                parent: hidden.clone(),
                offset,
            }),
        )?;
        {
            let mut state = self.state.lock();
            if state.committed.values().any(|page| page.pins != 0) {
                // 固定页正被 DMA 使用，移交给隐藏节点后写保护会破坏设备写入的可见性。
                return Err(VmError::BadState);
            }
            let mut hidden_state = hidden.state.lock();
            hidden_state.pages = state.pages;
            hidden_state.committed = core::mem::take(&mut state.committed);
            hidden_state.relation = state.relation.take();
            state.relation = Some(Relation::Cow {
                parent: hidden.clone(),
                offset: 0,
            });
        }
        // 已提交页现属于隐藏节点：父对象的可写翻译必须撤销，否则写入会穿透到共享页。
        let pages_total = self.state.lock().pages;
        self.zap(0..pages_total, ZapMode::RemoveWrite)?;
        Ok(child)
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
            Kind::Paged { .. } => {}
        }
        let mut state = self.state.lock();
        if index >= state.pages {
            return Err(Resolve::Error(VmError::OutOfRange));
        }
        if let Some(page) = state.committed.get_mut(&index) {
            if write {
                page.dirty = true;
            }
            return Ok(ResolvedPage {
                ppn: page.frame.ppn,
                writable: true,
                cache: CachePolicy::Cached,
            });
        }
        // 祖先链：找到页则只读共享，写入时复制到本对象。
        let mut ancestor = match &state.relation {
            Some(Relation::Cow { parent, offset }) => Some((parent.clone(), index + offset)),
            _ => None,
        };
        while let Some((vmo, ancestor_index)) = ancestor {
            let ancestor_state = vmo.state.lock();
            if ancestor_index >= ancestor_state.pages {
                break;
            }
            if let Some(page) = ancestor_state.committed.get(&ancestor_index) {
                if !write {
                    return Ok(ResolvedPage {
                        ppn: page.frame.ppn,
                        writable: false,
                        cache: CachePolicy::Cached,
                    });
                }
                let frame = frame_allocator::alloc_copy(page.frame.bytes())
                    .ok_or(Resolve::Error(VmError::NoMemory))?;
                drop(ancestor_state);
                let page = commit_page(&mut state, index, frame)?;
                drop(state);
                // 本对象与子孙的映射可能仍只读指向祖先页框；不撤销时它们在复制后继续读到旧内容
                // （缺页路径只替换发生缺页的那一条翻译，VMO 写入与调试写入不经缺页）。
                self.zap(index..index + 1, ZapMode::Unmap).map_err(Resolve::Error)?;
                return Ok(page);
            }
            let next = match &ancestor_state.relation {
                Some(Relation::Cow { parent, offset }) => Some((parent.clone(), ancestor_index + offset)),
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
        let frame = frame_allocator::alloc().ok_or(Resolve::Error(VmError::NoMemory))?;
        commit_page(&mut state, index, frame)
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
        mut with_page: impl FnMut(ResolvedPage) -> Result<R, VmError>,
    ) -> Result<R, VmError> {
        loop {
            let pending = {
                let _tree = self.lock_tree()?;
                match self.resolve_locked(index, write) {
                    Ok(page) => return with_page(page),
                    Err(Resolve::Error(error)) => return Err(error),
                    Err(Resolve::NeedPage { vmo, index }) => {
                        vmo.request_page(index)?;
                        (vmo, index)
                    }
                }
            };
            pending.0.wait_for_page(pending.1)?;
        }
    }

    /// 调用者持有树锁。
    fn request_page(&self, index: usize) -> Result<(), VmError> {
        let Kind::Paged {
            pager: Some(pager), ..
        } = &self.kind
        else {
            unreachable!("page request on a VMO without pager");
        };
        let mut state = self.state.lock();
        if state.requested.contains_key(&index) {
            return Ok(());
        }
        state
            .requested
            .try_insert(index, ())
            .map_err(|_| VmError::NoMemory)?;
        drop(state);
        pager
            .sink
            .request(PagerRequest {
                key: pager.key,
                offset: (index * PAGE_SIZE) as u64,
                length: PAGE_SIZE as u64,
            })
            .map_err(|()| VmError::Io)
    }

    fn wait_for_page(&self, index: usize) -> Result<(), VmError> {
        let mut failed = false;
        let result = self.supplied.wait_until(
            || {
                let state = self.state.lock();
                let committed = state.committed.contains_key(&index);
                failed = !committed && !state.requested.contains_key(&index);
                committed || failed
            },
            None,
        );
        match result {
            WaitResult::Woken if failed => Err(VmError::Io),
            WaitResult::Woken => Ok(()),
            WaitResult::OutOfMemory => Err(VmError::NoMemory),
            WaitResult::TimedOut | WaitResult::Interrupted => Err(VmError::Interrupted),
        }
    }

    /// pager 补页：把 `source` 中 `[source_offset, +length)` 的已提交页移入本对象
    /// `[offset, +length)` 中尚未提交的位置，然后唤醒等待者。
    ///
    /// # Errors
    /// 本对象不是 pager 支撑、参数未对齐或越界、`source` 页缺失或被固定。
    pub(crate) fn supply_pages(
        self: &Arc<Self>,
        offset: u64,
        length: u64,
        source: &Arc<Vmo>,
        source_offset: u64,
    ) -> Result<(), VmError> {
        if !matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
            return Err(VmError::NotSupported);
        }
        let (first, count) = page_range(offset, length)?;
        let (source_first, _) = page_range(source_offset, length)?;
        if Arc::ptr_eq(&self.tree, &source.tree) || !matches!(source.kind, Kind::Paged { pager: None, .. }) {
            return Err(VmError::InvalidArgs);
        }
        let _target = self.lock_tree()?;
        let _source = source.lock_tree()?;
        let mut source_state = source.state.lock();
        let mut state = self.state.lock();
        if first.checked_add(count).is_none_or(|end| end > state.pages)
            || source_first
                .checked_add(count)
                .is_none_or(|end| end > source_state.pages)
        {
            return Err(VmError::OutOfRange);
        }
        if (0..count).any(|page| {
            source_state
                .committed
                .get(&(source_first + page))
                .is_none_or(|page| page.pins != 0)
        }) {
            return Err(VmError::BadState);
        }
        for page in 0..count {
            let moved = source_state
                .committed
                .take_entry(&(source_first + page))
                .expect("validated source page disappeared");
            let index = first + page;
            state.requested.remove(&index);
            if state.committed.contains_key(&index) {
                // 已有页（并发补入或已写入）保持不变；多余的源页随 moved 释放。
                continue;
            }
            let mut moved = moved;
            moved.set_key(index);
            moved.value_mut().dirty = false;
            state.committed.commit_vacant(moved);
        }
        drop(state);
        drop(source_state);
        // 源 VMO 的映射仍可能指向被移走的页框，必须撤销。
        source.zap(source_first..source_first + count, ZapMode::Unmap)?;
        self.supplied.wake_all();
        Ok(())
    }

    /// pager 报告无法提供内容：等待者以 `Io` 失败，之后的缺页同样失败。
    /// pager 无法提供 `[offset, offset + length)`：撤销区间内已发出的请求，当前等待者以 `Io` 失败；
    /// 之后的访问重新发起请求。
    ///
    /// # Errors
    /// 不是 pager 支撑的 VMO 返回 `NotSupported`；参数未对齐返回 `InvalidArgs`。
    pub(crate) fn fail_range(&self, offset: u64, length: u64) -> Result<(), VmError> {
        if !matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
            return Err(VmError::NotSupported);
        }
        let (first, count) = page_range(offset, length)?;
        let _tree = self.lock_tree()?;
        let mut state = self.state.lock();
        for index in first..first + count {
            state.requested.remove(&index);
        }
        drop(state);
        self.supplied.wake_all();
        Ok(())
    }

    /// 返回 `[offset, offset + length)` 中第一段连续脏页的字节区间。
    ///
    /// # Errors
    /// 不是 pager 支撑的 VMO 返回 `NotSupported`。
    pub(crate) fn first_dirty_range(&self, offset: u64, length: u64) -> Result<Option<(u64, u64)>, VmError> {
        if !matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
            return Err(VmError::NotSupported);
        }
        let (first, count) = page_range(offset, length)?;
        let _tree = self.lock_tree()?;
        let state = self.state.lock();
        let mut start = None;
        let mut end = first;
        for (&index, page) in state.committed.iter_from(&first) {
            if index >= first + count {
                break;
            }
            match start {
                None if page.dirty => {
                    start = Some(index);
                    end = index + 1;
                }
                Some(_) if page.dirty && index == end => end = index + 1,
                Some(_) => break,
                None => {}
            }
        }
        Ok(start.map(|start| ((start * PAGE_SIZE) as u64, ((end - start) * PAGE_SIZE) as u64)))
    }

    /// 回写开始：清除区间脏标记并写保护映射，之后的写入重新置脏。
    ///
    /// # Errors
    /// 不是 pager 支撑的 VMO 返回 `NotSupported`。
    pub(crate) fn begin_writeback(&self, offset: u64, length: u64) -> Result<(), VmError> {
        if !matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
            return Err(VmError::NotSupported);
        }
        let (first, count) = page_range(offset, length)?;
        let _tree = self.lock_tree()?;
        self.state.lock().committed.for_each_mut(|&index, page| {
            if (first..first + count).contains(&index) {
                page.dirty = false;
            }
        });
        self.zap(first..first + count, ZapMode::RemoveWrite)
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
    pub(crate) fn op_range(self: &Arc<Self>, op: RangeOp, offset: u64, length: u64) -> Result<(), VmError> {
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
            .take_while(|(index, _)| **index < range.end)
            .any(|(_, page)| page.pins != 0)
        {
            return Err(VmError::BadState);
        }
        self.zap(range.clone(), ZapMode::Unmap)?;
        let mut state = self.state.lock();
        let mut retired = FallibleMap::new();
        loop {
            let next = state
                .committed
                .iter_from(&range.start)
                .next()
                .map(|(&index, _)| index)
                .filter(|index| *index < range.end);
            let Some(index) = next else { break };
            let entry = state.committed.take_entry(&index).expect("committed page vanished");
            retired.commit_vacant(entry);
        }
        drop(state);
        // fence 已在 zap 中完成，页框可以释放。
        drop(retired);
        Ok(())
    }

    /// 改变大小；缩小时撤销并释放越界页。
    ///
    /// # Errors
    /// 不可调整大小或有越界的固定页。
    pub(crate) fn set_size(self: &Arc<Self>, size: u64) -> Result<(), VmError> {
        let Kind::Paged { resizable: true, .. } = self.kind else {
            return Err(VmError::NotSupported);
        };
        let pages = pages_for(size)?;
        let _tree = self.lock_tree()?;
        let old = self.state.lock().pages;
        if pages < old {
            self.release_pages(pages..old)?;
        }
        self.state.lock().pages = pages;
        Ok(())
    }

    /// 固定一段页供 DMA；返回期间页不可撤销。
    ///
    /// # Errors
    /// 越界、分配失败或 pager 失败。
    pub(crate) fn pin(self: &Arc<Self>, offset: u64, length: u64) -> Result<PinnedRange, VmError> {
        let (first, count) = page_range(offset, length)?;
        let unpin_wait = TaskMutexWaitPreparation::prepare().map_err(|_| VmError::NoMemory)?;
        let mut pages = Vec::new();
        pages.try_reserve_exact(count).map_err(|_| VmError::NoMemory)?;
        for index in first..first + count {
            // 固定必须作用在本对象拥有的页上：写解析把祖先页复制到本对象。
            let ppn = self.with_resolved(index, true, |page| {
                if let Some(page) = self.state.lock().committed.get_mut(&index) {
                    page.pins += 1;
                }
                Ok(page.ppn)
            });
            match ppn {
                Ok(ppn) => pages.push(ppn),
                Err(error) => {
                    let mut wait = unpin_wait;
                    let _tree = self.tree.lock.lock_prepared(&mut wait);
                    self.unpin(first, pages.len());
                    return Err(error);
                }
            }
        }
        Ok(PinnedRange {
            vmo: self.clone(),
            first_page: first,
            pages,
            unpin_wait: Some(unpin_wait),
        })
    }

    /// 调用者持有树锁。
    fn unpin(&self, first: usize, count: usize) {
        self.state.lock().committed.for_each_mut(|&index, page| {
            if (first..first + count).contains(&index) && page.pins != 0 {
                page.pins -= 1;
            }
        });
    }

    /// 登记一个映射本对象的映射。
    ///
    /// # Errors
    /// 分配失败返回 `NoMemory`。
    pub(super) fn register_mapping(&self, mapping: &Arc<Mapping>) -> Result<(), VmError> {
        let _tree = self.lock_tree()?;
        let mut state = self.state.lock();
        state.mappings.retain(|mapping| mapping.strong_count() != 0);
        state.mappings.try_reserve(1).map_err(|_| VmError::NoMemory)?;
        state.mappings.push(Arc::downgrade(mapping));
        Ok(())
    }

    /// 调用者持有树锁：撤销映射本对象 `pages` 区间（以及 at-least-on-write 子孙对应区间）的翻译，
    /// 并在返回前完成 fence。
    fn zap(&self, pages: core::ops::Range<usize>, mode: ZapMode) -> Result<(), VmError> {
        let (mappings, children) = {
            let state = self.state.lock();
            let mut mappings = Vec::new();
            mappings
                .try_reserve_exact(state.mappings.len())
                .map_err(|_| VmError::NoMemory)?;
            mappings.extend(state.mappings.iter().filter_map(Weak::upgrade));
            let mut children = Vec::new();
            children
                .try_reserve_exact(state.children.len())
                .map_err(|_| VmError::NoMemory)?;
            children.extend(state.children.iter().filter_map(Weak::upgrade));
            (mappings, children)
        };
        for mapping in &mappings {
            mapping.zap_vmo_pages(pages.clone(), mode)?;
        }
        for child in &children {
            let offset = match &child.state.lock().relation {
                Some(Relation::Cow { offset, .. }) => *offset,
                _ => continue,
            };
            let start = pages.start.saturating_sub(offset);
            let end = pages.end.saturating_sub(offset);
            if start < end {
                child.zap(start..end, ZapMode::Unmap)?;
            }
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
    NeedPage { vmo: Arc<Vmo>, index: usize },
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

fn commit_page(state: &mut State, index: usize, frame: FrameTracker) -> Result<ResolvedPage, Resolve> {
    let ppn = frame.ppn;
    state
        .committed
        .try_insert(
            index,
            Page {
                frame,
                pins: 0,
                dirty: true,
            },
        )
        .map_err(|_| Resolve::Error(VmError::NoMemory))?;
    Ok(ResolvedPage {
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

pub(crate) static DIAG_VMOS: core::sync::atomic::AtomicIsize = core::sync::atomic::AtomicIsize::new(0);
impl Drop for Vmo {
    fn drop(&mut self) {
        DIAG_VMOS.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    }
}
