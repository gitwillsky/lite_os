//! 用户地址空间、VMAR 树与映射。
//!
//! # 结构
//!
//! 每个地址空间拥有一棵 VMAR 树：根 VMAR 覆盖该空间管理的地址区间，子节点是子 VMAR
//! （地址区间预留）或映射。成员的 normal/private 硬件页表视图与树由同一把地址空间锁保护。
//!
//! # 共享地址空间
//!
//! 共享空间管理高半区，其成员各自管理低半区，并在自己的页表中按需填充共享区的翻译：
//!
//! 1. 成员在共享区缺页时，到共享空间查找映射，把翻译装进成员自己的页表；
//! 2. 共享区撤销翻译时，依次持有共享空间锁与每个成员的锁，从全部页表中撤销；
//! 3. 映射的 `live` 标志在共享空间锁内清除，装页前在同一把锁内复查，迟到的缺页不会复活旧翻译。
//!
//! # 映射的修改
//!
//! 映射对象一经发布便不可变；部分解除映射或改变权限时，先在锁外构造剩余部分的新映射并登记到 VMO，
//! 再在锁内原子替换，被替换的映射保活到 TLB fence 完成后才释放。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;

use super::execution_views::{ExecutionView, ExecutionViews};
use super::page_table::PagePermissions;
use super::translation::{TranslationCommit, synchronize_address_space_retirement};
use super::vmo::{CachePolicy, FaultSource, ResolvedPage, Vmo};
use super::{FaultAccess, USER_END, USER_START, VmError};
use crate::arch::mmu::AddressSpaceToken;
use crate::execution::sync::{TaskMutex, TaskMutexGuard};
use crate::memory::{FrameTracker, PAGE_SIZE, VirtualAddress, VirtualPageNumber, frame_allocator};
use fallible_tree::FallibleMap;

/// 共享区起点：用户区间的上半部分，按 1 GiB 对齐，使两区不共享任何中间页表页。
pub(crate) const SHARED_START: usize = (USER_END / 2) & !((1 << 30) - 1);

/// 撤销翻译的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZapMode {
    /// 撤销翻译；之后访问重新缺页。
    Unmap,
    /// 只撤销写权限；之后写入重新缺页（COW、回写）。
    RemoveWrite,
}

const ROOT: u64 = 0;

/// VMO 一段区间在某个地址空间中的映射；发布后不可变。
pub(crate) struct Mapping {
    space: Weak<AddressSpace>,
    base: usize,
    size: usize,
    vmo: Arc<Vmo>,
    vmo_page: usize,
    permissions: PagePermissions,
    // OWNER: 建立映射时句柄 rights 允许的最高权限；`protect` 不得超出，缺失时只读句柄映射后可被改为可写。
    ceiling: PagePermissions,
    // FLAG: 映射仍在 VMAR 树中；在所属地址空间锁内清除与复查，缺失时迟到的缺页会复活已解除的翻译。
    live: AtomicBool,
}

impl Mapping {
    fn new(
        space: &Arc<AddressSpace>,
        base: usize,
        size: usize,
        vmo: Arc<Vmo>,
        vmo_page: usize,
        permissions: PagePermissions,
        ceiling: PagePermissions,
    ) -> Result<Arc<Self>, VmError> {
        let mapping = Arc::try_new(Self {
            space: Arc::downgrade(space),
            base,
            size,
            vmo,
            vmo_page,
            permissions,
            ceiling,
            live: AtomicBool::new(false),
        })
        .map_err(|_| VmError::NoMemory)?;
        mapping.vmo.register_mapping(&mapping)?;
        Ok(mapping)
    }

    fn end(&self) -> usize {
        self.base + self.size
    }

    /// 由 VMO 调用（持树锁）：撤销本映射中对应 VMO 页区间的翻译并完成 fence。
    pub(super) fn zap_vmo_pages(&self, pages: Range<usize>, mode: ZapMode) -> Result<(), VmError> {
        if !self.live.load(Ordering::Acquire) {
            return Ok(());
        }
        let own_end = self.vmo_page + self.size / PAGE_SIZE;
        let start = pages.start.max(self.vmo_page);
        let end = pages.end.min(own_end);
        if start >= end {
            return Ok(());
        }
        let Some(space) = self.space.upgrade() else {
            return Ok(());
        };
        let va = self.base + (start - self.vmo_page) * PAGE_SIZE
            ..self.base + (end - self.vmo_page) * PAGE_SIZE;
        space.zap_range(va, mode)
    }

    /// 由 `base` 起 `size` 字节的子区间构造新映射（权限可替换）。
    fn piece(
        &self,
        space: &Arc<AddressSpace>,
        range: Range<usize>,
        permissions: PagePermissions,
    ) -> Result<Arc<Self>, VmError> {
        Self::new(
            space,
            range.start,
            range.end - range.start,
            self.vmo.clone(),
            self.vmo_page + (range.start - self.base) / PAGE_SIZE,
            permissions,
            self.ceiling,
        )
    }
}

enum Child {
    Vmar { id: u64, size: usize },
    Mapping(Arc<Mapping>),
}

impl Child {
    fn end(&self, base: usize) -> usize {
        match self {
            Self::Vmar { size, .. } => base + size,
            Self::Mapping(mapping) => mapping.end(),
        }
    }
}

struct VmarNode {
    base: usize,
    size: usize,
    parent: Option<u64>,
    children: FallibleMap<usize, Child>,
}

struct SpaceInner {
    views: ExecutionViews,
    nodes: FallibleMap<u64, VmarNode>,
    next_id: u64,
    // OWNER: RISC-V 线程 trap context 页（supervisor-only）；AArch64 不使用。
    contexts: FallibleMap<VirtualPageNumber, FrameTracker>,
}

/// 用户地址空间。
pub(crate) struct AddressSpace {
    koid: u64,
    range: Range<usize>,
    token: AddressSpaceToken,
    // OWNER: private 根的不变身份；最终 IRQ-masked transfer 不取可睡眠的地址空间锁。
    // 两 token 仅保活各根的 ASID context，映射与页表生命周期仍由 inner 唯一管理。
    private_token: Option<AddressSpaceToken>,
    shared: Option<Arc<AddressSpace>>,
    // OWNER: 共享空间登记的成员；共享区撤销翻译时逐个处理。Weak 不延长成员寿命。
    members: Mutex<Vec<Weak<AddressSpace>>>,
    // OWNER: 页表与 VMAR 树同锁；拆开会让缺页装入已解除映射的翻译。
    inner: TaskMutex<SpaceInner>,
}

/// VMAR 句柄引用的地址区间。
pub(crate) struct Vmar {
    koid: u64,
    space: Arc<AddressSpace>,
    id: u64,
    base: usize,
    size: usize,
    permissions: PagePermissions,
}

/// VMAR 的地址区间。
#[derive(Debug, Clone, Copy)]
pub(crate) struct VmarInfo {
    pub(crate) base: usize,
    pub(crate) size: usize,
}

/// `Vmar::map`/`allocate` 的放置方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// 在 VMAR 内选择第一个足够大的空隙。
    Any,
    /// 放在 VMAR 内指定偏移，区间必须空闲。
    Specific(usize),
    /// 放在 VMAR 内指定偏移，先解除区间内已有映射。
    Overwrite(usize),
}

/// 映射的放置、权限上限与缺页准入；只用于创建事务，不复制到另一份映射状态。
pub(crate) struct MapOptions {
    /// VMAR 内的放置方式。
    pub(crate) placement: Placement,
    /// 当前映射权限。
    pub(crate) permissions: PagePermissions,
    /// VMO 句柄允许的最大权限。
    pub(crate) ceiling: PagePermissions,
    /// 允许窗口超出当前 backing，访问时再产生缺页。
    pub(crate) allow_faults: bool,
}

impl AddressSpace {
    fn create_with(
        range: Range<usize>,
        shared: Option<Arc<AddressSpace>>,
    ) -> Result<(Arc<Self>, Arc<Vmar>), VmError> {
        let views = ExecutionViews::new(shared.is_some())?;
        let permissions = PagePermissions::READ | PagePermissions::WRITE | PagePermissions::EXECUTE;
        let mut nodes = FallibleMap::new();
        nodes
            .try_insert(
                ROOT,
                VmarNode {
                    base: range.start,
                    size: range.end - range.start,
                    parent: None,
                    children: FallibleMap::new(),
                },
            )
            .map_err(|_| VmError::NoMemory)?;
        let space = Arc::try_new(Self {
            koid: crate::id::next_koid(),
            token: views.normal.token(),
            private_token: views.private_token(),
            range: range.clone(),
            shared,
            members: Mutex::new(Vec::new()),
            inner: TaskMutex::new(SpaceInner {
                views,
                nodes,
                next_id: ROOT + 1,
                contexts: FallibleMap::new(),
            }),
        })
        .map_err(|_| VmError::NoMemory)?;
        let root = Arc::try_new(Vmar {
            koid: crate::id::next_koid(),
            space: space.clone(),
            id: ROOT,
            base: range.start,
            size: range.end - range.start,
            permissions,
        })
        .map_err(|_| VmError::NoMemory)?;
        Ok((space, root))
    }

    /// 创建管理完整用户区间的独立地址空间。
    ///
    /// # Errors
    /// 页表或节点分配失败返回 `NoMemory`。
    pub(crate) fn create() -> Result<(Arc<Self>, Arc<Vmar>), VmError> {
        Self::create_with(USER_START..USER_END, None)
    }

    /// 创建只管理共享区（高半区）的共享地址空间。
    ///
    /// # Errors
    /// 同 [`Self::create`]。
    pub(crate) fn create_shared() -> Result<(Arc<Self>, Arc<Vmar>), VmError> {
        Self::create_with(SHARED_START..USER_END, None)
    }

    /// 创建共享 `shared` 高半区、自己管理低半区的成员地址空间。
    ///
    /// # Errors
    /// `shared` 不是共享空间返回 `InvalidArgs`；分配失败返回 `NoMemory`。
    pub(crate) fn create_member(shared: &Arc<Self>) -> Result<(Arc<Self>, Arc<Vmar>), VmError> {
        if shared.range.start != SHARED_START || shared.shared.is_some() {
            return Err(VmError::InvalidArgs);
        }
        let mut members = shared.members.lock();
        members.retain(|member| member.strong_count() != 0);
        members.try_reserve(1).map_err(|_| VmError::NoMemory)?;
        drop(members);
        let (space, root) = Self::create_with(USER_START..SHARED_START, Some(shared.clone()))?;
        let mut members = shared.members.lock();
        if members.try_reserve(1).is_err() {
            return Err(VmError::NoMemory);
        }
        members.push(Arc::downgrade(&space));
        Ok((space, root))
    }

    /// `address` 所在地址空间的全局编号（futex 私有 key）。
    ///
    /// 成员空间的共享区地址属于共享空间：同一共享区内的 futex 对全部成员是同一个 key，
    /// 否则在不同成员进程中运行的同一服务线程会在同一把锁上互相看不见。
    pub(crate) fn futex_owner(&self, address: usize) -> u64 {
        match &self.shared {
            Some(shared) if shared.range.contains(&address) => shared.koid,
            _ => self.koid,
        }
    }

    /// 解析地址所在映射的 VMO 与字节偏移（futex 共享 key）。
    ///
    /// # Errors
    /// 未映射返回 `NotMapped`。
    pub(crate) fn resolve_object(self: &Arc<Self>, address: usize) -> Result<(u64, u64), VmError> {
        let (_, mapping) = self.lookup(address)?;
        let offset = (mapping.vmo_page * PAGE_SIZE + (address - mapping.base)) as u64;
        Ok((mapping.vmo.id(), offset))
    }

    /// 返回用户态时激活的页表 token。
    pub(crate) fn token(&self, view: ExecutionView) -> AddressSpaceToken {
        match view {
            ExecutionView::Normal => self.token.clone(),
            ExecutionView::Private => self
                .private_token
                .as_ref()
                .expect("restricted mode requires a shared-process member")
                .clone(),
        }
    }

    /// 共享进程成员具有独立 private 硬件根；普通/共享模板进程不能进入受限模式。
    pub(crate) fn has_private_view(&self) -> bool {
        self.private_token.is_some()
    }

    /// 受限缺页不能解析共享区；否则隐藏区会在下一次重试前被错误补入或无限重试。
    pub(crate) fn fault_private(
        self: &Arc<Self>,
        address: usize,
        access: FaultAccess,
    ) -> Result<FaultSource, VmError> {
        if !self.has_private_view() || !self.range.contains(&address) {
            return Err(VmError::AccessDenied);
        }
        self.fault(address, access)
    }

    fn lock(&self) -> Result<TaskMutexGuard<'_, SpaceInner>, VmError> {
        self.inner.lock().map_err(|_| VmError::NoMemory)
    }

    fn owner_of(self: &Arc<Self>, address: usize) -> Result<Arc<Self>, VmError> {
        let owner = match &self.shared {
            Some(shared) if shared.range.contains(&address) => shared.clone(),
            _ => self.clone(),
        };
        if owner.range.contains(&address) {
            Ok(owner)
        } else {
            Err(VmError::NotMapped)
        }
    }

    fn lookup(self: &Arc<Self>, address: usize) -> Result<(Arc<Self>, Arc<Mapping>), VmError> {
        let owner = self.owner_of(address)?;
        let mapping = owner
            .lock()?
            .find_mapping(address)
            .ok_or(VmError::NotMapped)?;
        Ok((owner, mapping))
    }

    /// 调试访问（`process_read_memory`/`process_write_memory`）：经映射背后的 VMO 读写，按映射建立时的
    /// 权限上限而非当前保护属性授权——调试器可以读不可访问页、写只读代码页（写时复制的子对象只改变
    /// 本进程的私有副本）。
    ///
    /// 写入可执行映射后撤销其翻译：下一次取指缺页重新安装翻译并发布指令。
    ///
    /// # Returns
    ///
    /// 已访问的字节数；遇到未映射、超出权限上限或 VMO 访问失败时提前结束。
    pub(crate) fn access_debug(
        self: &Arc<Self>,
        address: usize,
        buffer: &mut [u8],
        write: bool,
    ) -> usize {
        let required = if write {
            PagePermissions::WRITE
        } else {
            PagePermissions::READ
        };
        let mut done = 0;
        while done < buffer.len() {
            let Some(cursor) = address.checked_add(done) else {
                break;
            };
            let Ok((_, mapping)) = self.lookup(cursor) else {
                break;
            };
            if !mapping.ceiling.contains(required) {
                break;
            }
            let chunk = (buffer.len() - done).min(mapping.base + mapping.size - cursor);
            let offset = (mapping.vmo_page * PAGE_SIZE + (cursor - mapping.base)) as u64;
            let bytes = &mut buffer[done..done + chunk];
            let result = if write {
                mapping.vmo.write(offset, bytes).and_then(|()| {
                    if !mapping.permissions.contains(PagePermissions::EXECUTE) {
                        return Ok(());
                    }
                    let first = offset as usize / PAGE_SIZE;
                    let last = (offset as usize + chunk).div_ceil(PAGE_SIZE);
                    mapping.zap_vmo_pages(first..last, ZapMode::Unmap)
                })
            } else {
                mapping.vmo.read(offset, bytes)
            };
            if result.is_err() {
                break;
            }
            done += chunk;
        }
        done
    }

    /// 处理一次用户缺页。
    ///
    /// # Errors
    /// 地址未映射返回 `NotMapped`，权限不足返回 `AccessDenied`；其余为 VMO 解析错误。
    pub(crate) fn fault(
        self: &Arc<Self>,
        address: usize,
        access: FaultAccess,
    ) -> Result<FaultSource, VmError> {
        let (owner, mapping) = self.lookup(address)?;
        if !mapping.permissions.contains(access.required()) {
            return Err(VmError::AccessDenied);
        }
        let page_address = address & !(PAGE_SIZE - 1);
        let index = mapping.vmo_page + (page_address - mapping.base) / PAGE_SIZE;
        let (mut commit, source) =
            mapping
                .vmo
                .with_resolved(index, access == FaultAccess::Write, |page| {
                    self.install(&owner, &mapping, page_address, page)
                        .map(|commit| (commit, page.source))
                })?;
        commit
            .synchronize()
            .unwrap_or_else(|error| panic!("user translation fence failed: {error}"));
        Ok(source)
    }

    /// 由 VMO 解析回调调用（持树锁）：装入一条翻译。
    fn install(
        self: &Arc<Self>,
        owner: &Arc<Self>,
        mapping: &Arc<Mapping>,
        address: usize,
        page: ResolvedPage,
    ) -> Result<TranslationCommit, VmError> {
        let mut permissions = mapping.permissions | PagePermissions::USER;
        if !page.writable {
            permissions.remove(PagePermissions::WRITE);
        }
        if page.cache == CachePolicy::Device {
            permissions |= PagePermissions::DEVICE;
        }
        let owner_guard = if Arc::ptr_eq(owner, self) {
            None
        } else {
            Some(owner.lock()?)
        };
        let mut inner = self.lock()?;
        let commit = TranslationCommit::new();
        if !mapping.live.load(Ordering::Acquire) {
            drop(owner_guard);
            return Ok(commit);
        }
        let vpn = VirtualAddress::from(address).floor();
        let commit = inner.views.install(vpn, page.ppn, permissions)?;
        drop(inner);
        drop(owner_guard);
        Ok(commit)
    }

    /// 撤销 `range` 内的翻译（本空间及全部成员），返回前完成 fence。
    pub(super) fn zap_range(&self, range: Range<usize>, mode: ZapMode) -> Result<(), VmError> {
        let mut commits = Vec::new();
        {
            let mut inner = self.lock()?;
            let members = self.member_snapshot()?;
            commits
                .try_reserve_exact(members.len() + 1)
                .map_err(|_| VmError::NoMemory)?;
            commits.push(inner.zap(range.clone(), mode)?);
            for member in &members {
                commits.push(member.lock()?.zap(range.clone(), mode)?);
            }
            drop(inner);
        }
        for mut commit in commits {
            commit
                .synchronize()
                .unwrap_or_else(|error| panic!("user translation fence failed: {error}"));
        }
        Ok(())
    }

    fn member_snapshot(&self) -> Result<Vec<Arc<Self>>, VmError> {
        let members = self.members.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(members.len())
            .map_err(|_| VmError::NoMemory)?;
        snapshot.extend(members.iter().filter_map(Weak::upgrade));
        Ok(snapshot)
    }

    /// 从当前地址空间复制用户内存；缺页按普通缺页处理。
    ///
    /// # Errors
    /// 任一字节不可读返回 `NotMapped`/`AccessDenied`。
    pub(crate) fn copy_from_user(
        self: &Arc<Self>,
        address: usize,
        output: &mut [u8],
    ) -> Result<(), VmError> {
        self.for_each_user_chunk(
            address,
            output.len(),
            FaultAccess::Read,
            |page, offset, range| {
                // SAFETY: `page` 是持地址空间锁期间已验证可读的用户页在 direct map 中的地址。
                let source = unsafe { core::slice::from_raw_parts(page.add(offset), range.len()) };
                output[range].copy_from_slice(source);
            },
        )
    }

    /// 向当前地址空间复制用户内存。
    ///
    /// # Errors
    /// 任一字节不可写返回 `NotMapped`/`AccessDenied`。
    pub(crate) fn copy_to_user(
        self: &Arc<Self>,
        address: usize,
        input: &[u8],
    ) -> Result<(), VmError> {
        self.for_each_user_chunk(
            address,
            input.len(),
            FaultAccess::Write,
            |page, offset, range| {
                // SAFETY: 同 `copy_from_user`；页已验证可写且写解析保证属于本映射的 VMO。
                let target =
                    unsafe { core::slice::from_raw_parts_mut(page.add(offset), range.len()) };
                target.copy_from_slice(&input[range]);
            },
        )
    }

    fn for_each_user_chunk(
        self: &Arc<Self>,
        address: usize,
        length: usize,
        access: FaultAccess,
        mut operation: impl FnMut(*mut u8, usize, Range<usize>),
    ) -> Result<(), VmError> {
        let end = address.checked_add(length).ok_or(VmError::NotMapped)?;
        if length != 0 && (address < USER_START || end > USER_END) {
            return Err(VmError::NotMapped);
        }
        let required = access.required() | PagePermissions::USER;
        let mut cursor = address;
        while cursor < end {
            let offset = cursor % PAGE_SIZE;
            let chunk = (PAGE_SIZE - offset).min(end - cursor);
            let start = cursor - address;
            let copied = {
                let inner = self.lock()?;
                match inner
                    .views
                    .normal
                    .translate(VirtualAddress::from(cursor).floor())
                {
                    Some(entry) if entry.permissions().contains(required) => {
                        operation(entry.ppn().as_page_mut_ptr(), offset, start..start + chunk);
                        true
                    }
                    _ => false,
                }
            };
            if copied {
                cursor += chunk;
            } else {
                self.fault(cursor, access)?;
            }
        }
        Ok(())
    }

    /// 为一个线程分配 supervisor-only 的 trap context 页（RISC-V）。
    ///
    /// # Returns
    /// 页的用户空间地址与 direct-map 指针。
    ///
    /// # Errors
    /// 区间耗尽或分配失败返回 `NoMemory`。
    pub(crate) fn allocate_context(&self) -> Result<(usize, *mut u8), VmError> {
        let range = crate::arch::mmu::THREAD_TRAP_CONTEXT_RANGE.ok_or(VmError::NotSupported)?;
        let frame = frame_allocator::alloc().ok_or(VmError::NoMemory)?;
        let pointer = frame.ppn.as_page_mut_ptr();
        let ppn = frame.ppn;
        let mut inner = self.lock()?;
        let mut address = range.end;
        let vpn = loop {
            address = address
                .checked_sub(PAGE_SIZE)
                .filter(|address| *address >= range.start)
                .ok_or(VmError::NoMemory)?;
            let vpn = VirtualAddress::from(address).floor();
            if !inner.contexts.contains_key(&vpn) {
                break vpn;
            }
        };
        inner
            .contexts
            .try_insert(vpn, frame)
            .map_err(|_| VmError::NoMemory)?;
        let mut commit = TranslationCommit::new();
        let result = inner.views.map_context(vpn, ppn, &mut commit);
        let retired = if result.is_err() {
            inner.views.unmap_context(vpn, &mut commit);
            inner.contexts.remove(&vpn)
        } else {
            None
        };
        drop(inner);
        commit
            .synchronize()
            .unwrap_or_else(|error| panic!("user translation fence failed: {error}"));
        drop(retired);
        result?;
        Ok((address, pointer))
    }

    /// 释放 [`Self::allocate_context`] 分配的页；线程已不再运行。
    pub(crate) fn release_context(&self, address: usize) {
        let vpn = VirtualAddress::from(address).floor();
        let mut commit = TranslationCommit::new();
        let frame = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|_| panic!("context release cannot allocate a waiter"));
            inner.views.unmap_context(vpn, &mut commit);
            inner
                .contexts
                .remove(&vpn)
                .expect("thread context page untracked")
        };
        commit
            .synchronize()
            .unwrap_or_else(|error| panic!("user translation fence failed: {error}"));
        drop(frame);
    }
}

impl SpaceInner {
    fn find_mapping(&self, address: usize) -> Option<Arc<Mapping>> {
        let mut node = ROOT;
        loop {
            let current = self.nodes.get(&node)?;
            let (&base, child) = current.children.floor(&address)?;
            if address >= child.end(base) {
                return None;
            }
            match child {
                Child::Vmar { id, .. } => node = *id,
                Child::Mapping(mapping) => return Some(mapping.clone()),
            }
        }
    }

    fn zap(&mut self, range: Range<usize>, mode: ZapMode) -> Result<TranslationCommit, VmError> {
        self.views.zap(range, mode)
    }

    /// 在 `node` 的子节点中找出 `[start, start + size)` 是否空闲。
    fn is_free(&self, node: &VmarNode, start: usize, size: usize) -> bool {
        let end = start + size;
        if start < node.base || end > node.base + node.size {
            return false;
        }
        if let Some((&base, child)) = node.children.floor(&start)
            && child.end(base) > start
        {
            return false;
        }
        node.children
            .ceiling(&start)
            .is_none_or(|(&base, _)| base >= end)
    }

    fn find_gap(&self, node: &VmarNode, size: usize) -> Option<usize> {
        let mut cursor = node.base;
        for (&base, child) in node.children.iter() {
            if base >= cursor && base - cursor >= size {
                return Some(cursor);
            }
            cursor = cursor.max(child.end(base));
        }
        (node.base + node.size - cursor >= size).then_some(cursor)
    }
}

impl Vmar {
    /// 对象全局编号。
    pub(crate) fn koid(&self) -> u64 {
        self.koid
    }

    /// 区间信息。
    pub(crate) fn info(&self) -> VmarInfo {
        VmarInfo {
            base: self.base,
            size: self.size,
        }
    }

    fn check_range(&self, address: usize, size: usize) -> Result<(), VmError> {
        if size == 0 || !address.is_multiple_of(PAGE_SIZE) || !size.is_multiple_of(PAGE_SIZE) {
            return Err(VmError::InvalidArgs);
        }
        let end = address.checked_add(size).ok_or(VmError::InvalidArgs)?;
        if address < self.base || end > self.base + self.size {
            return Err(VmError::OutOfRange);
        }
        Ok(())
    }

    fn place(
        &self,
        inner: &SpaceInner,
        placement: Placement,
        size: usize,
    ) -> Result<usize, VmError> {
        let node = inner.nodes.get(&self.id).ok_or(VmError::BadState)?;
        match placement {
            Placement::Any => inner.find_gap(node, size).ok_or(VmError::NoMemory),
            Placement::Specific(offset) | Placement::Overwrite(offset) => {
                let start = self.base.checked_add(offset).ok_or(VmError::InvalidArgs)?;
                if !start.is_multiple_of(PAGE_SIZE) {
                    return Err(VmError::InvalidArgs);
                }
                if inner.is_free(node, start, size) {
                    Ok(start)
                } else {
                    Err(VmError::AddressInUse)
                }
            }
        }
    }

    /// 映射 VMO 区间。
    ///
    /// # Parameters
    /// - `options`: 放置方式、权限上限与 faultable mapping 准入。
    /// - `vmo_offset`/`size`: 页对齐的 VMO 字节区间。
    ///
    /// `options.permissions` 必须是本 VMAR 权限与句柄 ceiling 的子集；之后 protect 仍受 ceiling 限制。
    ///
    /// # Returns
    /// 映射起始地址。
    ///
    /// # Errors
    /// 参数非法、越界、地址冲突、权限超出或分配失败。
    pub(crate) fn map(
        &self,
        vmo: &Arc<Vmo>,
        vmo_offset: u64,
        size: usize,
        options: MapOptions,
    ) -> Result<usize, VmError> {
        let MapOptions {
            placement,
            permissions,
            ceiling,
            allow_faults,
        } = options;
        if size == 0
            || !size.is_multiple_of(PAGE_SIZE)
            || !vmo_offset.is_multiple_of(PAGE_SIZE as u64)
        {
            return Err(VmError::InvalidArgs);
        }
        if !self.permissions.contains(permissions) || !ceiling.contains(permissions) {
            return Err(VmError::AccessDenied);
        }
        let end = vmo_offset
            .checked_add(size as u64)
            .ok_or(VmError::OutOfRange)?;
        if !allow_faults && end > vmo.size() {
            return Err(VmError::OutOfRange);
        }
        if let Placement::Overwrite(offset) = placement {
            let start = self.base.checked_add(offset).ok_or(VmError::InvalidArgs)?;
            self.unmap(start, size)?;
        }
        let vmo_page =
            usize::try_from(vmo_offset / PAGE_SIZE as u64).map_err(|_| VmError::OutOfRange)?;
        // 地址在锁内确定；映射对象先以占位地址构造并登记，锁内再确定最终地址会改变不可变对象，
        // 因此按“先选址、后构造、再复查”的顺序重试。
        loop {
            let start = {
                let inner = self.space.lock()?;
                self.place(&inner, placement, size)?
            };
            let mapping = Mapping::new(
                &self.space,
                start,
                size,
                vmo.clone(),
                vmo_page,
                permissions,
                ceiling & self.permissions,
            )?;
            let entry = FallibleMap::try_prepare(start, Child::Mapping(mapping.clone()))
                .map_err(|_| VmError::NoMemory)?;
            let mut inner = self.space.lock()?;
            let node = inner.nodes.get(&self.id).ok_or(VmError::BadState)?;
            if !inner.is_free(node, start, size) {
                if matches!(placement, Placement::Any) {
                    continue;
                }
                return Err(VmError::AddressInUse);
            }
            mapping.live.store(true, Ordering::Release);
            inner
                .nodes
                .get_mut(&self.id)
                .expect("VMAR node checked above")
                .children
                .commit_vacant(entry);
            return Ok(start);
        }
    }

    /// 分配子 VMAR。
    ///
    /// # Errors
    /// 参数非法、越界、地址冲突、权限超出或分配失败。
    pub(crate) fn allocate(
        &self,
        placement: Placement,
        size: usize,
        permissions: PagePermissions,
    ) -> Result<Arc<Vmar>, VmError> {
        if size == 0
            || !size.is_multiple_of(PAGE_SIZE)
            || matches!(placement, Placement::Overwrite(_))
        {
            return Err(VmError::InvalidArgs);
        }
        if !self.permissions.contains(permissions) {
            return Err(VmError::AccessDenied);
        }
        let mut inner = self.space.lock()?;
        let start = self.place(&inner, placement, size)?;
        let id = inner.next_id;
        let node = FallibleMap::try_prepare(
            id,
            VmarNode {
                base: start,
                size,
                parent: Some(self.id),
                children: FallibleMap::new(),
            },
        )
        .map_err(|_| VmError::NoMemory)?;
        let child = FallibleMap::try_prepare(start, Child::Vmar { id, size })
            .map_err(|_| VmError::NoMemory)?;
        let vmar = Arc::try_new(Vmar {
            koid: crate::id::next_koid(),
            space: self.space.clone(),
            id,
            base: start,
            size,
            permissions,
        })
        .map_err(|_| VmError::NoMemory)?;
        inner.next_id += 1;
        inner.nodes.commit_vacant(node);
        inner
            .nodes
            .get_mut(&self.id)
            .expect("VMAR node placed above")
            .children
            .commit_vacant(child);
        Ok(vmar)
    }

    /// 解除 `[address, address + size)` 内的映射；部分覆盖的映射被切分。
    ///
    /// # Errors
    /// 参数非法、越界或区间与子 VMAR 部分重叠。
    pub(crate) fn unmap(&self, address: usize, size: usize) -> Result<(), VmError> {
        self.check_range(address, size)?;
        self.rewrite(address..address + size, None)
    }

    /// 改变 `[address, address + size)` 内映射的权限；部分覆盖的映射被切分。
    ///
    /// # Errors
    /// 参数非法、越界、权限超出 VMAR 或映射可写上限。
    pub(crate) fn protect(
        &self,
        address: usize,
        size: usize,
        permissions: PagePermissions,
    ) -> Result<(), VmError> {
        self.check_range(address, size)?;
        if !self.permissions.contains(permissions) {
            return Err(VmError::AccessDenied);
        }
        self.rewrite(address..address + size, Some(permissions))
    }

    /// 解除（`permissions` 为 `None`）或改写区间内的映射。
    ///
    /// 1. 锁内收集与区间重叠的直接子映射；
    /// 2. 锁外构造剩余部分与改写部分的新映射并登记到 VMO；
    /// 3. 锁内复查收集的映射仍在，原子替换并撤销区间翻译；fence 后释放旧映射。
    fn rewrite(
        &self,
        range: Range<usize>,
        permissions: Option<PagePermissions>,
    ) -> Result<(), VmError> {
        loop {
            let affected = {
                let inner = self.space.lock()?;
                let node = inner.nodes.get(&self.id).ok_or(VmError::BadState)?;
                let mut affected = Vec::new();
                let first = node
                    .children
                    .floor(&range.start)
                    .map_or(range.start, |(&base, _)| base);
                for (&base, child) in node.children.iter_from(&first) {
                    if base >= range.end {
                        break;
                    }
                    if child.end(base) <= range.start {
                        continue;
                    }
                    match child {
                        Child::Vmar { .. } => return Err(VmError::BadState),
                        Child::Mapping(mapping) => {
                            affected.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                            affected.push(mapping.clone());
                        }
                    }
                }
                affected
            };
            if affected.is_empty() {
                return Ok(());
            }
            let mut replacements = Vec::new();
            for mapping in &affected {
                if let Some(permissions) = permissions
                    && !mapping.ceiling.contains(permissions)
                {
                    return Err(VmError::AccessDenied);
                }
                let left = mapping.base..range.start.max(mapping.base);
                let middle = range.start.max(mapping.base)..range.end.min(mapping.end());
                let right = range.end.min(mapping.end())..mapping.end();
                replacements.try_reserve(3).map_err(|_| VmError::NoMemory)?;
                if !left.is_empty() {
                    replacements.push(mapping.piece(&self.space, left, mapping.permissions)?);
                }
                if let Some(permissions) = permissions {
                    replacements.push(mapping.piece(&self.space, middle, permissions)?);
                }
                if !right.is_empty() {
                    replacements.push(mapping.piece(&self.space, right, mapping.permissions)?);
                }
            }
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(replacements.len())
                .map_err(|_| VmError::NoMemory)?;
            for replacement in &replacements {
                entries.push(
                    FallibleMap::try_prepare(replacement.base, Child::Mapping(replacement.clone()))
                        .map_err(|_| VmError::NoMemory)?,
                );
            }
            let mut retired = Vec::new();
            retired
                .try_reserve_exact(affected.len())
                .map_err(|_| VmError::NoMemory)?;
            let mut commits = Vec::new();
            {
                let mut inner = self.space.lock()?;
                let node = inner.nodes.get(&self.id).ok_or(VmError::BadState)?;
                let unchanged = affected.iter().all(|mapping| {
                    matches!(node.children.get(&mapping.base), Some(Child::Mapping(current)) if Arc::ptr_eq(current, mapping))
                });
                if !unchanged {
                    continue;
                }
                let node = inner
                    .nodes
                    .get_mut(&self.id)
                    .expect("VMAR node checked above");
                for mapping in &affected {
                    mapping.live.store(false, Ordering::Release);
                    if let Some(Child::Mapping(old)) = node.children.remove(&mapping.base) {
                        retired.push(old);
                    }
                }
                for (replacement, entry) in replacements.iter().zip(entries) {
                    replacement.live.store(true, Ordering::Release);
                    node.children.commit_vacant(entry);
                }
                let members = self.space.member_snapshot()?;
                commits
                    .try_reserve_exact(members.len() + 1)
                    .map_err(|_| VmError::NoMemory)?;
                commits.push(inner.zap(range.clone(), ZapMode::Unmap)?);
                for member in &members {
                    commits.push(member.lock()?.zap(range.clone(), ZapMode::Unmap)?);
                }
            }
            for mut commit in commits {
                commit
                    .synchronize()
                    .unwrap_or_else(|error| panic!("user translation fence failed: {error}"));
            }
            drop(retired);
            return Ok(());
        }
    }

    /// 销毁 VMAR：解除其中全部映射与子 VMAR，并从父 VMAR 中移除。之后对本 VMAR 的操作返回 `BadState`。
    ///
    /// # Errors
    /// 分配失败返回 `NoMemory`，已销毁返回 `BadState`。
    pub(crate) fn destroy(&self) -> Result<(), VmError> {
        let mut retired = Vec::new();
        let mut commits = Vec::new();
        {
            let mut inner = self.space.lock()?;
            let node = inner.nodes.remove(&self.id).ok_or(VmError::BadState)?;
            if let Some(parent) = node.parent
                && let Some(parent) = inner.nodes.get_mut(&parent)
            {
                drop(parent.children.remove(&node.base));
            }
            // 深度优先摘除子树，映射保活到 fence 之后。
            let mut pending = Vec::new();
            pending.try_reserve(1).map_err(|_| VmError::NoMemory)?;
            pending.push(node);
            while let Some(node) = pending.pop() {
                for (_, child) in node.children.iter() {
                    match child {
                        Child::Vmar { id, .. } => {
                            if let Some(child) = inner.nodes.remove(id) {
                                pending.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                                pending.push(child);
                            }
                        }
                        Child::Mapping(mapping) => {
                            mapping.live.store(false, Ordering::Release);
                            retired.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                            retired.push(mapping.clone());
                        }
                    }
                }
            }
            let members = self.space.member_snapshot()?;
            commits
                .try_reserve_exact(members.len() + 1)
                .map_err(|_| VmError::NoMemory)?;
            let range = self.base..self.base + self.size;
            commits.push(inner.zap(range.clone(), ZapMode::Unmap)?);
            for member in &members {
                commits.push(member.lock()?.zap(range.clone(), ZapMode::Unmap)?);
            }
        }
        for mut commit in commits {
            commit
                .synchronize()
                .unwrap_or_else(|error| panic!("user translation fence failed: {error}"));
        }
        drop(retired);
        Ok(())
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // 1. 映射与页框仍由 inner 保活；先让全部 CPU 清除本空间的翻译，ASID 才能复用。
        // 2. fence 完成后释放 ASID；字段随后析构并释放页表页、映射与页框。
        synchronize_address_space_retirement()
            .expect("platform TLB synchronization failed while retiring address space");
        self.inner.get_mut().views.retire();
    }
}
