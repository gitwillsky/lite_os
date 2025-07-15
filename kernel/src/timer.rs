use riscv::register;
use alloc::{vec::Vec, boxed::Box, collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};
use crate::{arch::sbi, board, config};
use lazy_static::lazy_static;

static mut TICK_INTERVAL_VALUE: u64 = 0;

const MSEC_PER_SEC: u64 = 1000;
const USEC_PER_SEC: u64 = 1000_000;

/// 定时器任务ID类型
type TimerTaskId = usize;

/// 定时器任务回调函数类型
type TimerCallback = Box<dyn FnOnce() + Send + 'static>;

/// 重复定时器回调函数类型
type RecurringCallback = Box<dyn Fn() + Send + Sync + 'static>;

/// 定时器任务
struct TimerTask {
    id: TimerTaskId,
    trigger_time: u64, // 微秒时间戳
    callback: Option<TimerCallback>,
    recurring: bool,
    interval: Option<u64>, // 重复间隔（微秒）
    recurring_callback: Option<Arc<RecurringCallback>>, // 重复任务的回调
}

/// 定时器任务管理器
struct TimerTaskManager {
    tasks: spin::Mutex<BTreeMap<TimerTaskId, TimerTask>>,
    next_id: AtomicUsize,
}

impl TimerTaskManager {
    fn new() -> Self {
        Self {
            tasks: spin::Mutex::new(BTreeMap::new()),
            next_id: AtomicUsize::new(1),
        }
    }

    /// 添加一次性定时器任务
    fn add_timer_task<F>(&self, trigger_time_us: u64, callback: F) -> TimerTaskId
    where
        F: FnOnce() + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let task = TimerTask {
            id,
            trigger_time: trigger_time_us,
            callback: Some(Box::new(callback)),
            recurring: false,
            interval: None,
            recurring_callback: None,
        };

        let mut tasks = self.tasks.lock();
        tasks.insert(id, task);
        drop(tasks);

        debug!("Added timer task {} to trigger at {}", id, trigger_time_us);
        id
    }

    /// 添加重复定时器任务
    fn add_recurring_timer_task<F>(&self, trigger_time_us: u64, interval_us: u64, callback: F) -> TimerTaskId
    where
        F: Fn() + Send + Sync + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let recurring_callback = Arc::new(Box::new(callback) as RecurringCallback);
        
        // 为第一次执行创建一个一次性回调
        let first_callback = {
            let cb = recurring_callback.clone();
            Box::new(move || cb()) as TimerCallback
        };

        let task = TimerTask {
            id,
            trigger_time: trigger_time_us,
            callback: Some(first_callback),
            recurring: true,
            interval: Some(interval_us),
            recurring_callback: Some(recurring_callback),
        };

        let mut tasks = self.tasks.lock();
        tasks.insert(id, task);
        drop(tasks);

        debug!("Added recurring timer task {} to trigger at {} with interval {}", 
               id, trigger_time_us, interval_us);
        id
    }

    /// 取消定时器任务
    fn cancel_timer_task(&self, task_id: TimerTaskId) -> bool {
        let mut tasks = self.tasks.lock();
        let removed = tasks.remove(&task_id).is_some();
        if removed {
            debug!("Cancelled timer task {}", task_id);
        }
        removed
    }

    /// 处理到期的定时器任务
    fn handle_expired_tasks(&self) {
        let current_time = get_time_us();
        let mut expired_tasks = Vec::new();
        let mut recurring_tasks = Vec::new();

        // 查找到期的任务
        {
            let mut tasks = self.tasks.lock();
            let mut to_remove = Vec::new();

            for (&task_id, task) in tasks.iter_mut() {
                if current_time >= task.trigger_time {
                    if let Some(callback) = task.callback.take() {
                        expired_tasks.push((task_id, callback));

                        if task.recurring && task.interval.is_some() {
                            // 重复任务，保存信息以便重新添加
                            if let Some(recurring_cb) = &task.recurring_callback {
                                let next_trigger = current_time + task.interval.unwrap();
                                recurring_tasks.push((
                                    task_id, 
                                    next_trigger, 
                                    task.interval.unwrap(),
                                    recurring_cb.clone()
                                ));
                            }
                        }

                        to_remove.push(task_id);
                    }
                }
            }

            // 移除已执行的任务
            for task_id in to_remove {
                tasks.remove(&task_id);
            }
        }

        // 执行到期的任务回调
        for (task_id, callback) in expired_tasks {
            debug!("Executing timer task {}", task_id);
            callback();
        }

        // 重新添加重复任务
        for (original_task_id, next_trigger, interval, recurring_callback) in recurring_tasks {
            debug!("Rescheduling recurring timer task {} for {}", original_task_id, next_trigger);
            
            // 创建新的任务实例
            let new_id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let new_callback = {
                let cb = recurring_callback.clone();
                Box::new(move || cb()) as TimerCallback
            };

            let new_task = TimerTask {
                id: new_id,
                trigger_time: next_trigger,
                callback: Some(new_callback),
                recurring: true,
                interval: Some(interval),
                recurring_callback: Some(recurring_callback),
            };

            // 添加新的任务实例
            let mut tasks = self.tasks.lock();
            tasks.insert(new_id, new_task);
            drop(tasks);

            debug!("Added new recurring timer task {} (replacing {})", new_id, original_task_id);
        }
    }

    /// 获取下一个定时器任务的触发时间
    fn next_timer_time(&self) -> Option<u64> {
        let tasks = self.tasks.lock();
        tasks.values().map(|task| task.trigger_time).min()
    }
}

lazy_static! {
    static ref TIMER_TASK_MANAGER: TimerTaskManager = TimerTaskManager::new();
}

/// 公共接口：添加定时器任务
pub fn add_timer_task<F>(trigger_time_us: u64, callback: F) -> TimerTaskId
where
    F: FnOnce() + Send + 'static,
{
    TIMER_TASK_MANAGER.add_timer_task(trigger_time_us, callback)
}

/// 公共接口：添加重复定时器任务
pub fn add_recurring_timer_task<F>(trigger_time_us: u64, interval_us: u64, callback: F) -> TimerTaskId
where
    F: Fn() + Send + Sync + 'static,
{
    TIMER_TASK_MANAGER.add_recurring_timer_task(trigger_time_us, interval_us, callback)
}

/// 公共接口：取消定时器任务
pub fn cancel_timer_task(task_id: TimerTaskId) -> bool {
    TIMER_TASK_MANAGER.cancel_timer_task(task_id)
}

/// 公共接口：处理到期的定时器任务（在时钟中断中调用）
pub fn handle_timer_tasks() {
    TIMER_TASK_MANAGER.handle_expired_tasks();
}
pub fn get_time_msec() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::get_board_info().time_base_freq;
    current_mtime / time_base_freq / MSEC_PER_SEC
}

pub fn get_time_us() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::get_board_info().time_base_freq;
    current_mtime * USEC_PER_SEC / time_base_freq
}

#[inline(always)]
pub fn set_next_timer_interrupt() {
    let current_mtime = register::time::read64();
    let next_mtime = current_mtime + unsafe { TICK_INTERVAL_VALUE };

    let _ = sbi::set_timer(next_mtime as usize);
}

pub fn init() {
    let time_base_freq = board::get_board_info().time_base_freq;

    unsafe {
        TICK_INTERVAL_VALUE = time_base_freq / config::TICKS_PER_SEC as u64;
        register::sie::set_stimer();
    }

    set_next_timer_interrupt();
    debug!("timer initialized");
}
