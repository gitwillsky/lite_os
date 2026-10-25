//! 系统总量采样：CPU 占用、内存与总数，并保存性能页要画的历史曲线。
use std::{collections::VecDeque, fs};

/// 历史曲线保留的采样数。
pub const HISTORY: usize = 120;
/// `/proc/stat` 的时钟频率（内核 `CLOCK_TICKS_PER_SECOND`）。
pub const TICKS_PER_SECOND: u64 = 100;

/// `/proc/meminfo` 里性能页用到的字段，单位 KiB。
#[derive(Clone, Copy, Default)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
    pub cached: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub slab: u64,
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
            swap_total: field("SwapTotal"),
            swap_free: field("SwapFree"),
            slab: field("Slab"),
        }
    }

    /// 已认可内存与上限：已用物理内存加已用交换空间，上限是物理内存加交换空间（Windows 的"认可用量"）。
    pub fn commit(&self) -> (u64, u64) {
        let used = self.total.saturating_sub(self.available)
            + self.swap_total.saturating_sub(self.swap_free);
        (used, self.total + self.swap_total)
    }
}

/// 全系统的句柄、线程与进程数。
#[derive(Clone, Copy, Default)]
pub struct Totals {
    pub handles: u64,
    pub threads: u64,
    pub processes: u64,
}

pub struct Monitor {
    /// 上一次的（忙碌，总计）时钟滴答。
    previous: Option<(u64, u64)>,
    pub cpu_percent: u8,
    pub cpu_history: VecDeque<u8>,
    pub memory_history: VecDeque<u8>,
    pub memory: Memory,
    pub commit_peak: u64,
    /// 全部 CPU 累计的空闲滴答，作为"System Idle Process"的 CPU 时间。
    pub idle_ticks: u64,
    /// 最近一个采样区间内全部 CPU 的总滴答，进程 CPU 占比以它为分母。
    pub interval_ticks: u64,
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            previous: None,
            cpu_percent: 0,
            cpu_history: VecDeque::with_capacity(HISTORY),
            memory_history: VecDeque::with_capacity(HISTORY),
            memory: Memory::default(),
            commit_peak: 0,
            idle_ticks: 0,
            interval_ticks: 0,
        }
    }

    /// 采样一次并把结果推进历史。`/proc/stat` 不可读时 CPU 保持上次的值。
    pub fn sample(&mut self) {
        if let Some((busy, total, idle)) = Self::read_cpu() {
            self.idle_ticks = idle;
            if let Some((previous_busy, previous_total)) = self.previous {
                let span = total.saturating_sub(previous_total);
                if span > 0 {
                    self.interval_ticks = span;
                    self.cpu_percent =
                        (busy.saturating_sub(previous_busy) * 100 / span).min(100) as u8;
                }
            }
            self.previous = Some((busy, total));
        }
        self.memory = Memory::read();
        let (used, limit) = self.memory.commit();
        self.commit_peak = self.commit_peak.max(used);
        let memory_percent = (used * 100).checked_div(limit).unwrap_or(0);
        push(&mut self.cpu_history, self.cpu_percent);
        push(&mut self.memory_history, memory_percent.min(100) as u8);
    }

    /// `/proc/stat` 首行的（忙碌，总计，空闲）滴答。
    fn read_cpu() -> Option<(u64, u64, u64)> {
        let text = fs::read_to_string("/proc/stat").ok()?;
        let fields: Vec<u64> = text
            .lines()
            .next()?
            .strip_prefix("cpu")?
            .split_whitespace()
            .filter_map(|value| value.parse().ok())
            .collect();
        let total: u64 = fields.iter().sum();
        let idle = fields.get(3).copied()? + fields.get(4).copied().unwrap_or(0);
        Some((total.saturating_sub(idle), total, idle))
    }
}

fn push(history: &mut VecDeque<u8>, value: u8) {
    if history.len() == HISTORY {
        history.pop_front();
    }
    history.push_back(value);
}
