//! weak 页面 maintenance registry；不是候选页回收队列，不能扫描未登记的 ELF MM。

use super::*;
use crate::fallible_tree::FallibleMap;
use alloc::sync::Weak;
use spin::Once;

struct Registry {
    slots: FallibleMap<usize, Weak<SwappablePage>>,
    // OWNER: 页数远大于 MM 数；每次最多检查 8 个 tombstone，不能为每个新页全表扫描。
    // cursor 只决定可复用 slot 的探测起点；identity 仍由 stable AVL slot 拥有。
    reuse_cursor: usize,
}

// OWNER: stable slots 只追加或复用 dead tombstone；强 owner 会使 unmapped tmpfs 永远存活。
// Draining 封闭目标槽集合；页在 reserve 前登记，callback 前释放 registry lock。
static PAGES: Once<Mutex<Registry>> = Once::new();

fn registry() -> &'static Mutex<Registry> {
    PAGES.call_once(|| {
        Mutex::new(Registry {
            slots: FallibleMap::new(),
            reuse_cursor: 0,
        })
    })
}

fn reuse(registry: &mut Registry, page: &Arc<SwappablePage>) -> bool {
    let length = registry.slots.len();
    for _ in 0..length.min(8) {
        let index = registry.reuse_cursor % length;
        registry.reuse_cursor = (index + 1) % length;
        let slot = registry.slots.get_mut(&index).expect("stable page slot");
        if slot.strong_count() == 0 {
            *slot = Arc::downgrade(page);
            return true;
        }
    }
    false
}

pub(super) fn register(page: &Arc<SwappablePage>) -> Result<(), SharedFileError> {
    let registry = registry();
    {
        let mut state = registry.lock();
        if reuse(&mut state, page) {
            return Ok(());
        }
    }
    let slot = FallibleMap::try_reserve_node().map_err(|_| SharedFileError::OutOfMemory)?;
    let mut state = registry.lock();
    if !reuse(&mut state, page) {
        let index = state.slots.len();
        state
            .slots
            .commit_vacant(slot.fill(index, Arc::downgrade(page)));
    }
    drop(state);
    Ok(())
}

/// 仅由已发布 Draining 的唯一 swapoff owner 调用；新页不能再领取目标 area 的槽。
pub(super) fn restore<E: From<SharedFileError>>(
    identity: SharedFileId,
    check: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let registry = registry();
    // 每个可换出页必须先登记再 reserve；Draining 之前已领取槽的所有页都在此 snapshot 内。
    // fork 只复制既有 page Arc。stable slot 不删除、live slot 不复用，单轮不会漏活 owner；
    // strong 已归零但字段仍析构的最后槽由 area 的 drain waiter 等待。
    // 无关新页不能持目标槽，不得以全局登记 generation 静止为退出条件。
    let count = registry.lock().slots.len();
    for index in 0..count {
        check()?;
        let page = registry
            .lock()
            .slots
            .get(&index)
            .cloned()
            .and_then(|page| page.upgrade());
        if let Some(page) = page {
            // 必须先等待迁移的 PageOp 再判断 identity，不能漏掉已 reserve 的 IO。
            let operation = page
                .operation
                .lock()
                .map_err(|_| SharedFileError::OutOfMemory)?;
            if page.storage() == Some(identity) {
                drop(page.pin_locked(operation)?);
            }
        }
    }
    Ok(())
}
