//! `getrusage`/`wait4` 的资源使用记账：tick 采样的 user/system 拆分、缺页、上下文切换与 RSS 峰值。
//!
//! # user/system 拆分
//!
//! 调度器精确记录总 CPU runtime，但不知道其中多少花在内核里。与 Linux 的 tick 采样一致：每个 timer tick
//! 按被打断的是用户态还是内核态计入 `user_ticks` 或 `system_ticks`，读取时用两者比例把精确总 runtime
//! 切开（Linux `cputime_adjust`）。比例按 tick 采样，所以只在统计意义上准确；`system` 不单调的风险由
//! `prev_system_us` 钳制。

use core::sync::atomic::{AtomicU64, Ordering};

use super::{AddressSpace, IoStatistics, TaskControlBlock, cputime_split};

/// 一次 `getrusage` 能报告的全部有记账字段；微秒与计数均已合并 user/system 拆分。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResourceUsage {
    pub(crate) user_us: u64,
    pub(crate) system_us: u64,
    /// 峰值 RSS（KiB）。
    pub(crate) max_rss_kib: u64,
    pub(crate) minor_faults: u64,
    pub(crate) major_faults: u64,
    /// 块输入/输出次数，口径同 Linux：存储读写字节除以 512。
    pub(crate) input_blocks: u64,
    pub(crate) output_blocks: u64,
    pub(crate) voluntary_switches: u64,
    pub(crate) involuntary_switches: u64,
}

impl ResourceUsage {
    /// 累加另一份使用量；峰值 RSS 取最大值而不是求和。
    pub(crate) fn merge(&mut self, other: &Self) {
        self.user_us += other.user_us;
        self.system_us += other.system_us;
        self.max_rss_kib = self.max_rss_kib.max(other.max_rss_kib);
        self.minor_faults += other.minor_faults;
        self.major_faults += other.major_faults;
        self.input_blocks += other.input_blocks;
        self.output_blocks += other.output_blocks;
        self.voluntary_switches += other.voluntary_switches;
        self.involuntary_switches += other.involuntary_switches;
    }
}

/// 一个 Thread 或 Process 的并发使用量 counter owner。
///
/// 每个事件同步递增当前 Thread 与所属 Process；缺少 Process 份额会在 worker 退出后丢失它的历史，
/// 缺少 Thread 份额则无法回答 `RUSAGE_THREAD`。全部 Relaxed：计数之间没有需要发布的数据。
#[derive(Debug, Default)]
pub(super) struct UsageAccounting {
    user_ticks: AtomicU64,
    system_ticks: AtomicU64,
    minor_faults: AtomicU64,
    major_faults: AtomicU64,
    voluntary_switches: AtomicU64,
    involuntary_switches: AtomicU64,
    // 采样得到的 RSS 峰值（页）。RSS 由 installed 状态派生，页换出没有事件，无法逐页维护精确峰值，
    // 所以只在 exec、退出与读取时采样。
    peak_rss_pages: AtomicU64,
    // OWNER: 本结构独占；已经报告过的最大 system 时间，钳制拆分比例波动造成的回退。
    prev_system_us: AtomicU64,
}

impl UsageAccounting {
    pub(super) fn charge_tick(&self, user: bool) {
        let ticks = if user {
            &self.user_ticks
        } else {
            &self.system_ticks
        };
        ticks.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn charge_fault(&self, major: bool) {
        let faults = if major {
            &self.major_faults
        } else {
            &self.minor_faults
        };
        faults.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn charge_switch(&self, voluntary: bool) {
        let switches = if voluntary {
            &self.voluntary_switches
        } else {
            &self.involuntary_switches
        };
        switches.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn observe_rss(&self, pages: u64) {
        self.peak_rss_pages.fetch_max(pages, Ordering::Relaxed);
    }

    /// 合成当前使用量。
    ///
    /// # Parameters
    ///
    /// - `runtime_us`: 同一对象的精确总 CPU runtime。
    /// - `io`: 同一对象的 I/O 快照，提供块输入/输出。
    /// - `current_rss_pages`: 读取时刻的 RSS；与已记录的峰值取最大。
    pub(super) fn snapshot(
        &self,
        runtime_us: u64,
        io: IoStatistics,
        current_rss_pages: u64,
    ) -> ResourceUsage {
        let user_ticks = self.user_ticks.load(Ordering::Relaxed);
        let system_ticks = self.system_ticks.load(Ordering::Relaxed);
        let raw_system_us = cputime_split::estimate_system_us(runtime_us, user_ticks, system_ticks);
        let reported = self
            .prev_system_us
            .fetch_max(raw_system_us, Ordering::Relaxed);
        let system_us = cputime_split::clamp_system_us(raw_system_us, reported, runtime_us);
        let peak_pages = self
            .peak_rss_pages
            .fetch_max(current_rss_pages, Ordering::Relaxed)
            .max(current_rss_pages);
        ResourceUsage {
            user_us: runtime_us - system_us,
            system_us,
            max_rss_kib: peak_pages * (crate::memory::PAGE_SIZE as u64 / 1024),
            minor_faults: self.minor_faults.load(Ordering::Relaxed),
            major_faults: self.major_faults.load(Ordering::Relaxed),
            input_blocks: io.read_bytes / 512,
            output_blocks: io.write_bytes / 512,
            voluntary_switches: self.voluntary_switches.load(Ordering::Relaxed),
            involuntary_switches: self.involuntary_switches.load(Ordering::Relaxed),
        }
    }
}

/// 已被 wait 回收的 child（含其已回收后代）的累计使用量，供 `RUSAGE_CHILDREN`。
///
/// OWNER: Process 独占；只在 `consume_child_status` 的 graph 锁内、retire 之前累加，自动回收不累加，
/// 与 Linux 一致。缺失时 `time` 这类外壳无法报告子命令的耗时。
#[derive(Debug, Default)]
pub(super) struct ReapedChildrenUsage {
    user_us: AtomicU64,
    system_us: AtomicU64,
    max_rss_kib: AtomicU64,
    minor_faults: AtomicU64,
    major_faults: AtomicU64,
    input_blocks: AtomicU64,
    output_blocks: AtomicU64,
    voluntary_switches: AtomicU64,
    involuntary_switches: AtomicU64,
}

impl ReapedChildrenUsage {
    pub(super) fn absorb(&self, child: &ResourceUsage) {
        self.user_us.fetch_add(child.user_us, Ordering::Relaxed);
        self.system_us.fetch_add(child.system_us, Ordering::Relaxed);
        self.max_rss_kib
            .fetch_max(child.max_rss_kib, Ordering::Relaxed);
        self.minor_faults
            .fetch_add(child.minor_faults, Ordering::Relaxed);
        self.major_faults
            .fetch_add(child.major_faults, Ordering::Relaxed);
        self.input_blocks
            .fetch_add(child.input_blocks, Ordering::Relaxed);
        self.output_blocks
            .fetch_add(child.output_blocks, Ordering::Relaxed);
        self.voluntary_switches
            .fetch_add(child.voluntary_switches, Ordering::Relaxed);
        self.involuntary_switches
            .fetch_add(child.involuntary_switches, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> ResourceUsage {
        ResourceUsage {
            user_us: self.user_us.load(Ordering::Relaxed),
            system_us: self.system_us.load(Ordering::Relaxed),
            max_rss_kib: self.max_rss_kib.load(Ordering::Relaxed),
            minor_faults: self.minor_faults.load(Ordering::Relaxed),
            major_faults: self.major_faults.load(Ordering::Relaxed),
            input_blocks: self.input_blocks.load(Ordering::Relaxed),
            output_blocks: self.output_blocks.load(Ordering::Relaxed),
            voluntary_switches: self.voluntary_switches.load(Ordering::Relaxed),
            involuntary_switches: self.involuntary_switches.load(Ordering::Relaxed),
        }
    }
}

impl TaskControlBlock {
    /// 把一个 timer tick 记给 calling Thread 与所属 Process；`user` 表示 tick 打断的是用户态。
    pub(crate) fn charge_timer_tick(&self, user: bool) {
        // 内核线程没有 user Thread/Process 份额，其 tick 不属于任何 rusage。
        if self.is_kernel_thread() {
            return;
        }
        self.thread().usage.charge_tick(user);
        self.process().usage.charge_tick(user);
    }

    /// 把一次已处理的缺页记给 calling Thread 与所属 Process；`major` 表示缺页期间发生了存储读取。
    pub(crate) fn charge_page_fault(&self, major: bool) {
        if self.is_kernel_thread() {
            return;
        }
        self.thread().usage.charge_fault(major);
        self.process().usage.charge_fault(major);
    }

    /// 记录该 Thread 让出 CPU：`voluntary` 为真是自己阻塞，否则是被抢占。
    pub(crate) fn charge_context_switch(&self, voluntary: bool) {
        if self.is_kernel_thread() {
            return;
        }
        self.thread().usage.charge_switch(voluntary);
        self.process().usage.charge_switch(voluntary);
    }

    fn current_rss_pages(&self) -> u64 {
        self.address_space()
            .page_statistics()
            .map_or(0, |statistics| statistics.resident_pages as u64)
    }

    /// `RUSAGE_THREAD`：calling Thread 自己的使用量；峰值 RSS 是整个 Process 的（共享地址空间）。
    pub(crate) fn thread_resource_usage(&self) -> ResourceUsage {
        let (_, thread_runtime_us) =
            self.cpu_runtime_snapshot(crate::execution::timer::get_time_us());
        let mut usage = self.thread().usage.snapshot(
            thread_runtime_us,
            self.thread().io_accounting.snapshot(),
            0,
        );
        usage.max_rss_kib = self.process_resource_usage().max_rss_kib;
        usage
    }

    /// `RUSAGE_SELF`：calling Process 全部 Thread 的使用量。
    pub(crate) fn process_resource_usage(&self) -> ResourceUsage {
        let (process_runtime_us, _) =
            self.cpu_runtime_snapshot(crate::execution::timer::get_time_us());
        self.process().usage.snapshot(
            process_runtime_us,
            self.process().io_accounting.snapshot(),
            self.current_rss_pages(),
        )
    }

    /// `RUSAGE_CHILDREN`：已被 wait 回收的 child 及其后代。
    pub(crate) fn reaped_children_usage(&self) -> ResourceUsage {
        self.process().reaped_children.snapshot()
    }

    /// 退出时，若 calling Thread 是映像的最后一个 user，把它的 RSS 并入 Process 峰值。
    ///
    /// 其他 Thread 还在用同一映像时不采样：它们退出时最后一个会采到完整的 RSS，避免每个 Thread 退出都扫描。
    pub(in crate::execution::task) fn record_exit_peak_rss(&self) {
        let address_space = self.address_space();
        // Thread 句柄持有一份，本函数克隆的一份是第二份；超过两份说明还有别的 user。
        if alloc::sync::Arc::strong_count(&address_space) == 2 {
            self.record_peak_rss(&address_space);
        }
    }

    /// 在 exec 或退出丢弃映像之前，把它的 RSS 并入 Process 峰值。
    pub(in crate::execution::task) fn record_peak_rss(&self, address_space: &AddressSpace) {
        if let Ok(statistics) = address_space.page_statistics() {
            self.process()
                .usage
                .observe_rss(statistics.resident_pages as u64);
        }
    }
}

impl super::Process {
    /// 已退出 Process 的最终使用量（自身加上它已回收的后代），供 parent 回收时累加与 `wait4` 报告。
    pub(in crate::execution::task) fn final_resource_usage(&self) -> ResourceUsage {
        let mut usage = self.usage.snapshot(
            self.process_cpu_runtime_us(),
            self.io_accounting.snapshot(),
            0,
        );
        usage.merge(&self.reaped_children.snapshot());
        usage
    }

    pub(in crate::execution::task) fn absorb_reaped_child(&self, child: &ResourceUsage) {
        self.reaped_children.absorb(child);
    }
}
