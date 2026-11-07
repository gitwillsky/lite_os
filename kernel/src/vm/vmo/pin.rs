//! DMA 固定：固定期间页不会被撤销、替换、淘汰或释放。
use super::*;

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

impl Vmo {
    /// 固定一段页供 DMA；返回期间页不可撤销。
    ///
    /// # Errors
    /// 越界、分配失败或 pager 失败。
    pub(crate) fn pin(self: &Arc<Self>, offset: u64, length: u64) -> Result<PinnedRange, VmError> {
        let (mut first, count) = page_range(offset, length)?;
        // 1. slice 不拥有页；把区间逐层投影到实际 owner，不能只返回物理地址却漏记 pin。
        //    树锁内验证每层窗口，随后 owner 的强引用由 PinnedRange 保活至解除固定。
        let mut owner = self.clone();
        if matches!(self.kind, Kind::Slice) {
            let _tree = self.lock_tree()?;
            loop {
                let state = owner.state.lock();
                if first.checked_add(count).is_none_or(|end| end > state.pages) {
                    return Err(VmError::OutOfRange);
                }
                let next = match &state.relation {
                    Some(Relation::Slice { parent, offset }) => Some((parent.clone(), *offset)),
                    _ => None,
                };
                drop(state);
                let Some((parent, offset)) = next else { break };
                first = first.checked_add(offset).ok_or(VmError::OutOfRange)?;
                owner = parent;
            }
        }
        // 2. 写解析将 COW 祖先复制到 owner；持同一树锁登记 pin，出错时归还已固定的前缀。
        let unpin_wait = TaskMutexWaitPreparation::prepare().map_err(|_| VmError::NoMemory)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| VmError::NoMemory)?;
        for index in first..first + count {
            // 固定必须作用在本对象拥有的页上：写解析把祖先页复制到本对象。
            let ppn = owner.with_resolved(index, true, |page| {
                if let Some(page) = owner.state.lock().committed.get_mut(&index) {
                    page.set_pins(page.pins.checked_add(1).expect("VMO pin count overflow"));
                }
                Ok(page.ppn)
            });
            match ppn {
                Ok(ppn) => pages.push(ppn),
                Err(error) => {
                    let mut wait = unpin_wait;
                    let _tree = owner.tree.lock.lock_prepared(&mut wait);
                    owner.unpin(first, pages.len());
                    return Err(error);
                }
            }
        }
        Ok(PinnedRange {
            vmo: owner,
            first_page: first,
            pages,
            unpin_wait: Some(unpin_wait),
        })
    }

    /// 调用者持有树锁。
    fn unpin(&self, first: usize, count: usize) {
        self.state.lock().committed.for_each_mut(|&index, page| {
            if (first..first + count).contains(&index) && page.pins != 0 {
                page.set_pins(page.pins - 1);
            }
        });
    }
}
