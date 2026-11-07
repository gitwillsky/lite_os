//! 内存紧张时淘汰 pager 支撑 VMO 的干净页（Zircon pager-backed eviction）。
//!
//! 1. frame allocator 在空闲页低于低水位时置位压力提示（`frame_allocator::under_pressure`）；
//! 2. 后台线程消费压力事件，用户缺页入口保留 [`relieve_memory_pressure`] 兜底；轮转已登记 pager VMO，
//!    只 try-lock 树锁；清除访问位并 fence，给热点第二次机会，淘汰未固定干净页，回到高水位停止；
//! 3. 淘汰先撤销本对象、COW 子孙与 slice 的全部翻译并完成 fence，再释放页框；之后的访问重新向 pager
//!    请求，内容由存储补回。Dirty 与 AwaitingClean 页从不淘汰，匿名内存不换出。

use super::*;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// 一次缺页入口至多淘汰的页数；限制回收给单次缺页增加的延迟。
const EVICT_BATCH: usize = 256;
/// 一次撤销翻译的最长连续页数；每段只做一次 zap/fence。
const RUN_LIMIT: usize = 64;

// OWNER: 全部 pager 支撑 VMO 的 Weak 登记；`create_paged` 登记并顺带清理失效项，淘汰只升级、不保活。
// 缺失时回收者无从找到可淘汰的页。登记可能扩容，因此用 task mutex；取得引用后放锁，绝不跨树锁。
static PAGED: TaskMutex<Vec<Weak<Vmo>>> = TaskMutex::new(Vec::new());
// OWNER: 轮转起点；每次回收从上次之后的对象开始，缺失时总是淘汰同一批文件。
static CURSOR: AtomicUsize = AtomicUsize::new(0);
// OWNER: 累计淘汰页数（vmstat 的 pgsteal），只作统计；缺失时无法观察回收是否发生。
static EVICTED: AtomicUsize = AtomicUsize::new(0);

// OWNER: 单调前进的环形访问代号（低 3 位）；页槽存代号，采样时把旧代压到 oldest，缺失时只能按页号淘汰。
static GENERATION: AtomicU8 = AtomicU8::new(0);

pub(super) fn current_generation() -> u8 {
    GENERATION.load(Ordering::Relaxed) & 7
}

/// 同一 owner 的批量页样本；物理身份过滤 COW 遮盖，引用位是全部逻辑别名的并集。
#[derive(Clone, Copy, Default)]
pub(in crate::vm) struct AccessSample {
    pub(in crate::vm) index: usize,
    pub(in crate::vm) physical_page: usize,
    pub(in crate::vm) referenced: bool,
    age: u8,
}

/// 累计淘汰的干净页数。
pub(crate) fn evicted_pages() -> usize {
    EVICTED.load(Ordering::Relaxed)
}

impl Vmo {
    /// 登记新建的 pager 支撑 VMO。
    pub(super) fn register_reclaimable(self: &Arc<Self>) -> Result<(), VmError> {
        let mut paged = PAGED.lock().map_err(|_| VmError::NoMemory)?;
        paged.retain(|vmo| vmo.strong_count() != 0);
        paged.try_reserve(1).map_err(|_| VmError::NoMemory)?;
        paged.push(Arc::downgrade(self));
        Ok(())
    }

    /// 淘汰至多 `budget` 个未固定干净页；实际访问的页先获第二次机会。
    /// 树锁忙或采集失败时保留页框；所有别名的访问位与撤销均在同一树锁事务中完成。
    pub(super) fn evict_clean(&self, budget: usize) -> usize {
        let Some(_tree) = self.tree.lock.try_lock() else {
            return 0;
        };
        let mut evicted = 0;
        let mut examined = 0;
        let mut cursor = self.state.lock().reclaim_cursor;
        while examined < budget {
            let mut samples = [AccessSample::default(); RUN_LIMIT];
            let count = {
                let state = self.state.lock();
                let limit = (budget - examined).min(RUN_LIMIT);
                let mut count = 0;
                let start = cursor;
                let before = examined;
                for (index, page) in state.committed.iter_from(&start).take(limit) {
                    examined += 1;
                    cursor = index + 1;
                    if page.is_reclaimable() {
                        samples[count] = AccessSample {
                            index,
                            physical_page: page.ppn().as_usize(),
                            ..AccessSample::default()
                        };
                        count += 1;
                    }
                }
                if before == examined {
                    cursor = 0;
                }
                count
            };
            self.state.lock().reclaim_cursor = cursor;
            if cursor == 0 {
                break;
            }
            if count == 0 {
                continue;
            }
            let samples = &mut samples[..count];
            if self.harvest_accessed(samples).is_err() {
                break;
            }
            {
                let mut state = self.state.lock();
                let generation = current_generation();
                for sample in samples.iter_mut() {
                    let page = state
                        .committed
                        .get_mut(&sample.index)
                        .expect("sampled owner page disappeared under tree lock");
                    sample.age = page.sampled_age(generation, sample.referenced);
                }
            }
            // 同一批候选先淘汰最老代，相邻对象页下标合并为一次 zap/fence。
            for age in (0..=3).rev() {
                let mut position = 0;
                while position < samples.len() {
                    let sample = samples[position];
                    position += 1;
                    if sample.referenced || sample.age != age {
                        continue;
                    }
                    let start = sample.index;
                    let mut end = start + 1;
                    while position < samples.len()
                        && !samples[position].referenced
                        && samples[position].age == age
                        && samples[position].index == end
                    {
                        end += 1;
                        position += 1;
                    }
                    if self.zap(start..end, ZapMode::Unmap).is_err() {
                        return evicted;
                    }
                    let mut state = self.state.lock();
                    for index in start..end {
                        drop(state.committed.remove(&index));
                    }
                    evicted += end - start;
                    EVICTED.fetch_add(end - start, Ordering::Relaxed);
                }
            }
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
    let count = match PAGED.lock() {
        Ok(paged) => paged.len(),
        Err(_) => return frame_allocator::under_pressure(),
    };
    let start = CURSOR.fetch_add(1, Ordering::Relaxed);
    for step in 0..count {
        if remaining == 0 || !frame_allocator::under_pressure() {
            break;
        }
        let vmo = {
            let Ok(paged) = PAGED.lock() else { break };
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

/// 后台回收循环。main 用通用内核线程承载，VM 只经 execution 的 task seam 让出/等待。
///
/// 正常时无周期唤醒；低水位边沿到达后分批回收到高水位。若只剩脏页/固定页，限速重试，
/// 让 pager/writeback/持锁者运行；不能在同一个高优先级循环中空转等待它们释放资源。
#[cfg(not(test))]
pub(crate) fn background_reclaim() {
    loop {
        while frame_allocator::under_pressure() {
            // 代号只有后台 owner 推进；并发用户缺页不能把全局代号高速绕回。
            GENERATION.fetch_add(1, Ordering::Relaxed);
            let before = evicted_pages();
            relieve_memory_pressure();
            crate::execution::sync::yield_current_task();
            if frame_allocator::under_pressure() && evicted_pages() == before {
                let deadline = crate::execution::timer::get_time_ns().saturating_add(10_000_000);
                if frame_allocator::wait_for_pressure(Some(deadline)) == WaitResult::Interrupted {
                    return;
                }
            }
        }
        if frame_allocator::wait_for_pressure(None) == WaitResult::Interrupted {
            return;
        }
    }
}
