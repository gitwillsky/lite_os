//! Pager 的页移交、失败与脏页/回写事务；VMO 树、页和映射寿命仍由父模块唯一拥有。
use super::*;
use core::sync::atomic::{AtomicU8, Ordering};

const PENDING: u8 = 0;
const SUPPLIED: u8 = 1;
const FAILED: u8 = 2;

/// 一次补页请求的完成结果；供页/失败在树锁内写入，等待者可跨作废及下一次请求保活它。
pub(super) struct PageRequestCompletion {
    // OWNER: 树锁裁决唯一完成者；Release/Acquire 让脱离树锁的等待者观察该请求的最终结果。
    status: AtomicU8,
}

impl Vmo {
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
        if Arc::ptr_eq(&self.tree, &source.tree)
            || !matches!(source.kind, Kind::Paged { pager: None, .. })
        {
            return Err(VmError::InvalidArgs);
        }
        let _target = self.lock_tree()?;
        let _source = source.lock_tree()?;
        let source_state = source.state.lock();
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
                .is_none_or(|page| page.pins() != 0)
        }) {
            return Err(VmError::BadState);
        }
        // 移交阶段不允许失败：目标区间的页块在任何改动之前一次建好（可失败），之后只做无分配的插入。
        state
            .committed
            .reserve_range(first, count)
            .map_err(|()| VmError::NoMemory)?;
        // 先撤销源 VMO 的映射并完成 fence，再移交页框：两棵树锁仍由本函数持有，期间没有新的源映射；
        // 反序时重复供页的源页可能在仍被映射时释放，违反页框退休契约。
        drop(state);
        drop(source_state);
        if let Err(error) = source.zap(source_first..source_first + count, ZapMode::Unmap) {
            self.state.lock().committed.discard_empty_chunks();
            return Err(error);
        }
        let mut source_state = source.state.lock();
        let mut state = self.state.lock();
        for page in 0..count {
            let mut moved = source_state
                .committed
                .remove(&(source_first + page))
                .expect("validated source page disappeared");
            let index = first + page;
            if let Some(request) = state.requested.remove(&index) {
                request.status.store(SUPPLIED, Ordering::Release);
            }
            if state.committed.contains_key(&index) {
                // 已有页（并发补入或已写入）保持不变；多余的源页随 moved 释放。
                continue;
            }
            moved.supply_to_pager(self.page_content());
            state.committed.insert_reserved(index, moved);
        }
        drop(state);
        drop(source_state);
        self.supplied.wake_all();
        Ok(())
    }

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
        while let Some((index, _)) = state
            .requested
            .ceiling(&first)
            .filter(|(index, _)| *index < first + count)
        {
            let request = state
                .requested
                .remove(&index)
                .expect("pending request disappeared under tree lock");
            request.status.store(FAILED, Ordering::Release);
        }
        drop(state);
        self.supplied.wake_all();
        Ok(())
    }

    /// pager 与本对象分离：之后的缺页不再请求而以 `Io` 失败，已发出的请求全部以失败结束并唤醒等待者
    /// （Zircon `pager_detach_vmo`）。内容 owner 退休前调用；之后迟到的请求 packet 可安全忽略。
    ///
    /// # Errors
    /// 不是 pager 支撑的 VMO 返回 `NotSupported`。
    pub(crate) fn detach_pager(&self) -> Result<(), VmError> {
        if !matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
            return Err(VmError::NotSupported);
        }
        let _tree = self.lock_tree()?;
        let mut state = self.state.lock();
        state.detached = true;
        while let Some((index, _)) = state.requested.first_key_value() {
            if let Some(request) = state.requested.remove(&index) {
                request.status.store(FAILED, Ordering::Release);
            }
        }
        drop(state);
        self.supplied.wake_all();
        Ok(())
    }

    /// 返回 `[offset, offset + length)` 中第一段连续脏页的字节区间。
    ///
    /// # Errors
    /// 不是 pager 支撑的 VMO 返回 `NotSupported`。
    pub(crate) fn first_dirty_range(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<Option<(u64, u64)>, VmError> {
        if !matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
            return Err(VmError::NotSupported);
        }
        let (first, count) = page_range(offset, length)?;
        let _tree = self.lock_tree()?;
        let state = self.state.lock();
        let mut start = None;
        let mut end = first;
        // 未收到 WRITEBACK_END 的页仍有未持久化内容，失败重试不能漏掉 AwaitingClean。
        for (index, page) in state.committed.iter_from(&first) {
            if index >= first + count {
                break;
            }
            match start {
                None if page.cleanliness() != Cleanliness::Clean => {
                    start = Some(index);
                    end = index + 1;
                }
                Some(_) if page.cleanliness() != Cleanliness::Clean && index == end => {
                    end = index + 1
                }
                Some(_) => break,
                None => {}
            }
        }
        Ok(start.map(|start| {
            (
                (start * PAGE_SIZE) as u64,
                ((end - start) * PAGE_SIZE) as u64,
            )
        }))
    }

    /// 回写开始：区间内的脏页转为 AwaitingClean 并写保护映射，之后的写入重新置脏。
    ///
    /// # Errors
    /// 不是 pager 支撑的 VMO 返回 `NotSupported`。
    pub(crate) fn begin_writeback(&self, offset: u64, length: u64) -> Result<(), VmError> {
        if !matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
            return Err(VmError::NotSupported);
        }
        let (first, count) = page_range(offset, length)?;
        let _tree = self.lock_tree()?;
        // 先完成全部别名的 write protection/fence；失败时保留 dirty，不能把未保护的页标 clean。
        // 树锁阻止新的 write fault；保护前完成的硬件写仍包含在随后读出的 writeback 快照中。
        self.zap(first..first + count, ZapMode::RemoveWrite)?;
        self.state
            .lock()
            .committed
            .for_each_range_mut(first..first + count, |_, page| {
                if page.cleanliness() == Cleanliness::Dirty {
                    page.set_cleanliness(Cleanliness::AwaitingClean);
                }
            });
        Ok(())
    }

    /// 回写完成（Zircon `WRITEBACK_END`）：存储已确认区间内容，仍为 AwaitingClean 的页转为 Clean，
    /// 之后才可被淘汰；回写期间重新写脏的页保持 Dirty。
    ///
    /// # Errors
    /// 不是 pager 支撑的 VMO 返回 `NotSupported`。
    pub(crate) fn end_writeback(&self, offset: u64, length: u64) -> Result<(), VmError> {
        if !matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
            return Err(VmError::NotSupported);
        }
        let (first, count) = page_range(offset, length)?;
        let _tree = self.lock_tree()?;
        self.state
            .lock()
            .committed
            .for_each_range_mut(first..first + count, |_, page| {
                if page.cleanliness() == Cleanliness::AwaitingClean {
                    page.set_cleanliness(Cleanliness::Clean);
                }
            });
        Ok(())
    }
}

impl Vmo {
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
}

impl Vmo {
    /// 完成缩小后越界的读取请求，让等待者回到解析循环重新判断大小（Zircon `InvalidateReadRequestsLocked`）。
    /// 调用者持树锁。完成结果只允许解析继续，不承诺该页存在或驻留；缺失时等待越界页的线程永远等不到 supply。
    pub(super) fn invalidate_read_requests_locked(&self, range: core::ops::Range<usize>) {
        let mut state = self.state.lock();
        while let Some(index) = state
            .requested
            .ceiling(&range.start)
            .map(|(index, _)| index)
            .filter(|&index| index < range.end)
        {
            let request = state
                .requested
                .remove(&index)
                .expect("pager request disappeared under tree lock");
            request.status.store(SUPPLIED, Ordering::Release);
        }
        drop(state);
        self.supplied.wake_all();
    }

    /// 为页索引 `index` 复用或投递请求；调用者持有树锁，成功返回后登记必须具有真实出口。
    ///
    /// # Returns
    /// 本次请求的完成记录；等待者必须保活它，不能改为观察下一次请求或页的当前驻留状态。
    ///
    /// # Errors
    /// 登记分配失败返回 `NoMemory`；出口拒绝时撤销登记并返回 `Io`。
    pub(super) fn request_page(&self, index: usize) -> Result<Arc<PageRequestCompletion>, VmError> {
        let Kind::Paged {
            pager: Some(pager), ..
        } = &self.kind
        else {
            unreachable!("page request on a VMO without pager");
        };
        let mut state = self.state.lock();
        if state.detached {
            return Err(VmError::Io);
        }
        if let Some(request) = state.requested.get(&index) {
            return Ok(request.clone());
        }
        let request = Arc::try_new(PageRequestCompletion {
            status: AtomicU8::new(PENDING),
        })
        .map_err(|_| VmError::NoMemory)?;
        state
            .requested
            .try_insert(index, request.clone())
            .map_err(|_| VmError::NoMemory)?;
        drop(state);
        if pager
            .sink
            .request(PagerRequest {
                key: pager.key,
                offset: (index * PAGE_SIZE) as u64,
                length: PAGE_SIZE as u64,
            })
            .is_err()
        {
            // Tree ownership excludes a newer admission: retire only the undelivered request.
            // Keeping it would make every later reader wait forever for a packet that never existed.
            self.state.lock().requested.remove(&index);
            return Err(VmError::Io);
        }
        Ok(request)
    }

    /// 等待本次请求的结果；成功后由解析循环重新确认页，不能把合法作废转换成 I/O 失败。
    ///
    /// # Errors
    /// pager 拒绝返回 `Io`，等待准备失败返回 `NoMemory`，线程终止返回 `Interrupted`。
    pub(super) fn wait_for_page(&self, request: &PageRequestCompletion) -> Result<(), VmError> {
        let mut status = PENDING;
        let result = self.supplied.wait_until(
            || {
                status = request.status.load(Ordering::Acquire);
                status != PENDING
            },
            None,
        );
        match result {
            WaitResult::Woken if status == FAILED => Err(VmError::Io),
            WaitResult::Woken => Ok(()),
            WaitResult::OutOfMemory => Err(VmError::NoMemory),
            WaitResult::TimedOut | WaitResult::Interrupted => Err(VmError::Interrupted),
        }
    }
}
