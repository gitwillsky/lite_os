//! 进程列表：读取 `/proc`，按名称排序；读取过程中消失的进程被跳过。
use std::{fs, process::Command};

pub struct Process {
    pub pid: u32,
    pub name: String,
    pub memory_kib: u64,
}

pub fn list() -> Vec<Process> {
    let mut processes: Vec<Process> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let name = fs::read_to_string(entry.path().join("comm"))
                .ok()?
                .trim()
                .to_owned();
            let resident_pages: u64 = fs::read_to_string(entry.path().join("statm"))
                .ok()?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            Some(Process {
                pid,
                name,
                memory_kib: resident_pages * 4,
            })
        })
        .collect();
    processes.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.pid.cmp(&b.pid))
    });
    processes
}

/// 强制结束进程；权限不足等失败由 `kill` 命令的退出状态反映。
pub fn terminate(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-9", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

/// 内存列：千分位加 `KB`，如 `12,345 KB`。
pub fn memory_text(kib: u64) -> String {
    let digits = kib.to_string();
    let mut grouped = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{grouped} KB")
}
