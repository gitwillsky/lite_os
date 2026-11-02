//! 进程列表：读取 `/proc`；读取过程中消失的进程被跳过。CPU 占比用两次采样之间的滴答增量计算。
use crate::system::{Monitor, TICKS_PER_SECOND, memory_text};
use linux_uapi::{Pid, Signal, process::PidFd};
use std::{collections::HashMap, fs, io};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Application,
    Worker,
    Idle,
}

pub struct Process {
    pub pid: u32,
    pub name: String,
    pub memory_kib: Option<u64>,
    pub kind: Kind,
    /// Linux starttime distinguishes samples after a numeric PID is reused.
    pub start_ticks: u64,
    /// Opened before reading proc metadata; never reacquired when the user clicks End.
    target: Option<PidFd>,
    /// 累计 CPU 滴答（用户加内核）。
    pub cpu_ticks: u64,
    /// CPU percentage in hundredths; rounding belongs to the cell formatter only.
    pub cpu_percent: u16,
    pub threads: u32,
    pub handles: u32,
}

/// 进程页的列，顺序即显示顺序。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Pid,
    Kind,
    Cpu,
    CpuTime,
    Memory,
}

impl Column {
    pub const ALL: [Column; 6] = [
        Column::Name,
        Column::Pid,
        Column::Kind,
        Column::Cpu,
        Column::CpuTime,
        Column::Memory,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Column::Name => "映像名称",
            Column::Pid => "PID",
            Column::Kind => "类型",
            Column::Cpu => "CPU",
            Column::CpuTime => "累计 CPU 时间",
            Column::Memory => "内存",
        }
    }

    /// 初始列宽（逻辑像素）；运行中的列宽由表头拖动决定。
    pub fn initial_width(self) -> i32 {
        match self {
            Column::Name => 100,
            Column::Pid => 40,
            Column::Kind => 92,
            Column::Cpu => 56,
            Column::CpuTime => 96,
            Column::Memory => 96,
        }
    }

    /// 数值列右对齐，文本列左对齐。
    pub fn right_aligned(self) -> bool {
        !matches!(self, Column::Name | Column::Kind)
    }

    /// 单元格文本。
    pub fn text(self, process: &Process) -> String {
        match self {
            Column::Name => process.name.clone(),
            Column::Pid => {
                if process.kind == Kind::Idle {
                    "—".into()
                } else {
                    process.pid.to_string()
                }
            }
            Column::Kind => process.kind.text().into(),
            Column::Cpu => format!(
                "{}.{:02}",
                process.cpu_percent / 100,
                process.cpu_percent % 100
            ),
            Column::CpuTime => cpu_time_text(process.cpu_ticks),
            Column::Memory => process.memory_kib.map_or_else(|| "—".into(), memory_text),
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
                Column::Kind => a.kind.text().cmp(b.kind.text()),
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
/// PID 0 是统计行：使用 /proc/stat 的全机空闲增量，不从可见进程之和推算。
pub fn list(previous: &HashMap<(u32, u64), u64>, monitor: &Monitor) -> Vec<Process> {
    let mut processes: Vec<Process> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let base = entry.path();
            // 1. 先固定身份，再读取元数据；读到已消失任务时跳过，不重新打开相同 PID。
            let target = Pid::new(pid.try_into().ok()?).and_then(|pid| PidFd::open(pid).ok());
            let stat = fs::read_to_string(base.join("stat")).ok()?;
            let (cpu_ticks, start_ticks) = statistics(&stat)?;
            let status = fs::read_to_string(base.join("status")).ok()?;
            let field = |key: &str| status.lines().find_map(|line| line.strip_prefix(key));
            let kind = match field("Kthread:")?.trim() {
                "0" => Kind::Application,
                "1" => Kind::Worker,
                _ => return None,
            };
            let name = fs::read_to_string(base.join("comm"))
                .ok()?
                .trim()
                .to_owned();
            let memory_kib = if kind == Kind::Application {
                fs::read_to_string(base.join("statm"))
                    .ok()
                    .and_then(|text| {
                        text.split_whitespace()
                            .nth(1)?
                            .parse::<u64>()
                            .ok()?
                            .checked_mul(4)
                    })
            } else {
                None
            };
            // 2. pidfd 的信号 0 检查只验证权限/存活；退出或不可操作的目标仍可显示。
            let target = target.filter(|target| {
                pid > 1 && kind == Kind::Application && target.check_signal_permission().is_ok()
            });
            let cpu_percent = utilization(
                previous,
                (pid, start_ticks),
                cpu_ticks,
                monitor.interval_ticks,
            );
            let threads = field("Threads:")?.trim().parse().ok()?;
            let handles = fs::read_dir(base.join("fd")).map_or(0, |fds| fds.count() as u32);
            // 3. 最后复查 starttime，避免跨 exec/exit/PID 复用拼接不同任务的样本。
            if statistics(&fs::read_to_string(base.join("stat")).ok()?)?.1 != start_ticks {
                return None;
            }
            Some(Process {
                pid,
                name,
                memory_kib,
                kind,
                start_ticks,
                target,
                cpu_ticks,
                cpu_percent,
                threads,
                handles,
            })
        })
        .collect();
    processes.push(Process {
        pid: 0,
        name: "系统空闲".into(),
        memory_kib: None,
        kind: Kind::Idle,
        start_ticks: 0,
        target: None,
        cpu_ticks: monitor.idle_ticks,
        cpu_percent: monitor.idle_percent,
        threads: 0,
        handles: 0,
    });
    processes
}

impl Kind {
    fn text(self) -> &'static str {
        match self {
            Self::Application => "应用进程",
            Self::Worker => "服务线程",
            Self::Idle => "统计项",
        }
    }
}

impl Process {
    pub fn can_terminate(&self) -> bool {
        self.target.is_some()
    }

    /// 复制采样时固定的身份；点击时不按 PID 重查，避免结束后来复用编号的任务。
    pub fn termination_target(&self) -> io::Result<Option<PidFd>> {
        self.target.as_ref().map(PidFd::try_clone).transpose()
    }
}

fn utilization(
    previous: &HashMap<(u32, u64), u64>,
    identity: (u32, u64),
    ticks: u64,
    interval: u64,
) -> u16 {
    previous
        .get(&identity)
        .copied()
        .and_then(|old| {
            ticks
                .saturating_sub(old)
                .saturating_mul(10_000)
                .checked_div(interval)
        })
        .unwrap_or(0)
        .min(10_000) as u16
}

/// 从 comm 最后一个右括号之后解析 utime/stime/starttime，名称中的括号不改变字段编号。
fn statistics(stat: &str) -> Option<(u64, u64)> {
    let fields: Vec<_> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    let user: u64 = fields.get(11)?.parse().ok()?;
    let system: u64 = fields.get(12)?.parse().ok()?;
    Some((user.checked_add(system)?, fields.get(19)?.parse().ok()?))
}

/// 结束已固定身份的进程；保留 errno 以区分退出与权限失败。
pub fn terminate(target: PidFd) -> io::Result<()> {
    target.signal(Signal::Kill)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_identity_prevents_cpu_debt_from_a_reused_pid() {
        let stat = "52 (name ) with spaces) S 1 1 1 0 -1 0 0 0 0 0 25 7 0 0 20 0 1 0 123 4096 1";
        assert_eq!(statistics(stat), Some((32, 123)));
        let previous = HashMap::from([((52, 123), 32)]);
        assert_eq!(utilization(&previous, (52, 123), 33, 1000), 10);
        assert_eq!(utilization(&previous, (52, 124), 33, 1000), 0);
        assert_eq!(utilization(&previous, (52, 123), 33, 0), 0);
    }
}
