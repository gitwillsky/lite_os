//! terminal 子进程的拉起、收割与 respawn。
//!
//! `ensure_minimum` 保证桌面上始终至少有一个 terminal；进程退出后按 1s
//! 节流重启，避免 exec 风暴。`SessionChild` 为每个进程建立独立 session，且在
//! desktop 退出时杀死并收割整个进程组。

use linux_uapi::process::SessionChild;
use std::{
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    process::Command,
    time::{Duration, Instant},
};

const RESPAWN_INTERVAL: Duration = Duration::from_secs(1);

pub struct Supervisor {
    children: Vec<SessionChild>,
    next_spawn_at: Instant,
    /// 抑制连续 respawn 的重复错误日志；缺少它会在持久 spawn 故障时每秒刷屏。
    spawn_error_reported: bool,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            next_spawn_at: Instant::now(),
            spawn_error_reported: false,
        }
    }

    /// 是否处于“无存活 terminal、等待 respawn”状态（事件循环据此给 poll
    /// 加超时）。
    pub fn waiting(&self) -> bool {
        self.children.is_empty()
    }

    /// 没有任何存活 terminal 且节流窗口已过时拉起一个。
    pub fn ensure_minimum(&mut self) {
        if !self.waiting() || Instant::now() < self.next_spawn_at {
            return;
        }
        self.next_spawn_at = Instant::now() + RESPAWN_INTERVAL;
        self.spawn(b"");
    }

    /// 立即多拉一个 terminal；`command` 非空时作为 terminal 的 argv[1]。
    pub fn spawn_one(&mut self, command: &[u8]) {
        self.spawn(command);
    }

    /// 收割已经退出的 terminal；查询失败时保留 owner，避免失去清理责任。
    pub fn reap(&mut self) {
        self.children
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
    }

    fn spawn(&mut self, command: &[u8]) {
        if self.children.try_reserve(1).is_err() {
            return;
        }
        let mut process = Command::new("/bin/terminal");
        process
            .env_clear()
            .env("PATH", "/sbin:/usr/sbin:/bin:/usr/bin")
            .env("HOME", "/root");
        if !command.is_empty() {
            process.arg(OsStr::from_bytes(command));
        }
        match SessionChild::spawn(&mut process) {
            Ok(child) => {
                self.children.push(child);
                self.spawn_error_reported = false;
            }
            Err(error) if !self.spawn_error_reported => {
                eprintln!("desktop: terminal spawn failed: {error}");
                self.spawn_error_reported = true;
            }
            Err(_) => {}
        }
    }
}
