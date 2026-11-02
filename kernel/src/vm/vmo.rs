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

mod pager;
use pager::PageRequestCompletion;

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
use fallible_tree::FallibleMap;
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
        let mut wait = self.unpin_wait.take().expect("pinned range released twice");
        let _tree = self.vmo.tree.lock.lock_prepared(&mut wait);
        self.vmo.unpin(self.first_page, self.pages.len());
    }
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
    committed: FallibleMap<usize, Page>,
    relation: Option<Relation>,
    // OWNER: 直接 COW 子对象；用于翻译撤销与判定隐藏节点是否仅剩一个子对象。Weak 不延长寿命。
    children: Vec<Weak<Vmo>>,
    // OWNER: 映射本对象的地址空间映射；Weak 在映射销毁后失效并被惰性清理。
    mappings: Vec<Weak<Mapping>>,
    // OWNER: 未完成补页的唯一登记；等待者保活本次结果，不从页是否仍驻留推断成功/失败。
    requested: FallibleMap<usize, Arc<PageRequestCompletion>>,
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
    fn new_in(
        tree: Arc<Tree>,
        kind: Kind,
        pages: usize,
        relation: Option<Relation>,
    ) -> Result<Arc<Self>, VmError> {
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
        let base = PhysicalPageNumber::from(
            usize::try_from(physical / PAGE_SIZE as u64).map_err(|_| VmError::InvalidArgs)?,
        );
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
        let guard = self.tree.lock.lock().map_err(|_| VmError::NoMemory)?;
        self.collapse_hidden_parents();
        Ok(guard)
    }

    /// 调用者持有树锁。只折叠唯一存活子对象是自己的隐藏父节点，不扫描仍有兄弟的祖先链。
    fn collapse_hidden_parents(&self) {
        loop {
            let mut state = self.state.lock();
            let (parent, offset, limit) = match &state.relation {
                Some(Relation::Cow {
                    parent,
                    offset,
                    limit,
                }) if matches!(parent.kind, Kind::Hidden) => (parent.clone(), *offset, *limit),
                _ => return,
            };
            let mut hidden = parent.state.lock();
            hidden.children.retain(|child| child.strong_count() != 0);
            if hidden.children.len() != 1 || !core::ptr::eq(hidden.children[0].as_ptr(), self) {
                return;
            }
            let child = hidden.children.pop().expect("sole COW child disappeared");
            let visible = limit
                .min(state.pages)
                .min(hidden.pages.saturating_sub(offset));
            // 1. 复用 AVL 节点转移可见且未被覆盖的页，不分配；已有页框的物理地址不变。
            // 被覆盖页没有存活子对象的翻译：COW 替换先 zap/fence，死亡兄弟的映射也已退休。
            while let Some((&index, _)) = hidden.committed.first_key_value() {
                let mut entry = hidden
                    .committed
                    .take_entry(&index)
                    .expect("hidden page disappeared");
                if index >= offset
                    && index - offset < visible
                    && !state.committed.contains_key(&(index - offset))
                {
                    entry.set_key(index - offset);
                    state.committed.commit_vacant(entry);
                }
            }
            // 2. 替换祖父的直接子对象，并把两段可见窗口求交；不可达祖先不再保活。
            state.relation = match hidden.relation.take() {
                Some(Relation::Cow {
                    parent: ancestor,
                    offset: ancestor_offset,
                    limit: ancestor_limit,
                }) => {
                    let inherited = visible.min(ancestor_limit.saturating_sub(offset));
                    let mut ancestor_state = ancestor.state.lock();
                    let position = ancestor_state
                        .children
                        .iter()
                        .position(|entry| core::ptr::eq(entry.as_ptr(), Arc::as_ptr(&parent)))
                        .expect("hidden parent absent from COW tree");
                    if inherited == 0 {
                        ancestor_state.children.swap_remove(position);
                        None
                    } else {
                        ancestor_state.children[position] = child;
                        drop(ancestor_state);
                        Some(Relation::Cow {
                            parent: ancestor,
                            offset: ancestor_offset + offset,
                            limit: inherited,
                        })
                    }
                }
                None => None,
                Some(Relation::Slice { .. }) => {
                    unreachable!("hidden node cannot have a slice relation")
                }
            };
            // 3. hidden 已无资源或祖先引用，释放它后继续折叠下一层。
        }
    }

    /// 字节大小。
    pub(crate) fn size(&self) -> u64 {
        (self.state.lock().pages * PAGE_SIZE) as u64
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
        let offset_pages =
            usize::try_from(offset / PAGE_SIZE as u64).map_err(|_| VmError::InvalidArgs)?;
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
                        limit: pages.min(parent_pages.saturating_sub(offset_pages)),
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
            Kind::Contiguous(_) | Kind::Physical { .. } | Kind::Slice | Kind::Hidden => {
                Err(VmError::NotSupported)
            }
        }
    }

    /// 调用者持有树锁。
    fn snapshot(self: &Arc<Self>, offset: usize, pages: usize) -> Result<Arc<Self>, VmError> {
        // 退出通知可先于兄弟映射退休：下一次快照时，已死亡兄弟可能落在祖先链中间。
        // 仅在快照慢路径遍历祖先，普通页访问仍只折叠直接父节点。
        let mut ancestor = match &self.state.lock().relation {
            Some(Relation::Cow { parent, .. }) => Some(parent.clone()),
            _ => None,
        };
        while let Some(node) = ancestor {
            node.collapse_hidden_parents();
            ancestor = match &node.state.lock().relation {
                Some(Relation::Cow { parent, .. }) => Some(parent.clone()),
                _ => None,
            };
        }
        let hidden = Self::new_in(self.tree.clone(), Kind::Hidden, 0, None)?;
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
                limit: pages.min(self.state.lock().pages.saturating_sub(offset)),
            }),
        )?;
        {
            let mut state = self.state.lock();
            if state.committed.values().any(|page| page.pins != 0) {
                // 固定页正被 DMA 使用，移交给隐藏节点后写保护会破坏设备写入的可见性。
                return Err(VmError::BadState);
            }
            let mut hidden_state = hidden.state.lock();
            hidden_state
                .children
                .try_reserve_exact(2)
                .map_err(|_| VmError::NoMemory)?;
            hidden_state.children.push(Arc::downgrade(self));
            hidden_state.children.push(Arc::downgrade(&child));
            if let Some(Relation::Cow { parent, .. }) = &state.relation {
                let mut parent_state = parent.state.lock();
                let entry = parent_state
                    .children
                    .iter_mut()
                    .find(|entry| core::ptr::eq(entry.as_ptr(), Arc::as_ptr(self)))
                    .expect("snapshot source absent from COW tree");
                *entry = Arc::downgrade(&hidden);
            }
            hidden_state.pages = state.pages;
            hidden_state.committed = core::mem::take(&mut state.committed);
            hidden_state.relation = state.relation.take();
            state.relation = Some(Relation::Cow {
                parent: hidden.clone(),
                offset: 0,
                limit: state.pages,
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
        if let Some(page) = state.committed.get_mut(&index) {
            if write {
                page.dirty = true;
            }
            return Ok(ResolvedPage {
                source: FaultSource::Memory,
                ppn: page.frame.ppn,
                // pager 的干净页只能安装只读翻译；否则 read fault 后的硬件写入绕过置脏，回写丢数据。
                writable: page.dirty || !matches!(self.kind, Kind::Paged { pager: Some(_), .. }),
                cache: CachePolicy::Cached,
            });
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
            let ancestor_state = vmo.state.lock();
            if ancestor_index >= ancestor_state.pages {
                break;
            }
            if let Some(page) = ancestor_state.committed.get(&ancestor_index) {
                if !write {
                    return Ok(ResolvedPage {
                        source: FaultSource::Memory,
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
        let mut source = FaultSource::Memory;
        loop {
            let pending = {
                let _tree = self.lock_tree()?;
                match self.resolve_locked(index, write) {
                    Ok(mut page) => {
                        page.source = source;
                        return with_page(page);
                    }
                    Err(Resolve::Error(error)) => return Err(error),
                    Err(Resolve::NeedPage { vmo, index }) => {
                        let request = vmo.request_page(index)?;
                        (vmo, request)
                    }
                }
            };
            pending.0.wait_for_page(&pending.1)?;
            source = FaultSource::Pager;
        }
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

    /// 读取无需等待 pager 的内容；树锁正忙或任一页需要补入时返回 `false`。
    ///
    /// # Parameters
    ///
    /// - `offset`: 首字节偏移。
    /// - `output`: 目标缓冲；返回 `false` 或错误时前缀可能已修改，调用者不得发布这些字节。
    ///
    /// # Returns
    ///
    /// 全部字节已复制为 `true`；不会发出 pager 请求，也不登记树锁等待者。
    ///
    /// # Errors
    ///
    /// 越界、匿名页分配失败或对象不支持读取。
    pub(crate) fn read_available(
        self: &Arc<Self>,
        offset: u64,
        output: &mut [u8],
    ) -> Result<bool, VmError> {
        let Some(_tree) = self.tree.lock.try_lock() else {
            return Ok(false);
        };
        let offset = usize::try_from(offset).map_err(|_| VmError::OutOfRange)?;
        let end = offset
            .checked_add(output.len())
            .ok_or(VmError::OutOfRange)?;
        let mut cursor = offset;
        while cursor < end {
            let page = match self.resolve_locked(cursor / PAGE_SIZE, false) {
                Ok(page) => page,
                Err(Resolve::NeedPage { .. }) => return Ok(false),
                Err(Resolve::Error(error)) => return Err(error),
            };
            let page_offset = cursor % PAGE_SIZE;
            let chunk = (PAGE_SIZE - page_offset).min(end - cursor);
            PageAccess(page.ppn).read(
                page_offset,
                &mut output[cursor - offset..cursor - offset + chunk],
            );
            cursor += chunk;
        }
        Ok(true)
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
                .ceiling(&range.start)
                .map(|(&index, _)| index)
                .filter(|index| *index < range.end);
            let Some(index) = next else { break };
            let entry = state
                .committed
                .take_entry(&index)
                .expect("committed page vanished");
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
        pages
            .try_reserve_exact(count)
            .map_err(|_| VmError::NoMemory)?;
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
        state
            .mappings
            .try_reserve(1)
            .map_err(|_| VmError::NoMemory)?;
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
) -> Result<ResolvedPage, Resolve> {
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
