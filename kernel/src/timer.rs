use riscv::register;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use spin::Mutex;

use crate::{arch::sbi, board, config, task::TaskControlBlock};

static mut TICK_INTERVAL_VALUE: u64 = 0;

const MSEC_PER_SEC: u64 = 1000;
const USEC_PER_SEC: u64 = 1000_000;
const NSEC_PER_SEC: u64 = 1000_000_000;

// 睡眠任务队列：以唤醒时间为键，任务为值
static SLEEPING_TASKS: Mutex<BTreeMap<u64, Arc<TaskControlBlock>>> = Mutex::new(BTreeMap::new());

pub fn get_time_msec() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::board_info().time_base_freq;
    current_mtime * MSEC_PER_SEC / time_base_freq
}

pub fn get_time_us() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::board_info().time_base_freq;
    current_mtime * USEC_PER_SEC / time_base_freq
}

pub fn get_time_ns() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::board_info().time_base_freq;
    current_mtime * NSEC_PER_SEC / time_base_freq
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

        // 唤醒任务
        for task in tasks_to_wakeup {
            crate::task::add_task(task);
        }
    }
}

// nanosleep 实现
pub fn nanosleep(nanoseconds: u64) -> isize {
    if nanoseconds == 0 {
        return 0;
    }

        debug!("1");
    // 对于非常短的睡眠，直接使用yield循环
    if nanoseconds < 1000000 { // 小于1毫秒
        let loops = nanoseconds / 10000; // 大约每10微秒yield一次
        for _ in 0..loops.max(1) {
            crate::task::suspend_current_and_run_next();
        }
        return 0;
    }

        debug!("2");
    if let Some(current_task) = crate::task::current_task() {
        let wake_time = get_time_ns() + nanoseconds;

        // 将当前任务加入睡眠队列
        add_sleeping_task(current_task, wake_time);

        debug!("3");
        // 让出CPU，等待被唤醒
        crate::task::suspend_current_and_run_next();
    }

    0
}

pub fn init() {
    let time_base_freq = board::board_info().time_base_freq;

    unsafe {
        TICK_INTERVAL_VALUE = time_base_freq / config::TICKS_PER_SEC as u64;
        register::sie::set_stimer();
    }

    set_next_timer_interrupt();
    debug!("timer initialized");
}
