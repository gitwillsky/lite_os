//! 八代滑动窗口回收：页块位图/链表只索引干净且未固定的页，跨 VMO 先处理最老活动代。
//! 协调器以 try-lock 合并并发回收；每次同时限制页样本和 VMO 数，所有游标跨批次保留。
use super::*;
use crate::execution::sync::IrqMutex;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use fallible_tree::FallibleMap;

const PAGE_BUDGET: usize = 256;
const OBJECT_BUDGET: usize = 64;
const SAMPLE_PAGES: usize = page_list::CHUNK_PAGES;

// OWNER: pager VMO 的唯一弱登记；节点在锁外准备，Drop 摘除，不保留死亡对象的控制块。
// registry guard 只保护索引，从不跨树锁；协调器保存稳定 koid 游标，登记不会重排已有项。
static PAGED: IrqMutex<FallibleMap<u64, Weak<Vmo>>> = IrqMutex::new(FallibleMap::new());
// OWNER: 唯一扫描/代窗口提交者；缺页路径只 try-lock，不能排队等待另一场回收。
static SCANNER: TaskMutex<Scanner> = TaskMutex::new(Scanner {
    sweep: None,
    next_sequence: 0,
});
// OWNER: 窗口的唯一规范值。高 61 位 youngest，低 3 位 youngest-oldest；写者持 SCANNER，
// 页访问只读此快照。只有旧队列完整排空后才推进 oldest，至多八代，不靠截断值猜年龄。
static WINDOW: AtomicU64 = AtomicU64::new(0);
// OWNER: 扫描器按实际执行来源累计批次样本与 fence 后释放页；Relaxed 只供观测，不参与回收或唤醒。
// 缺少来源拆分会把后台回收伪报成前台分配停顿；计数不增加锁或逐页状态。
struct Counters {
    scanned: AtomicUsize,
    evicted: AtomicUsize,
}
impl Counters {
    const fn new() -> Self {
        Self {
            scanned: AtomicUsize::new(0),
            evicted: AtomicUsize::new(0),
        }
    }
    fn record(&self, result: &Reclaimed) {
        if result.examined != 0 {
            self.scanned.fetch_add(result.examined, Ordering::Relaxed);
        }
        if result.evicted != 0 {
            self.evicted.fetch_add(result.evicted, Ordering::Relaxed);
        }
    }
}
// OWNER: 两种执行来源共用同一算法，计数仅在各自批次完成后累加；不保有页或参与策略。
static FOREGROUND: Counters = Counters::new();
// OWNER: 后台线程实际执行的扫描/释放页数，不把晋升样本混作释放成功。
static BACKGROUND: Counters = Counters::new();
// OWNER: 内存压力触发的前台回收尝试，包含协调器繁忙而未扫描的尝试；不伪造为零。
static FOREGROUND_RUNS: AtomicUsize = AtomicUsize::new(0);

/// 回收执行来源的累计观测；各项独立取样，不是页所有权或策略状态。
pub(crate) struct ReclaimStatistics {
    pub(crate) foreground_runs: usize,
    pub(crate) foreground_scanned: usize,
    pub(crate) foreground_evicted: usize,
    pub(crate) background_scanned: usize,
    pub(crate) background_evicted: usize,
}

#[derive(Clone, Copy)]
struct Window {
    oldest: u64,
    youngest: u64,
}
impl Window {
    fn load() -> Self {
        let bits = WINDOW.load(Ordering::Acquire);
        let youngest = bits >> 3;
        Self {
            oldest: youngest - (bits & 7),
            youngest,
        }
    }
    /// 调用者持 SCANNER；完整扫空旧代后才可缩小窗口，禁止覆盖仍存活的环形队列。
    fn publish(self) {
        assert!(self.oldest <= self.youngest && self.youngest - self.oldest < 8);
        assert!(
            self.youngest <= u64::MAX >> 3,
            "generation sequence exhausted"
        );
        WINDOW.store(
            (self.youngest << 3) | (self.youngest - self.oldest),
            Ordering::Release,
        );
    }
}
pub(super) fn current_generation() -> u8 {
    (WINDOW.load(Ordering::Acquire) >> 3) as u8 & 7
}

struct Scanner {
    sweep: Option<Sweep>,
    next_sequence: u64,
}
struct Sweep {
    sequence: u64,
    limit: u64,
    next_key: Option<u64>,
    nonempty: bool,
    blocked: bool,
    changed: bool,
}

/// 同一 owner 的页号升序样本；映射层以物理身份过滤 COW 遮盖，合并所有别名的访问位。
#[derive(Clone, Copy, Default)]
pub(in crate::vm) struct AccessSample {
    pub(in crate::vm) index: usize,
    pub(in crate::vm) physical_page: usize,
    pub(in crate::vm) referenced: bool,
}
#[derive(Default)]
pub(super) struct Reclaimed {
    pub(super) examined: usize,
    pub(super) evicted: usize,
    promoted: usize,
    nonempty: bool,
    blocked: bool,
}
#[derive(Default)]
struct Step {
    finished: bool,
    changed: bool,
}

/// 读取前台/后台扫描和释放的累计页数，不等待扫描器，不触发回收。
pub(crate) fn reclaim_statistics() -> ReclaimStatistics {
    ReclaimStatistics {
        foreground_runs: FOREGROUND_RUNS.load(Ordering::Relaxed),
        foreground_scanned: FOREGROUND.scanned.load(Ordering::Relaxed),
        foreground_evicted: FOREGROUND.evicted.load(Ordering::Relaxed),
        background_scanned: BACKGROUND.scanned.load(Ordering::Relaxed),
        background_evicted: BACKGROUND.evicted.load(Ordering::Relaxed),
    }
}
pub(super) fn unregister(id: u64) {
    drop(PAGED.lock().remove(&id));
}

impl Vmo {
    /// 发表弱登记；所有失败发生在索引提交之前，析构可重复尝试摘除未登记 id。
    pub(super) fn register_reclaimable(self: &Arc<Self>) -> Result<(), VmError> {
        let entry = FallibleMap::try_prepare(self.id, Arc::downgrade(self))
            .map_err(|_| VmError::NoMemory)?;
        PAGED.lock().commit_vacant(entry);
        Ok(())
    }

    /// 只采集指定活动代的候选，不遍历脏页/固定页；树锁忙时报告未知，不能据此证明该代为空。
    pub(super) fn evict_generation(&self, generation: u8, current: u8) -> Reclaimed {
        let Some(_tree) = self.tree.lock.try_lock() else {
            return Reclaimed {
                blocked: true,
                ..Reclaimed::default()
            };
        };
        let mut samples = [AccessSample::default(); SAMPLE_PAGES];
        let mut count = 0;
        self.state
            .lock()
            .committed
            .visit_generation(generation, |index, page| {
                samples[count] = AccessSample {
                    index,
                    physical_page: page.ppn().as_usize(),
                    referenced: false,
                };
                count += 1;
            });
        let samples = &mut samples[..count];
        samples.sort_unstable_by_key(|sample| sample.index);
        let mut result = Reclaimed {
            examined: count,
            ..Reclaimed::default()
        };
        if count != 0 && self.harvest_accessed(samples).is_err() {
            result.blocked = true;
        } else {
            let mut state = self.state.lock();
            for sample in samples.iter_mut() {
                state
                    .committed
                    .with_mut(&sample.index, |page| {
                        if sample.referenced && page.generation() != current {
                            page.promote_access(current);
                            result.promoted += 1;
                        }
                        sample.referenced |= !page.is_reclaimable()
                            || page.generation() != generation
                            || page.ppn().as_usize() != sample.physical_page;
                    })
                    .expect("sampled owner page disappeared under tree lock");
            }
            drop(state);
            let mut position = 0;
            while position < samples.len() {
                let sample = samples[position];
                position += 1;
                if sample.referenced {
                    continue;
                }
                let start = sample.index;
                let mut end = start + 1;
                while position < samples.len()
                    && !samples[position].referenced
                    && samples[position].index == end
                {
                    position += 1;
                    end += 1;
                }
                if self.zap(start..end, ZapMode::Unmap).is_err() {
                    result.blocked = true;
                    break;
                }
                let mut state = self.state.lock();
                for index in start..end {
                    drop(state.committed.remove(&index));
                }
                result.evicted += end - start;
            }
        }
        result.nonempty = !self.state.lock().committed.generation_empty(generation);
        result
    }
}

impl Scanner {
    fn begin(&mut self) {
        let mut window = Window::load();
        // 至少留一代可供晋升；窗口有空间时扩展历史，满八代必须先证明 oldest 全空。
        if window.youngest - window.oldest < 7 {
            window.youngest = window
                .youngest
                .checked_add(1)
                .expect("generation sequence exhausted");
            window.publish();
        }
        let sequence = self.next_sequence.max(window.oldest).min(window.youngest);
        let limit = PAGED.lock().floor(&u64::MAX).map_or(0, |(id, _)| *id);
        self.sweep = Some(Sweep {
            sequence,
            limit,
            next_key: Some(0),
            nonempty: false,
            blocked: false,
            changed: false,
        });
    }

    fn finish(&mut self) -> Step {
        let sweep = self.sweep.take().expect("missing generation sweep");
        let mut window = Window::load();
        if sweep.sequence == window.oldest
            && !sweep.nonempty
            && !sweep.blocked
            && window.oldest < window.youngest
        {
            window.oldest += 1;
            window.publish();
        }
        // 有回收/晋升优先回到最老代；旧代被锁住且无进展时，遍历其后的可用代，避免锁持有者饿死系统。
        self.next_sequence = if sweep.changed || sweep.sequence >= window.youngest {
            window.oldest
        } else {
            sweep.sequence + 1
        };
        Step {
            finished: true,
            changed: sweep.changed,
        }
    }

    fn step(&mut self, counters: &Counters) -> Step {
        if self.sweep.is_none() {
            self.begin();
        }
        let mut pages = PAGE_BUDGET;
        for _ in 0..OBJECT_BUDGET {
            let sweep = self.sweep.as_mut().expect("generation sweep missing");
            let candidate = {
                let mut registry = PAGED.lock();
                let next = sweep
                    .next_key
                    .and_then(|key| registry.ceiling(&key))
                    .filter(|(id, _)| **id <= sweep.limit)
                    .map(|(id, object)| (*id, object.upgrade()));
                if let Some((id, None)) = next {
                    drop(registry.remove(&id));
                }
                next
            };
            let Some((id, object)) = candidate else {
                return self.finish();
            };
            sweep.next_key = id.checked_add(1);
            if let Some(object) = object {
                let current = Window::load().youngest as u8 & 7;
                let result = object.evict_generation(sweep.sequence as u8 & 7, current);
                counters.record(&result);
                pages -= result.examined;
                sweep.nonempty |= result.nonempty;
                sweep.blocked |= result.blocked;
                sweep.changed |= result.evicted != 0 || result.promoted != 0;
            }
            if pages < SAMPLE_PAGES || !frame_allocator::under_pressure() {
                break;
            }
        }
        Step::default()
    }
}

fn reclaim_step(counters: &Counters) -> Step {
    let Some(mut scanner) = SCANNER.try_lock() else {
        return Step {
            finished: true,
            changed: false,
        };
    };
    scanner.step(counters)
}

/// 用户缺页入口的有界兜底；不等待并发扫描协调器，一次最多 256 个页样本、64 个 VMO。
pub(crate) fn relieve_memory_pressure() -> bool {
    if frame_allocator::under_pressure() {
        FOREGROUND_RUNS.fetch_add(1, Ordering::Relaxed);
        reclaim_step(&FOREGROUND);
    }
    frame_allocator::under_pressure()
}

/// 后台 owner 消费实际水位边沿；有界批次之间主动 yield，整轮无进展时才限速重试。
#[cfg(not(test))]
pub(crate) fn background_reclaim() {
    loop {
        while frame_allocator::under_pressure() {
            let step = reclaim_step(&BACKGROUND);
            crate::execution::sync::yield_current_task();
            if step.finished && !step.changed && frame_allocator::under_pressure() {
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
