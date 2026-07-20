//! terminal 子进程的拉起 / 收割 / respawn。
//!
//! listen socket 就绪后 `fork` + `execve("/bin/terminal")`。子进程 pid 保存在
//! 固定数组（上限 [`MAX_CLIENTS`]）：`ensure_minimum` 保证桌面上始终至少有
//! 一个 terminal（死亡经 `waitpid(WNOHANG)` 收割后按 1s 节流 respawn，避免
//! exec 风暴；事件循环在 `waiting()` 期间给 poll 加超时驱动重试），
//! `spawn_one` 供开始菜单立即多拉一个（数组满时静默忽略）。
//!
//! `spawn_one` 可携带命令字符串：非空时作为 terminal 的 argv[1] 传入
//! （terminal 把 argv[1..] 按空格 join 注入 PTY 执行并 SET_TITLE）；空命令
//! 则 argv 仅 "terminal"（普通终端）。

use crate::{clients::MAX_CLIENTS, ffi};

/// respawn 节流间隔（毫秒）。
const RESPAWN_INTERVAL_MS: u64 = 1_000;
/// 命令字节上限（开始菜单 conf 解析已按 96B 截断，留 NUL 余量）。
const MAX_COMMAND: usize = 128;

pub struct Supervisor {
    /// 存活的 terminal pid 数组；-1 表示空槽。
    children: [i32; MAX_CLIENTS],
    /// 下一次允许 ensure_minimum spawn 的单调时间（毫秒）。
    next_spawn_at: u64,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            children: [-1; MAX_CLIENTS],
            next_spawn_at: 0,
        }
    }

    /// 是否处于“无存活 terminal、等待 respawn”状态（事件循环据此给 poll
    /// 加超时）。
    pub fn waiting(&self) -> bool {
        self.children.iter().all(|pid| *pid < 0)
    }

    /// 没有任何存活 terminal 且节流窗口已过时拉起一个。
    pub fn ensure_minimum(&mut self) {
        if !self.waiting() {
            return;
        }
        let now = ffi::monotonic_milliseconds();
        if now < self.next_spawn_at {
            return;
        }
        self.next_spawn_at = now + RESPAWN_INTERVAL_MS;
        self.spawn(b"");
    }

    /// 立即多拉一个 terminal（开始菜单程序项 / `终端` 项）；有空槽才 fork，
    /// 失败静默忽略（下一次 `ensure_minimum` 仍会兜底保证至少一个）。
    ///
    /// `command` 非空时作为 terminal 的 argv[1] 传入（terminal 注入 PTY 执行）；
    /// 空命令打开普通终端。
    pub fn spawn_one(&mut self, command: &[u8]) {
        self.spawn(command);
    }

    /// `waitpid(-1, WNOHANG)` 收割所有已退出子进程并按 pid 从数组移除。
    pub fn reap(&mut self) {
        loop {
            let mut status = 0;
            // SAFETY: status 在 waitpid 期间有效。
            let result = unsafe { ffi::waitpid(-1, &mut status, ffi::WNOHANG) };
            if result > 0 {
                if let Some(slot) = self.children.iter_mut().find(|pid| **pid == result) {
                    *slot = -1;
                }
                continue;
            }
            if result < 0 && ffi::errno() == ffi::EINTR {
                continue;
            }
            return;
        }
    }

    /// 有空槽时 fork + exec 一个 terminal 并登记 pid；`command` 非空时作为
    /// argv[1] 传入（超长按 [`MAX_COMMAND`] 截断）。
    fn spawn(&mut self, command: &[u8]) {
        let Some(slot) = self.children.iter_mut().find(|pid| **pid < 0) else {
            return;
        };
        // SAFETY: fork 无前置条件。
        let parent = unsafe { ffi::getpid() };
        let pid = unsafe { ffi::fork() };
        if pid < 0 {
            return;
        }
        if pid == 0 {
            // 子进程：pdeathsig 保证桌面死亡时 terminal 不泄漏（竞态见
            // console-session：fork 前父进程已死则 getppid 不等于 parent）；
            // setsid 脱离桌面所在会话。
            unsafe {
                if ffi::prctl(ffi::PR_SET_PDEATHSIG, ffi::SIGKILL) < 0
                    || ffi::getppid() != parent
                    || ffi::setsid() < 0
                {
                    ffi::_exit(126);
                }
                let mut command_buffer = [0u8; MAX_COMMAND];
                let command_len = command.len().min(MAX_COMMAND - 1);
                command_buffer[..command_len].copy_from_slice(&command[..command_len]);
                // argv 恒为三元组：空命令时 argv[1] 直接置 NULL。
                let arguments = [
                    ffi::c_str(b"terminal\0"),
                    if command_len == 0 {
                        core::ptr::null()
                    } else {
                        command_buffer.as_ptr().cast()
                    },
                    core::ptr::null(),
                ];
                let environment = [
                    ffi::c_str(b"PATH=/sbin:/usr/sbin:/bin:/usr/bin\0"),
                    ffi::c_str(b"HOME=/root\0"),
                    core::ptr::null(),
                ];
                ffi::execve(
                    ffi::c_str(b"/bin/terminal\0"),
                    arguments.as_ptr(),
                    environment.as_ptr(),
                );
                ffi::_exit(127);
            }
        }
        *slot = pid;
    }
}
