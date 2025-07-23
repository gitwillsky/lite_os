use core::sync::atomic::{AtomicU64, Ordering};

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use riscv::register;
use spin::Mutex;

use crate::{arch::sbi, board, config, task::TaskControlBlock};

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

// 睡眠任务队列：以唤醒时间为键，任务为值
static SLEEPING_TASKS: Mutex<BTreeMap<u64, Arc<TaskControlBlock>>> = Mutex::new(BTreeMap::new());

// 读取 Goldfish RTC 获取真实的 Unix 时间戳（纳秒）
fn read_goldfish_rtc_ns() -> Option<u64> {
    let board_info = board::board_info();
    debug!("Checking for RTC device...");
    if let Some(rtc) = board_info.rtc_device {
        debug!("Found RTC device at base address: {:#x}", rtc.base_addr);
        
        // 检查地址是否合理
        if rtc.base_addr == 0 {
            warn!("Invalid RTC base address: 0x0");
            return None;
        }
        
        // 暂时跳过实际读取，直接返回 None 来避免卡死
        // 这样可以让系统继续运行，使用默认时间
        warn!("Skipping RTC read to avoid potential hang, using default time");
        None
        
        // TODO: 以后可以在这里实现更安全的 RTC 读取方式
        // 可能需要先映射内存区域或者使用不同的访问方法
        /*
        unsafe {
            // 读取低32位，然后读取高32位
            let low = core::ptr::read_volatile((rtc.base_addr + RTC_TIME_LOW) as *const u32);
            let high = core::ptr::read_volatile((rtc.base_addr + RTC_TIME_HIGH) as *const u32);
            
            // 组合成64位纳秒时间戳
            Some(((high as u64) << 32) | (low as u64))
        }
        */
    } else {
        warn!("Goldfish RTC device not found in device tree");
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

// 将任务加入睡眠队列
pub fn add_sleeping_task(task: Arc<TaskControlBlock>, wake_time_ns: u64) {
    let mut sleeping_tasks = SLEEPING_TASKS.lock();

    // 避免时间冲突：如果已存在相同时间，则递增1纳秒
    let mut actual_wake_time = wake_time_ns;
    while sleeping_tasks.contains_key(&actual_wake_time) {
        actual_wake_time += 1;
    }

    sleeping_tasks.insert(actual_wake_time, task);
}

// 检查并唤醒到期的睡眠任务
pub fn check_and_wakeup_sleeping_tasks() {
    // 尝试获取锁，如果失败说明其他地方正在使用，直接返回避免死锁
    if let Some(mut sleeping_tasks) = SLEEPING_TASKS.try_lock() {
        let current_time = get_time_ns();

        // 收集需要唤醒的任务
        let mut tasks_to_wakeup = alloc::vec::Vec::new();
        let mut keys_to_remove = alloc::vec::Vec::new();

        for (&wake_time, task) in sleeping_tasks.iter() {
            if wake_time <= current_time {
                tasks_to_wakeup.push(task.clone());
                keys_to_remove.push(wake_time);
            } else {
                // BTreeMap是有序的，后面的都不会到期
                break;
            }
        }

        // 从睡眠队列中移除
        for key in keys_to_remove {
            sleeping_tasks.remove(&key);
        }

        // 释放锁后再唤醒任务
        drop(sleeping_tasks);

        for task in tasks_to_wakeup {
            // 将任务状态从Sleeping改为Ready
            *task.task_status.lock() = crate::task::TaskStatus::Ready;
            crate::task::add_task(task);
        }
    }
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

        // 设置任务状态为睡眠，防止被调度器重新加入就绪队列
        *current_task.task_status.lock() = crate::task::TaskStatus::Sleeping;

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

pub fn init() {
    let time_base_freq = board::board_info().time_base_freq;

    unsafe {
        TICK_INTERVAL_VALUE = time_base_freq / config::TICKS_PER_SEC as u64;
        register::sie::set_stimer();
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
    set_next_timer_interrupt();
    debug!("timer initialized with real-time clock");
}
