//! 单一后台执行体的物理页压力策略；resident、writeback 与 retirement 仍归原 owner。

use super::{KERNEL_HEAP_GROWTH_PAGES, frame_allocator, shared_file};

const IDLE_INTERVAL_NS: u64 = 100_000_000;
const ACTIVE_INTERVAL_NS: u64 = 10_000_000;
const RECLAIM_BATCH_PAGES: usize = 64;

struct BackgroundReclaim {
    // OWNER: 仅后台线程的栈上状态；低水位开始、高水位结束，避免在单一阈值反复启停。
    // 不复制 allocator free count，也不持有 resident；缺失 hysteresis 会提前停止回收。
    active: bool,
}

impl BackgroundReclaim {
    fn new() -> Self {
        Self { active: false }
    }

    fn poll(&mut self) -> u64 {
        // 1. 容量与 free count 来自同一次 allocator snapshot；阈值不改变 progress reserve。
        let snapshot = frame_allocator::statistics();
        let low = (snapshot.capacity_pages / 64)
            .max(KERNEL_HEAP_GROWTH_PAGES * 4)
            .min(snapshot.capacity_pages);
        let high = (snapshot.capacity_pages / 32)
            .max(low.saturating_add(KERNEL_HEAP_GROWTH_PAGES * 4))
            .min(snapshot.capacity_pages);
        if snapshot.free_pages >= high {
            self.active = false;
        } else if snapshot.free_pages < low {
            self.active = true;
        }
        if !self.active {
            return IDLE_INTERVAL_NS;
        }
        // 2. 复用唯一 registry cursor、try-lock、writeback 与 fence-before-release transaction。
        // 每轮最多 64 页和 1024 resident scans，不分配 snapshot 或无限循环全局 owner。
        let result = shared_file::reclaim_background_pages(
            high.saturating_sub(snapshot.free_pages)
                .min(RECLAIM_BATCH_PAGES),
        );
        // 3. 始终让出 CPU；没有可回收页或 owner 正忙时按 idle 间隔退避，不忙等/杀进程。
        if result.reclaimed_pages() == 0 {
            IDLE_INTERVAL_NS
        } else {
            ACTIVE_INTERVAL_NS
        }
    }
}

/// 运行唯一的通用物理内存压力回收线程。
///
/// # Parameters
///
/// - `now`: composition root 注入的 monotonic nanosecond clock。
/// - `sleep_until`: 调度器已初始化的 absolute deadline sleep；memory 不依赖 task/timer。
///
/// # Returns
///
/// 永不返回；每轮释放全部 owner lock 后睡眠，分配失败仍由原 direct-reclaim 路径处理。
pub(crate) fn run_background_reclaim(now: fn() -> u64, mut sleep_until: impl FnMut(u64)) -> ! {
    let mut pressure = BackgroundReclaim::new();
    loop {
        let interval = pressure.poll();
        sleep_until(now().saturating_add(interval));
    }
}
