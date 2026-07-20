//! terminal 子进程的拉起 / 收割 / respawn。
//!
//! listen socket 就绪后 `fork` + `execve("/bin/terminal")`；子进程死亡
//! （含用户点关闭按钮 → `CLOSE_REQUEST` → terminal 主动退出）经
//! `waitpid(WNOHANG)` 收割后立即 respawn，保证桌面上永远有一个终端。
//! spawn 失败（如 rootfs 尚未就绪）按 1s 间隔节流重试，避免 exec 风暴；
//! 事件循环在 `waiting()` 期间给 poll 加超时来驱动重试。

use crate::ffi;

/// respawn 节流间隔（毫秒）。
const RESPAWN_INTERVAL_MS: u64 = 1_000;

pub struct Supervisor {
    /// 存活的 terminal pid；-1 表示当前没有（等待 respawn）。
    child: i32,
    /// 下一次允许 spawn 的单调时间（毫秒）。
    next_spawn_at: u64,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            child: -1,
            next_spawn_at: 0,
        }
    }

    /// 是否处于“无 terminal、等待 respawn”状态（事件循环据此给 poll 加超时）。
    pub fn waiting(&self) -> bool {
        self.child < 0
    }

    /// 没有存活 terminal 且节流窗口已过时拉起一个。
    pub fn ensure_terminal(&mut self) {
        if self.child >= 0 {
            return;
        }
        let now = ffi::monotonic_milliseconds();
        if now < self.next_spawn_at {
            return;
        }
        self.next_spawn_at = now + RESPAWN_INTERVAL_MS;
        self.spawn();
    }

    /// `waitpid(-1, WNOHANG)` 收割所有已退出子进程；terminal 退出后标记
    /// 为待 respawn（下一轮 `ensure_terminal` 生效）。
    pub fn reap(&mut self) {
        loop {
            let mut status = 0;
            // SAFETY: status 在 waitpid 期间有效。
            let result = unsafe { ffi::waitpid(-1, &mut status, ffi::WNOHANG) };
            if result > 0 {
                if result == self.child {
                    self.child = -1;
                }
                continue;
            }
            if result < 0 && ffi::errno() == ffi::EINTR {
                continue;
            }
            return;
        }
    }

    fn spawn(&mut self) {
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
                let arguments = [ffi::c_str(b"terminal\0"), core::ptr::null()];
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
        self.child = pid;
    }
}
