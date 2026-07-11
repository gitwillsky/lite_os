mod dynamic_linking;
mod errno;
mod fs;
mod futex;
pub mod graphics;
mod memory;
mod process;
mod signal;
mod timer;
mod watchdog;

use crate::syscall::{
    fs::*, graphics::*, memory::*, process::*, signal::*, timer::*, watchdog::*,
};
use syscall_abi::*;

pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    match syscall_id {
        SYSCALL_GETCWD => sys_get_cwd(args[0] as *mut u8, args[1]),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP2 => sys_dup2(args[0], args[1]),
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1] as i32, args[2]),
        SYSCALL_PAUSE => sys_pause(),
        SYSCALL_ALARM => sys_alarm(args[0] as u32),
        SYSCALL_SIGNAL => sys_signal(args[0] as u32, args[1]),
        SYSCALL_OPEN => sys_open(args[0] as *const u8, args[1] as u32),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_PIPE => sys_pipe(args[0] as *mut i32),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as isize, args[2]),
        SYSCALL_READ => sys_read(args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_STAT => sys_stat(args[0] as *const u8, args[1] as *mut u8),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_NANOSLEEP => {
            sys_nanosleep(args[0] as *const timer::TimeSpec, args[1] as *mut timer::TimeSpec)
        }
        SYSCALL_GETUID => sys_getuid(),
        SYSCALL_GETGID => sys_getgid(),
        SYSCALL_GETEUID => sys_geteuid(),
        SYSCALL_GETEGID => sys_getegid(),
        SYSCALL_SHUTDOWN => sys_shutdown(),
        SYSCALL_YIELD => sys_sched_yield(),
        SYSCALL_KILL => sys_kill(args[0], args[1] as u32),
        SYSCALL_SIGACTION => sys_sigaction(
            args[0] as u32,
            args[1] as *const signal::SigAction,
            args[2] as *mut signal::SigAction,
        ),
        SYSCALL_SIGPROCMASK => {
            sys_sigprocmask(args[0] as i32, args[1] as *const u64, args[2] as *mut u64)
        }
        SYSCALL_SIGRETURN => sys_sigreturn(),
        SYSCALL_FLOCK => sys_flock(args[0], args[1] as i32),
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_SETGID => sys_setgid(args[0] as u32),
        SYSCALL_SETEUID => sys_seteuid(args[0] as u32),
        SYSCALL_SETEGID => sys_setegid(args[0] as u32),
        SYSCALL_GETPID => sys_get_pid(),
        SYSCALL_GETTID => sys_get_tid(),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_SBRK => sys_sbrk(args[0] as isize),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_FORK => sys_fork(),
        SYSCALL_EXEC => sys_execve(
            args[0] as *const u8,        // path
            args[1] as *const *const u8, // argv
            args[2] as *const *const u8, // envp
        ),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2] as i32, 0, -1, 0),
        SYSCALL_WAIT => sys_wait_pid(args[0] as isize, args[1] as *mut i32),

        SYSCALL_GUI_CREATE_CONTEXT => sys_gui_create_context(),
        SYSCALL_GUI_GET_SCREEN_INFO => {
            sys_gui_get_screen_info(args[0] as *mut graphics::GuiScreenInfo)
        }
        SYSCALL_GUI_FLUSH_RECTS => {
            sys_gui_flush_rects(args[0] as *const crate::drivers::framebuffer::Rect, args[1])
        }
        SYSCALL_GUI_MAP_FRAMEBUFFER => sys_gui_map_framebuffer(args[0] as *mut usize),

        SYSCALL_LISTDIR => sys_listdir(args[0] as *const u8, args[1] as *mut u8, args[2]),
        SYSCALL_MKDIR => sys_mkdir(args[0] as *const u8),
        SYSCALL_REMOVE => sys_remove(args[0] as *const u8),
        SYSCALL_READ_FILE => sys_read_file(args[0] as *const u8, args[1] as *mut u8, args[2]),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_MKFIFO => sys_mkfifo(args[0] as *const u8, args[1] as u32),
        SYSCALL_CHMOD => sys_chmod(args[0] as *const u8, args[1] as u32),
        SYSCALL_CHOWN => {
            sys_chown(args[0] as *const u8, args[1] as u32, args[2] as u32)
        }
        SYSCALL_GET_ARGS => sys_get_args(args[0] as *mut usize, args[1] as *mut u8, args[2]),

        SYSCALL_GET_PROCESS_LIST => sys_get_process_list(args[0] as *mut u32, args[1]),
        SYSCALL_GET_PROCESS_INFO => {
            sys_get_process_info(args[0] as u32, args[1] as *mut process::ProcessInfo)
        }
        SYSCALL_GET_SYSTEM_STATS => {
            sys_get_system_stats(args[0] as *mut process::SystemStats)
        }
        SYSCALL_GET_CPU_CORE_INFO => {
            sys_get_cpu_core_info(args[0] as *mut process::CpuCoreInfo)
        }
        SYSCALL_GET_TIME_MS => sys_get_time_msec(),
        SYSCALL_GET_TIME_US => sys_get_time_us(),
        SYSCALL_GET_TIME_NS => sys_get_time_ns(),
        SYSCALL_TIME => sys_time(),
        SYSCALL_GETTIMEOFDAY => {
            sys_gettimeofday(args[0] as *mut timer::TimeVal, args[1] as *mut u8)
        }
        SYSCALL_WATCHDOG_CONFIGURE => {
            sys_watchdog_configure(args[0] as *const crate::watchdog::WatchdogConfig)
        }
        SYSCALL_WATCHDOG_START => sys_watchdog_start(),
        SYSCALL_WATCHDOG_STOP => sys_watchdog_stop(),
        SYSCALL_WATCHDOG_FEED => sys_watchdog_feed(),
        SYSCALL_WATCHDOG_GET_INFO => {
            sys_watchdog_get_info(args[0] as *mut crate::watchdog::WatchdogInfo)
        }
        SYSCALL_WATCHDOG_SET_PRESET => sys_watchdog_set_preset(args[0] as u32),
        SYSCALL_THREAD_CREATE => sys_thread_create(args[0], args[1], args[2]),
        SYSCALL_THREAD_EXIT => sys_thread_exit(args[0] as i32),
        SYSCALL_THREAD_JOIN => sys_thread_join(args[0], args[1] as *mut i32),
        SYSCALL_SHM_CREATE => sys_shm_create(args[0]),
        SYSCALL_SHM_MAP => sys_shm_map(args[0], args[1] as i32),
        SYSCALL_SHM_CLOSE => sys_shm_close(args[0]),
        SYSCALL_POLL => sys_poll(args[0] as *mut u8, args[1], args[2] as isize),
        SYSCALL_UDS_LISTEN => sys_uds_listen(args[0] as *const u8, args[1]),
        SYSCALL_UDS_ACCEPT => sys_uds_accept(args[0] as *const u8),
        SYSCALL_UDS_CONNECT => sys_uds_connect(args[0] as *const u8),

        _ => {
            error!("syscall: invalid syscall_id: {}", syscall_id);
            -1
        }
    }
}
