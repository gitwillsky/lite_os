use core::sync::atomic::{AtomicU64, Ordering};

use riscv::register;
use spin::Mutex;

use crate::{arch::sbi, board, config, drivers::GoldfishRTC};

static mut TICK_INTERVAL_VALUE: u64 = 0;

const MSEC_PER_SEC: u64 = 1000;
const USEC_PER_SEC: u64 = 1000_000;
const NSEC_PER_SEC: u64 = 1000_000_000;

// Unix 纪元时间常量 (1970-01-01 00:00:00 UTC)
const UNIX_EPOCH_SECONDS: u64 = 0;

// Goldfish RTC 寄存器偏移
const RTC_TIME_LOW: usize = 0x00; // 纳秒时间低32位
const RTC_TIME_HIGH: usize = 0x04; // 纳秒时间高32位

// 系统启动时的时间偏移，从 Goldfish RTC 获取真实时间
static BOOT_TIME_UNIX_SECONDS: AtomicU64 = AtomicU64::new(0);

// 睡眠任务队列已迁移到 task_manager 模块进行统一管理

// 全局 RTC 设备实例
static RTC_DEVICE: Mutex<Option<GoldfishRTC>> = Mutex::new(None);

// 初始化 RTC 设备
fn init_rtc_device() -> Option<GoldfishRTC> {
    let board_info = board::board_info();
    debug!("Checking for RTC device...");

    if let Some(rtc_info) = board_info.rtc_device {
        debug!(
            "Found RTC device at base address: {:#x}, size: {:#x}",
            rtc_info.base_addr, rtc_info.size
        );

        // 检查地址是否合理
        if rtc_info.base_addr == 0 {
            warn!("Invalid RTC base address: 0x0");
            return None;
        }

        // 使用 MmioBus 创建 RTC 设备
        match GoldfishRTC::new(rtc_info) {
            Ok(rtc) => {
                debug!("Successfully initialized Goldfish RTC device");
                Some(rtc)
            }
            Err(err) => {
                warn!("Failed to initialize Goldfish RTC: {:?}", err);
                None
            }
        }
    } else {
        warn!("Goldfish RTC device not found in device tree");
        None
    }
}

// 读取 Goldfish RTC 获取真实的 Unix 时间戳（纳秒）
fn read_goldfish_rtc_ns() -> Option<u64> {
    let rtc_guard = RTC_DEVICE.lock();
    if let Some(rtc) = rtc_guard.as_ref() {
        match rtc.read_time_ns() {
            Ok(time_ns) => {
                debug!("Successfully read RTC time: {} ns", time_ns);
                Some(time_ns)
            }
            Err(err) => {
                warn!("Failed to read RTC time: {:?}", err);
                None
            }
        }
    } else {
        debug!("RTC device not initialized");
        None
    }
}

// 获取真实的 Unix 时间戳（秒）
pub fn get_unix_timestamp() -> u64 {
    let boot_time = BOOT_TIME_UNIX_SECONDS.load(Ordering::Relaxed);
    if boot_time == 0 {
        // 如果还没有初始化，直接从 RTC 读取当前时间
        if let Some(rtc_ns) = read_goldfish_rtc_ns() {
            rtc_ns / NSEC_PER_SEC
        } else {
            // RTC 不可用，返回基于系统运行时间的估计值
            warn!("RTC not available, using boot time estimate");
            1704067200 + (get_time_ns() / NSEC_PER_SEC) // 2024-01-01 + uptime
        }
    } else {
        // 使用启动时间偏移 + 系统运行时间
        boot_time + (get_time_ns() / NSEC_PER_SEC)
    }
}

// 获取真实的 Unix 时间戳（微秒）
pub fn get_unix_timestamp_us() -> u64 {
    let boot_time = BOOT_TIME_UNIX_SECONDS.load(Ordering::Relaxed);
    if boot_time == 0 {
        if let Some(rtc_ns) = read_goldfish_rtc_ns() {
            rtc_ns / 1000
        } else {
            // RTC 不可用，返回基于系统运行时间的估计值
            1704067200 * USEC_PER_SEC + get_time_us()
        }
    } else {
        boot_time * USEC_PER_SEC + get_time_us()
    }
}

pub fn get_time_msec() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::board_info().time_base_freq;
    // 避免溢出：先除后乘
    (current_mtime / time_base_freq) * MSEC_PER_SEC
}

pub fn get_time_us() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::board_info().time_base_freq;
    // 使用128位运算避免溢出
    ((current_mtime as u128 * USEC_PER_SEC as u128) / time_base_freq as u128) as u64
}

pub fn get_time_ns() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::board_info().time_base_freq;
    // 使用128位运算避免溢出
    ((current_mtime as u128 * NSEC_PER_SEC as u128) / time_base_freq as u128) as u64
}

#[inline(always)]
pub fn set_next_timer_interrupt() {
    let current_mtime = register::time::read64();
    let next_mtime = current_mtime + unsafe { TICK_INTERVAL_VALUE };

    let _ = sbi::set_timer(next_mtime as usize);
}

// 将任务加入睡眠队列 - 使用统一任务管理器
pub fn add_sleeping_task(task: alloc::sync::Arc<crate::task::TaskControlBlock>, wake_time_ns: u64) {
    crate::task::add_sleeping_task(task, wake_time_ns);
}

pub fn get_sleeping_tasks() -> alloc::vec::Vec<alloc::sync::Arc<crate::task::TaskControlBlock>> {
    crate::task::get_sleeping_tasks()
}

// 检查并唤醒到期的睡眠任务 - 使用统一任务管理器
pub fn check_and_wakeup_sleeping_tasks() {
    let current_time = get_time_ns();
    // 统一任务管理器会处理状态更新和重新加入调度器的逻辑
    crate::task::check_and_wakeup_sleeping_tasks(current_time);
}

// nanosleep 实现
pub fn nanosleep(nanoseconds: u64) -> isize {
    if nanoseconds == 0 {
        return 0;
    }

    let start_time = get_time_ns();

    // 无论时间长短，都使用睡眠队列来保证准确性
    if let Some(current_task) = crate::task::current_task() {
        let wake_time = start_time + nanoseconds;

        // 使用统一的任务状态更新方法
        crate::task::set_task_status(&current_task, crate::task::TaskStatus::Sleeping);

        // 将当前任务加入睡眠队列
        add_sleeping_task(current_task, wake_time);

        // 让出CPU，等待被唤醒（此时任务状态为Sleeping，不会被重新加入就绪队列）
        crate::task::block_current_and_run_next();

        // 醒来后检查实际时间
        let end_time = get_time_ns();
        let actual_sleep = end_time - start_time;
    } else {
        // 如果没有当前任务，使用忙等待（不推荐，但作为备用方案）
        let start_time = get_time_ns();
        while get_time_ns() - start_time < nanoseconds {
            // 忙等待
        }
    }

    0
}

pub fn enable_timer_interrupt() {
    let time_base_freq = board::board_info().time_base_freq;

    unsafe {
        TICK_INTERVAL_VALUE = time_base_freq / config::TICKS_PER_SEC as u64;
        register::sie::set_stimer();
    }

    set_next_timer_interrupt();
}

pub fn init_rtc() {
    // 初始化 RTC 设备
    if let Some(rtc) = init_rtc_device() {
        *RTC_DEVICE.lock() = Some(rtc);
        debug!("RTC device initialized successfully");
    } else {
        warn!("Failed to initialize RTC device");
    }

    // 从 Goldfish RTC 获取真实的启动时间
    if let Some(current_unix_ns) = read_goldfish_rtc_ns() {
        let boot_time = current_unix_ns / NSEC_PER_SEC;
        BOOT_TIME_UNIX_SECONDS.store(boot_time, Ordering::Relaxed);
        debug!(
            "Boot time set to Unix timestamp: {} (from Goldfish RTC)",
            boot_time
        );
    } else {
        warn!("Goldfish RTC not available, using default boot time");
        BOOT_TIME_UNIX_SECONDS.store(1704067200, Ordering::Relaxed);
    }
    debug!("timer initialized with real-time clock");
}
