//! 系统总量采样：CPU 占用、内存与总数，并保存性能页要画的历史曲线。
use std::{collections::VecDeque, fs};

/// 历史曲线保留的采样数。
pub const HISTORY: usize = 120;
/// `/proc/stat` 的时钟频率（内核 `CLOCK_TICKS_PER_SECOND`）。
pub const TICKS_PER_SECOND: u64 = 100;

/// `/proc/meminfo` 里性能页用到的字段，单位 KiB；语义同 Linux 7.1 `fs/proc/meminfo.c`。
#[derive(Clone, Copy, Default)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
    /// 含 Shmem 的 Linux Cached。
    pub cached: u64,
    pub slab: u64,
    /// 至少被映射一次的私有匿名页，物理页只计一次。
    pub anon: u64,
    /// 至少被映射一次的文件、shmem 与驱动插入页。
    pub mapped: u64,
    /// tmpfs、memfd、SysV/共享匿名与 GPU（GEM shmem）缓冲。
    pub shmem: u64,
    pub kernel_stack: u64,
    pub page_tables: u64,
}

impl Memory {
    fn read() -> Self {
        let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
                .unwrap_or(0)
        };
        Self {
            total: field("MemTotal"),
            available: field("MemAvailable"),
            cached: field("Cached"),
            slab: field("Slab"),
            anon: field("AnonPages"),
            mapped: field("Mapped"),
            shmem: field("Shmem"),
            kernel_stack: field("KernelStack"),
            page_tables: field("PageTables"),
        }
    }

    /// 内核自身占用：内核堆（Slab）、task 内核栈与页表。
    pub fn kernel(&self) -> u64 {
        self.slab + self.kernel_stack + self.page_tables
    }

    /// 实际已用物理内存与总物理内存；不把它称为内存承诺，也不混入交换空间。
    pub fn usage(&self) -> (u64, u64) {
        (self.total.saturating_sub(self.available), self.total)
    }
}

/// 全系统的句柄、线程与进程数。
#[derive(Clone, Copy, Default)]
pub struct Totals {
    pub handles: u64,
    pub threads: Option<u64>,
    pub processes: Option<u64>,
}

/// 一个逻辑 CPU 的独立采样状态；按 procfs 的 CPU 编号关联，不假定编号连续。
#[derive(Clone)]
pub struct Cpu {
    pub id: u32,
    pub percent: u8,
    pub history: VecDeque<u8>,
    previous: (u64, u64),
}

#[derive(Clone)]
pub struct Monitor {
    /// 上一次的（忙碌，总计）时钟滴答。
    previous: Option<(u64, u64)>,
    pub cpu_percent: u8,
    /// 全部 CPU 合计的历史，供“所有 CPU 使用一个图表”。
    pub cpu_history: VecDeque<u8>,
    pub cpus: Vec<Cpu>,
    /// 已完成的采样次数；曲线网格按它与数据同步左移。
    pub samples: u64,
    pub memory_history: VecDeque<u8>,
    pub memory: Memory,
    pub usage_peak: u64,
    /// 全部 CPU 累计的空闲滴答，作为"System Idle Process"的 CPU 时间。
    pub idle_ticks: u64,
    /// 全机空闲百分比的百分之一；取自与 CPU 总量相同的采样区间。
    pub idle_percent: u16,
    /// 最近一个采样区间内全部 CPU 的总滴答，进程 CPU 占比以它为分母。
    pub interval_ticks: u64,
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            previous: None,
            cpu_percent: 0,
            cpu_history: VecDeque::with_capacity(HISTORY),
            cpus: Vec::new(),
            samples: 0,
            memory_history: VecDeque::with_capacity(HISTORY),
            memory: Memory::default(),
            usage_peak: 0,
            idle_ticks: 0,
            idle_percent: 0,
            interval_ticks: 0,
        }
    }

    /// 采样一次并把结果推进历史。`/proc/stat` 不可读时 CPU 保持上次的值。
    pub fn sample(&mut self) {
        self.interval_ticks = 0;
        if let Ok(text) = fs::read_to_string("/proc/stat") {
            self.sample_cpu(&text);
        }
        self.memory = Memory::read();
        let (used, limit) = self.memory.usage();
        self.usage_peak = self.usage_peak.max(used);
        let memory_percent = (used * 100).checked_div(limit).unwrap_or(0);
        push(&mut self.memory_history, memory_percent.min(100) as u8);
        push(&mut self.cpu_history, self.cpu_percent);
        self.samples += 1;
    }

    fn sample_cpu(&mut self, text: &str) {
        self.interval_ticks = 0;
        let mut cpus = Vec::new();
        let mut previous = std::mem::take(&mut self.cpus);
        // 1. 同一次读取同时解析总体和各 CPU，避免两次读取的区间不一致。
        // 2. 通过逻辑编号接续历史；新增 CPU 从零开始，已移除 CPU 不再保留曲线。
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let Some(name) = fields.next() else { continue };
            let Some(suffix) = name.strip_prefix("cpu") else {
                continue;
            };
            let Some((busy, total, idle)) = cpu_ticks(fields) else {
                continue;
            };
            if !suffix.is_empty() {
                let Ok(id) = suffix.parse() else { continue };
                let mut cpu = if let Some(index) = previous.iter().position(|cpu| cpu.id == id) {
                    previous.swap_remove(index)
                } else {
                    Cpu {
                        id,
                        percent: 0,
                        history: VecDeque::with_capacity(HISTORY),
                        previous: (busy, total),
                    }
                };
                cpu.percent = cpu_percent(cpu.previous, (busy, total));
                cpu.previous = (busy, total);
                push(&mut cpu.history, cpu.percent);
                cpus.push(cpu);
                continue;
            }
            self.idle_ticks = idle;
            if let Some((previous_busy, previous_total)) = self.previous {
                let span = total.saturating_sub(previous_total);
                if span > 0 {
                    self.interval_ticks = span;
                    self.idle_percent = (total
                        .saturating_sub(busy)
                        .saturating_sub(previous_total.saturating_sub(previous_busy))
                        .saturating_mul(10_000)
                        / span)
                        .min(10_000) as u16;
                    self.cpu_percent = cpu_percent((previous_busy, previous_total), (busy, total));
                }
            }
            self.previous = Some((busy, total));
        }
        cpus.sort_by_key(|cpu| cpu.id);
        self.cpus = cpus;
    }
}

/// 只计 Linux 前八项；guest/guest_nice 已包含在 user/nice，再加会重复计算。
fn cpu_ticks(fields: std::str::SplitWhitespace<'_>) -> Option<(u64, u64, u64)> {
    let fields: Vec<u64> = fields
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let total: u64 = fields.iter().sum();
    let idle = fields.get(3).copied()? + fields.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total, idle))
}

fn cpu_percent((old_busy, old_total): (u64, u64), (busy, total): (u64, u64)) -> u8 {
    busy.saturating_sub(old_busy)
        .saturating_mul(100)
        .checked_div(total.saturating_sub(old_total))
        .unwrap_or(0)
        .min(100) as u8
}

fn push(history: &mut VecDeque<u8>, value: u8) {
    if history.len() == HISTORY {
        history.pop_front();
    }
    history.push_back(value);
}

/// 将内存采样值转换为带二进制单位的显示文本，供进程、性能与状态栏共用。
///
/// # 参数
/// - `kib`：以 KiB 为单位的原始采样值。
///
/// # 返回
/// KiB 显示整数；MiB 及以上显示一位小数，舍入到 1024 时提升单位。
pub fn memory_text(kib: u64) -> String {
    const UNITS: [&str; 7] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB"];
    let mut unit = 0;
    let mut divisor = 1u128;
    // 用整数十分位舍入，避免浮点精度损失；u128 容纳 u64 采样值乘十，不会溢出。
    let mut tenths = u128::from(kib) * 10;
    while tenths >= 10_240 && unit + 1 < UNITS.len() {
        unit += 1;
        divisor *= 1024;
        tenths = (u128::from(kib) * 10 + divisor / 2) / divisor;
    }
    if unit == 0 {
        format!("{kib} {}", UNITS[unit])
    } else {
        format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::Monitor;

    #[test]
    fn cpu_history_follows_ids_through_reordering_and_hotplug_without_double_counting_guest() {
        let mut monitor = Monitor::new();
        monitor.sample_cpu(
            "cpu 100 0 0 100 0 0 0 0 50 0\ncpu0 100 0 0 0 0 0 0 0 50 0\ncpu3 0 0 0 100 0 0 0 0 0 0",
        );
        monitor.sample_cpu(
            "cpu 150 0 0 150 0 0 0 0 80 0\ncpu3 0 0 0 150 0 0 0 0 0 0\ncpu0 150 0 0 0 0 0 0 0 80 0",
        );
        assert_eq!(monitor.cpu_percent, 50);
        assert_eq!(monitor.idle_percent, 5_000);
        assert_eq!(monitor.interval_ticks, 100);
        assert_eq!((monitor.cpus[0].id, monitor.cpus[0].percent), (0, 100));
        assert_eq!((monitor.cpus[1].id, monitor.cpus[1].percent), (3, 0));
        monitor.sample_cpu("cpu 250 0 0 150 0 0 0 0 80 0\ncpu7 100 0 0 100 0 0 0 0 0 0\ncpu3 100 0 0 150 0 0 0 0 0 0");
        assert_eq!(
            (
                monitor.cpus[0].id,
                monitor.cpus[0].percent,
                monitor.cpus[0].history.len()
            ),
            (3, 100, 3)
        );
        assert_eq!(
            (
                monitor.cpus[1].id,
                monitor.cpus[1].percent,
                monitor.cpus[1].history.len()
            ),
            (7, 0, 1)
        );
    }
    #[test]
    fn idle_retains_fractional_load_and_zero_interval_does_not_reuse_the_old_span() {
        let mut monitor = Monitor::new();
        monitor.sample_cpu("cpu 0 0 0 0 0 0 0 0");
        monitor.sample_cpu("cpu 1 0 0 1999 0 0 0 0");
        assert_eq!(monitor.idle_percent, 9_995);
        monitor.sample_cpu("cpu 1 0 0 1999 0 0 0 0");
        assert_eq!(monitor.interval_ticks, 0);
    }
}
