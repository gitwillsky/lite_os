use crate::memory::page_table::{translated_byte_buffer, translated_ref_mut};
use crate::task::{current_user_token, current_task};
use crate::timer;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use spin::Mutex;
use lazy_static::lazy_static;

pub fn sys_get_time_msec() -> isize {
    timer::get_time_msec() as isize
}

pub fn sys_get_time_us() -> isize {
    timer::get_time_us() as isize
}

pub fn sys_get_time_ns() -> isize {
    timer::get_time_ns() as isize
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TimeSpec {
    pub tv_sec: u64,  // 秒
    pub tv_nsec: u64, // 纳秒
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TimeVal {
    pub tv_sec: u64,  // 秒
    pub tv_usec: u64, // 微秒
}

pub fn sys_nanosleep(req: *const TimeSpec, rem: *mut TimeSpec) -> isize {
    if req.is_null() {
        return -22; // EINVAL
    }

    // 安全地从用户空间读取TimeSpec结构
    let token = current_user_token();
    let req_buffers =
        translated_byte_buffer(token, req as *const u8, core::mem::size_of::<TimeSpec>());

    if req_buffers.is_empty() {
        return -14; // EFAULT: bad address
    }

    // 从缓冲区中读取TimeSpec
    let timespec = unsafe { *(req_buffers[0].as_ptr() as *const TimeSpec) };

    // 参数验证
    if timespec.tv_nsec >= 1000_000_000 {
        return -22; // EINVAL: invalid nanoseconds
    }

    // 转换为纳秒
    let total_nanoseconds = timespec.tv_sec * 1_000_000_000 + timespec.tv_nsec;

    if total_nanoseconds == 0 {
        return 0; // 无需睡眠
    }

    // 调用内核睡眠函数
    crate::task::nanosleep(total_nanoseconds)
}

// 获取 Unix 时间戳（秒）
pub fn sys_time() -> isize {
    timer::get_unix_timestamp() as isize
}

// 获取当前时间和时区信息 (POSIX gettimeofday)
pub fn sys_gettimeofday(tv: *mut TimeVal, tz: *mut u8) -> isize {
    if tv.is_null() {
        return -22; // EINVAL
    }

    // 获取真实的 Unix 时间戳
    let unix_timestamp_us = timer::get_unix_timestamp_us();
    let seconds = unix_timestamp_us / 1_000_000;
    let microseconds = unix_timestamp_us % 1_000_000;

    let timeval = TimeVal {
        tv_sec: seconds,
        tv_usec: microseconds,
    };

    // 安全地写入用户空间
    let token = current_user_token();
    let mut tv_buffers =
        translated_byte_buffer(token, tv as *const u8, core::mem::size_of::<TimeVal>());

    if tv_buffers.is_empty() {
        return -14; // EFAULT: bad address
    }

    // 写入 TimeVal 结构
    unsafe {
        core::ptr::copy_nonoverlapping(
            &timeval as *const TimeVal as *const u8,
            tv_buffers[0].as_mut_ptr(),
            core::mem::size_of::<TimeVal>(),
        );
    }

    // 忽略时区参数（在现代系统中通常为 null）
    // 如果需要可以在此处处理时区信息

    0 // 成功
}

// Clock IDs
const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_REALTIME_COARSE: i32 = 5;
const CLOCK_MONOTONIC_COARSE: i32 = 6;
const CLOCK_BOOTTIME: i32 = 7;

// Timer flags
const TIMER_ABSTIME: i32 = 1;

// Signal event structure
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SigEvent {
    pub sigev_notify: i32,      // Notification method
    pub sigev_signo: i32,       // Signal number
    pub sigev_value: usize,     // Signal value
    pub sigev_notify_function: usize,  // Function pointer (unused in kernel)
    pub sigev_notify_attributes: usize, // Thread attributes (unused)
}

// Timer specification
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ITimerSpec {
    pub it_interval: TimeSpec,  // Timer interval
    pub it_value: TimeSpec,     // Timer expiration
}

// Timer structure
struct Timer {
    id: i32,
    clockid: i32,
    owner_pid: usize,
    sigevent: SigEvent,
    interval: u64,  // nanoseconds
    next_expiry: u64,  // nanoseconds since epoch or boot
    armed: bool,
}

lazy_static! {
    static ref TIMER_MANAGER: Mutex<TimerManager> = Mutex::new(TimerManager::new());
}

struct TimerManager {
    timers: BTreeMap<i32, Arc<Mutex<Timer>>>,
    next_timer_id: i32,
}

impl TimerManager {
    fn new() -> Self {
        Self {
            timers: BTreeMap::new(),
            next_timer_id: 1,
        }
    }

    fn create_timer(&mut self, clockid: i32, sigevent: SigEvent, owner_pid: usize) -> i32 {
        let timer_id = self.next_timer_id;
        self.next_timer_id += 1;

        let timer = Arc::new(Mutex::new(Timer {
            id: timer_id,
            clockid,
            owner_pid,
            sigevent,
            interval: 0,
            next_expiry: 0,
            armed: false,
        }));

        self.timers.insert(timer_id, timer);
        timer_id
    }

    fn get_timer(&self, timer_id: i32) -> Option<Arc<Mutex<Timer>>> {
        self.timers.get(&timer_id).cloned()
    }

    fn delete_timer(&mut self, timer_id: i32) -> bool {
        self.timers.remove(&timer_id).is_some()
    }

    fn check_and_fire_timers(&mut self) {
        let current_time = timer::get_time_ns();
        let current_realtime = timer::get_unix_timestamp_us() * 1000;

        for (_, timer_arc) in self.timers.iter() {
            let mut timer = timer_arc.lock();

            if !timer.armed || timer.next_expiry == 0 {
                continue;
            }

            let current = match timer.clockid {
                CLOCK_REALTIME => current_realtime,
                CLOCK_MONOTONIC | CLOCK_BOOTTIME => current_time,
                _ => continue,
            };

            if current >= timer.next_expiry {
                // Timer expired - send signal
                if timer.sigevent.sigev_notify == 1 { // SIGEV_SIGNAL
                    use crate::signal::{send_signal, Signal};
                    if let Err(e) = send_signal(timer.owner_pid, Signal::from_u8(timer.sigevent.sigev_signo as u8).unwrap_or(Signal::SIGALRM)) {
                        warn!("Failed to send timer signal: {:?}", e);
                    }
                }

                // Handle periodic timer
                if timer.interval > 0 {
                    timer.next_expiry += timer.interval;
                    // Handle case where we missed multiple intervals
                    while timer.next_expiry <= current {
                        timer.next_expiry += timer.interval;
                    }
                } else {
                    // One-shot timer
                    timer.armed = false;
                }
            }
        }
    }
}

// Called from timer interrupt handler
pub fn check_timers() {
    TIMER_MANAGER.lock().check_and_fire_timers();
}

/// clock_gettime - 获取时钟时间
pub fn sys_clock_gettime(clockid: i32, tp: *mut TimeSpec) -> isize {
    if tp.is_null() {
        return -22; // EINVAL
    }

    let timespec = match clockid {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => {
            let unix_timestamp_ns = timer::get_unix_timestamp() * 1_000_000_000;
            TimeSpec {
                tv_sec: unix_timestamp_ns / 1_000_000_000,
                tv_nsec: unix_timestamp_ns % 1_000_000_000,
            }
        }
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            let monotonic_ns = timer::get_time_ns();
            TimeSpec {
                tv_sec: monotonic_ns / 1_000_000_000,
                tv_nsec: monotonic_ns % 1_000_000_000,
            }
        }
        CLOCK_PROCESS_CPUTIME_ID => {
            if let Some(task) = crate::task::current_task() {
                let cpu_time = task.user_cpu_time.load(core::sync::atomic::Ordering::Relaxed)
                    + task.kernel_cpu_time.load(core::sync::atomic::Ordering::Relaxed);
                let cpu_time_ns = cpu_time * 1000;
                TimeSpec {
                    tv_sec: cpu_time_ns / 1_000_000_000,
                    tv_nsec: cpu_time_ns % 1_000_000_000,
                }
            } else {
                return -1;
            }
        }
        CLOCK_THREAD_CPUTIME_ID => {
            if let Some(task) = crate::task::current_task() {
                let cpu_time = task.user_cpu_time.load(core::sync::atomic::Ordering::Relaxed)
                    + task.kernel_cpu_time.load(core::sync::atomic::Ordering::Relaxed);
                let cpu_time_ns = cpu_time * 1000;
                TimeSpec {
                    tv_sec: cpu_time_ns / 1_000_000_000,
                    tv_nsec: cpu_time_ns % 1_000_000_000,
                }
            } else {
                return -1;
            }
        }
        _ => return -22, // EINVAL
    };

    let token = current_user_token();
    let mut tp_buffers = translated_byte_buffer(token, tp as *const u8, core::mem::size_of::<TimeSpec>());

    if tp_buffers.is_empty() {
        return -14; // EFAULT
    }

    unsafe {
        core::ptr::copy_nonoverlapping(
            &timespec as *const TimeSpec as *const u8,
            tp_buffers[0].as_mut_ptr(),
            core::mem::size_of::<TimeSpec>(),
        );
    }

    0
}

/// clock_settime - 设置时钟时间
pub fn sys_clock_settime(clockid: i32, tp: *const TimeSpec) -> isize {
    if tp.is_null() {
        return -22; // EINVAL
    }

    if clockid != CLOCK_REALTIME {
        return -22; // EINVAL
    }

    if let Some(task) = crate::task::current_task() {
        if !task.is_root() {
            return -1; // EPERM
        }
    } else {
        return -1;
    }

    let token = current_user_token();
    let tp_buffers = translated_byte_buffer(token, tp as *const u8, core::mem::size_of::<TimeSpec>());

    if tp_buffers.is_empty() {
        return -14; // EFAULT
    }

    let timespec = unsafe { *(tp_buffers[0].as_ptr() as *const TimeSpec) };

    if timespec.tv_nsec >= 1_000_000_000 {
        return -22; // EINVAL
    }

    let new_timestamp_ns = timespec.tv_sec * 1_000_000_000 + timespec.tv_nsec;
    timer::set_unix_timestamp_ns(new_timestamp_ns);

    0
}

/// clock_getres - 获取时钟精度
pub fn sys_clock_getres(clockid: i32, res: *mut TimeSpec) -> isize {
    match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID |
        CLOCK_MONOTONIC_RAW | CLOCK_REALTIME_COARSE | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            if !res.is_null() {
                let resolution = match clockid {
                    CLOCK_REALTIME_COARSE | CLOCK_MONOTONIC_COARSE => {
                        TimeSpec {
                            tv_sec: 0,
                            tv_nsec: 10_000_000, // 10ms for coarse clocks
                        }
                    }
                    _ => {
                        TimeSpec {
                            tv_sec: 0,
                            tv_nsec: 1000, // 1 microsecond
                        }
                    }
                };

                let token = current_user_token();
                let mut res_buffers = translated_byte_buffer(
                    token,
                    res as *const u8,
                    core::mem::size_of::<TimeSpec>()
                );

                if res_buffers.is_empty() {
                    return -14; // EFAULT
                }

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        &resolution as *const TimeSpec as *const u8,
                        res_buffers[0].as_mut_ptr(),
                        core::mem::size_of::<TimeSpec>(),
                    );
                }
            }
            0
        }
        _ => -22, // EINVAL
    }
}

/// timer_create - 创建定时器
pub fn sys_timer_create(clockid: i32, sevp: *mut u8, timerid: *mut i32) -> isize {
    match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME => {},
        _ => return -22, // EINVAL
    }

    let sigevent = if sevp.is_null() {
        // Default: SIGALRM signal
        SigEvent {
            sigev_notify: 1, // SIGEV_SIGNAL
            sigev_signo: 14, // SIGALRM
            sigev_value: 0,
            sigev_notify_function: 0,
            sigev_notify_attributes: 0,
        }
    } else {
        let token = current_user_token();
        let sevp_buffers = translated_byte_buffer(token, sevp, core::mem::size_of::<SigEvent>());

        if sevp_buffers.is_empty() {
            return -14; // EFAULT
        }

        unsafe { *(sevp_buffers[0].as_ptr() as *const SigEvent) }
    };

    let owner_pid = if let Some(task) = crate::task::current_task() {
        task.pid()
    } else {
        return -1;
    };

    let timer_id = TIMER_MANAGER.lock().create_timer(clockid, sigevent, owner_pid);

    if !timerid.is_null() {
        let token = current_user_token();
        let mut tid_buffers = translated_byte_buffer(
            token,
            timerid as *const u8,
            core::mem::size_of::<i32>()
        );

        if tid_buffers.is_empty() {
            return -14; // EFAULT
        }

        unsafe {
            core::ptr::copy_nonoverlapping(
                &timer_id as *const i32 as *const u8,
                tid_buffers[0].as_mut_ptr(),
                core::mem::size_of::<i32>(),
            );
        }
    }

    0
}

/// timer_settime - 设置定时器
pub fn sys_timer_settime(timerid: i32, flags: i32, new_value: *const u8) -> isize {
    if new_value.is_null() {
        return -22; // EINVAL
    }

    let token = current_user_token();
    let new_buffers = translated_byte_buffer(token, new_value, core::mem::size_of::<ITimerSpec>());

    if new_buffers.is_empty() {
        return -14; // EFAULT
    }

    let new_spec = unsafe { *(new_buffers[0].as_ptr() as *const ITimerSpec) };

    // Validate timespec values
    if new_spec.it_value.tv_nsec >= 1_000_000_000 ||
       new_spec.it_interval.tv_nsec >= 1_000_000_000 {
        return -22; // EINVAL
    }

    let timer_manager = TIMER_MANAGER.lock();
    if let Some(timer_arc) = timer_manager.get_timer(timerid) {
        let mut timer = timer_arc.lock();

        // Disarm timer if it_value is zero
        if new_spec.it_value.tv_sec == 0 && new_spec.it_value.tv_nsec == 0 {
            timer.armed = false;
            timer.next_expiry = 0;
            return 0;
        }

        // Calculate expiry time
        let value_ns = new_spec.it_value.tv_sec * 1_000_000_000 + new_spec.it_value.tv_nsec;
        let interval_ns = new_spec.it_interval.tv_sec * 1_000_000_000 + new_spec.it_interval.tv_nsec;

        let base_time = match timer.clockid {
            CLOCK_REALTIME => timer::get_unix_timestamp_us() * 1000,
            _ => timer::get_time_ns(),
        };

        timer.next_expiry = if flags & TIMER_ABSTIME != 0 {
            value_ns
        } else {
            base_time + value_ns
        };

        timer.interval = interval_ns;
        timer.armed = true;

        0
    } else {
        -22 // EINVAL - invalid timer ID
    }
}

/// timer_delete - 删除定时器
pub fn sys_timer_delete(timerid: i32) -> isize {
    if TIMER_MANAGER.lock().delete_timer(timerid) {
        0
    } else {
        -22 // EINVAL
    }
}
