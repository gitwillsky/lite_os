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
            if let Some(request) = state.requested.remove(&index) {
                request.status.store(SUPPLIED, Ordering::Release);
            }
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
        Ok(start.map(|start| {
            (
                (start * PAGE_SIZE) as u64,
                ((end - start) * PAGE_SIZE) as u64,
            )
        }))
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
        // 先完成全部别名的 write protection/fence；失败时保留 dirty，不能把未保护的页标 clean。
        // 树锁阻止新的 write fault；保护前完成的硬件写仍包含在随后读出的 writeback 快照中。
        self.zap(first..first + count, ZapMode::RemoveWrite)?;
        self.state.lock().committed.for_each_mut(|&index, page| {
            if (first..first + count).contains(&index) {
                page.dirty = false;
            }
        });
        Ok(())
    }
}

impl Vmo {
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
