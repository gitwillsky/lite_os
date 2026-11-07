//! 内存紧张时淘汰 pager 支撑 VMO 的干净页（Zircon pager-backed eviction）。
//!
//! 1. frame allocator 在空闲页低于低水位时置位压力提示（`frame_allocator::under_pressure`）；
//! 2. 用户缺页的异常入口（不持任何内核锁）调用 [`relieve_memory_pressure`]：从轮转游标开始扫描已登记的
//!    pager VMO，只 `try_lock` 树锁（被占用的树跳过，不等待），淘汰 Clean 且未固定的页，每次至多
//!    [`EVICT_BATCH`] 页，回到高水位即停止；
//! 3. 淘汰先撤销本对象、COW 子孙与 slice 的全部翻译并完成 fence，再释放页框；之后的访问重新向 pager
//!    请求，内容由存储补回。Dirty 与 AwaitingClean 页从不淘汰，匿名内存不换出。

use super::*;
use core::sync::atomic::{AtomicUsize, Ordering};

/// 一次缺页入口至多淘汰的页数；限制回收给单次缺页增加的延迟。
const EVICT_BATCH: usize = 256;
/// 一次撤销翻译的最长连续页数；每段只做一次 zap/fence。
const RUN_LIMIT: usize = 64;

// OWNER: 全部 pager 支撑 VMO 的 Weak 登记；`create_paged` 登记并顺带清理失效项，淘汰只升级、不保活。
// 缺失时回收者无从找到可淘汰的页。spin 锁只在登记与按下标取一项时短暂持有，从不跨树锁。
static PAGED: Mutex<Vec<Weak<Vmo>>> = Mutex::new(Vec::new());
// OWNER: 轮转起点；每次回收从上次之后的对象开始，缺失时总是淘汰同一批文件。
static CURSOR: AtomicUsize = AtomicUsize::new(0);
// OWNER: 累计淘汰页数（vmstat 的 pgsteal），只作统计；缺失时无法观察回收是否发生。
static EVICTED: AtomicUsize = AtomicUsize::new(0);

/// 累计淘汰的干净页数。
pub(crate) fn evicted_pages() -> usize {
    EVICTED.load(Ordering::Relaxed)
}

impl Vmo {
    /// 登记新建的 pager 支撑 VMO。
    pub(super) fn register_reclaimable(self: &Arc<Self>) -> Result<(), VmError> {
        let mut paged = PAGED.lock();
        paged.retain(|vmo| vmo.strong_count() != 0);
        paged.try_reserve(1).map_err(|_| VmError::NoMemory)?;
        paged.push(Arc::downgrade(self));
        Ok(())
    }

    /// 淘汰至多 `budget` 个 Clean、未固定的页，返回实际淘汰数；树锁被占用时不等待、返回 0。
    pub(super) fn evict_clean(&self, budget: usize) -> usize {
        let Some(_tree) = self.tree.lock.try_lock() else {
            return 0;
        };
        let mut evicted = 0;
        let mut cursor = 0;
        while evicted < budget {
            // 找下一段连续可淘汰页 [start, end)。
            let (start, end) = {
                let state = self.state.lock();
                let limit = (budget - evicted).min(RUN_LIMIT);
                let mut run: Option<(usize, usize)> = None;
                for (index, page) in state.committed.iter_from(&cursor) {
                    let evictable = page.is_reclaimable();
                    match run {
                        Some((start, end)) if evictable && index == end && end - start < limit => {
                            run = Some((start, end + 1));
                        }
                        Some(_) => break,
                        None if evictable => run = Some((index, index + 1)),
                        None => {}
                    }
                }
                let Some(run) = run else { break };
                run
            };
            // 撤销失败（分配等待者失败）时保留这些页，下次再试。
            if self.zap(start..end, ZapMode::Unmap).is_err() {
                break;
            }
            let mut state = self.state.lock();
            for index in start..end {
                // 树锁内页的状态不会变化；fence 已在 zap 中完成，页框可以释放。
                drop(state.committed.remove(&index));
            }
            evicted += end - start;
            EVICTED.fetch_add(end - start, Ordering::Relaxed);
            cursor = end;
        }
        evicted
    }
}

/// 空闲页低于低水位时淘汰干净的文件页；只能在不持任何内核锁的上下文（用户缺页入口）调用。
///
/// # Returns
///
/// 回收之后是否仍处于内存压力（低于低水位且尚未回到高水位）。
pub(crate) fn relieve_memory_pressure() -> bool {
    if !frame_allocator::under_pressure() {
        return false;
    }
    let mut remaining = EVICT_BATCH;
    let count = PAGED.lock().len();
    let start = CURSOR.fetch_add(1, Ordering::Relaxed);
    for step in 0..count {
        if remaining == 0 || !frame_allocator::under_pressure() {
            break;
        }
        let vmo = {
            let paged = PAGED.lock();
            // 并发登记会清理失效项、改变长度：按当前长度取模，偶尔重复或跳过一项无害。
            match paged.len() {
                0 => None,
                length => paged[(start + step) % length].upgrade(),
            }
        };
        if let Some(vmo) = vmo {
            remaining -= vmo.evict_clean(remaining);
        }
    }
    frame_allocator::under_pressure()
}
