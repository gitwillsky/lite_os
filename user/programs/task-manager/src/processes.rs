//! 进程列表：读取 `/proc`；读取过程中消失的进程被跳过。CPU 占比用两次采样之间的滴答增量计算。
use crate::system::{Monitor, TICKS_PER_SECOND};
use std::{collections::HashMap, fs, process::Command};

pub struct Process {
    pub pid: u32,
    pub name: String,
    pub memory_kib: u64,
    /// 累计 CPU 滴答（用户加内核）。
    pub cpu_ticks: u64,
    pub cpu_percent: u8,
    pub threads: u32,
    pub handles: u32,
}

/// 进程页的列，顺序即显示顺序。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Pid,
    Cpu,
    CpuTime,
    Memory,
}

impl Column {
    pub const ALL: [Column; 5] = [
        Column::Name,
        Column::Pid,
        Column::Cpu,
        Column::CpuTime,
        Column::Memory,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Column::Name => "映像名称",
            Column::Pid => "PID",
            Column::Cpu => "CPU",
            Column::CpuTime => "CPU 时间",
            Column::Memory => "内存使用",
        }
    }

    /// 列宽（逻辑像素）。
    pub fn width(self) -> i32 {
        match self {
            Column::Name => 100,
            Column::Pid => 40,
            Column::Cpu => 34,
            Column::CpuTime => 66,
            Column::Memory => 78,
        }
    }

    /// 数值列右对齐，文本列左对齐。
    pub fn right_aligned(self) -> bool {
        self != Column::Name
    }

    /// 单元格文本。
    pub fn text(self, process: &Process) -> String {
        match self {
            Column::Name => process.name.clone(),
            Column::Pid => process.pid.to_string(),
            Column::Cpu => format!("{:02}", process.cpu_percent),
            Column::CpuTime => cpu_time_text(process.cpu_ticks),
            Column::Memory => memory_text(process.memory_kib),
        }
    }
}

/// 进程页的排序：列加方向。默认按 PID 升序（Windows 2000 打开时的顺序）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub column: Column,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            column: Column::Pid,
            descending: false,
        }
    }
}

impl Sort {
    /// 点击列标题：同一列翻转方向，换列则数值列从大到小、文本列从小到大开始。
    pub fn click(&mut self, column: Column) {
        if self.column == column {
            self.descending = !self.descending;
        } else {
            *self = Self {
                column,
                descending: column.right_aligned() && column != Column::Pid,
            };
        }
    }

    pub fn apply(self, rows: &mut [Process]) {
        rows.sort_by(|a, b| {
            let order = match self.column {
                Column::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Column::Pid => a.pid.cmp(&b.pid),
                Column::Cpu => a.cpu_percent.cmp(&b.cpu_percent),
                Column::CpuTime => a.cpu_ticks.cmp(&b.cpu_ticks),
                Column::Memory => a.memory_kib.cmp(&b.memory_kib),
            };
            let order = if self.descending {
                order.reverse()
            } else {
                order
            };
            order.then(a.pid.cmp(&b.pid))
        });
    }
}

/// 读取全部进程。`previous` 是上一次各进程的累计滴答（用来算 CPU 占比），`monitor` 提供采样区间与空闲时间。
///
/// 第一项是合成的 "System Idle Process"（PID 0）：占比为系统空闲的部分，与 Windows 2000 一致。
pub fn list(previous: &HashMap<u32, u64>, monitor: &Monitor) -> Vec<Process> {
    let mut processes: Vec<Process> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let base = entry.path();
            let name = fs::read_to_string(base.join("comm"))
                .ok()?
                .trim()
                .to_owned();
            let resident_pages: u64 = fs::read_to_string(base.join("statm"))
                .ok()?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            let cpu_ticks = cpu_ticks(&fs::read_to_string(base.join("stat")).ok()?)?;
            let delta = cpu_ticks.saturating_sub(previous.get(&pid).copied().unwrap_or(cpu_ticks));
            let cpu_percent = (delta * 100)
                .checked_div(monitor.interval_ticks)
                .map_or(0, |percent| percent.min(100) as u8);
            let threads = fs::read_to_string(base.join("status"))
                .ok()
                .and_then(|status| {
                    status
                        .lines()
                        .find_map(|line| line.strip_prefix("Threads:")?.trim().parse().ok())
                })
                .unwrap_or(1);
            let handles = fs::read_dir(base.join("fd")).map_or(0, |fds| fds.count() as u32);
            Some(Process {
                pid,
                name,
                memory_kib: resident_pages * 4,
                cpu_ticks,
                cpu_percent,
                threads,
                handles,
            })
        })
        .collect();
    let busy: u32 = processes.iter().map(|p| u32::from(p.cpu_percent)).sum();
    processes.push(Process {
        pid: 0,
        name: "System Idle Process".into(),
        memory_kib: 0,
        cpu_ticks: monitor.idle_ticks,
        cpu_percent: 100u32.saturating_sub(busy).min(100) as u8,
        threads: 0,
        handles: 0,
    });
    processes
}

/// `/proc/<pid>/stat` 里的 utime + stime。进程名可以含空格与括号，所以从最后一个 `)` 之后数字段。
fn cpu_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace().skip(11);
    let user: u64 = fields.next()?.parse().ok()?;
    let system: u64 = fields.next()?.parse().ok()?;
    Some(user + system)
}

/// 强制结束进程；权限不足等失败由 `kill` 命令的退出状态反映。
pub fn terminate(pid: u32) -> bool {
    pid != 0
        && Command::new("/bin/kill")
            .args(["-9", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
}

/// CPU 时间列：`H:MM:SS`。
pub fn cpu_time_text(ticks: u64) -> String {
    let seconds = ticks / TICKS_PER_SECOND;
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// 千分位加 `K`，如 `12,345 K`（Windows 2000 的内存列格式）。
pub fn memory_text(kib: u64) -> String {
    format!("{} K", grouped(kib))
}

/// 带千分位的整数。
pub fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}
