use alloc::vec::Vec;
use spin::Once;

use crate::{
    arch::sbi,
    loader::{get_app_data, get_num_app},
    sync::UPSafeCell,
    task::{
        context::TaskContext,
        task::{TaskControlBlock, TaskStatus},
    },
    trap::TrapContext,
};

pub struct TaskManager {
    num_app: usize,
    inner: UPSafeCell<TaskManagerInner>,
}

struct TaskManagerInner {
    tasks: Vec<TaskControlBlock>,
    current_task: usize,
}

impl TaskManagerInner {
    /// 查找下一个就绪的任务
    fn find_next_ready_task(&self) -> Option<usize> {
        let n = self.tasks.len();
        let mut cur = self.current_task;
        for _ in 0..n {
            cur = (cur + 1) % n;
            if self.tasks[cur].task_status == TaskStatus::Ready {
                return Some(cur);
            }
        }
        None
    }

    /// 标记当前任务为已退出
    fn mark_current_exited(&mut self) {
        self.tasks[self.current_task].task_status = TaskStatus::Exited;
    }

    /// 标记当前任务为挂起状态
    fn mark_current_suspended(&mut self) {
        let current_task = &mut self.tasks[self.current_task];
        if current_task.task_status == TaskStatus::Running {
            current_task.task_status = TaskStatus::Ready;
        }
    }

    /// 切换到下一个任务，返回 (当前任务上下文指针, 下一个任务上下文指针)
    fn switch_to_next_task(&mut self) -> Option<(*mut TaskContext, *const TaskContext)> {
        if let Some(next_task_id) = self.find_next_ready_task() {
            let current_task_id = self.current_task;

            // 安全地获取两个任务的可变引用
            let (first, second) = self.tasks.split_at_mut(current_task_id.max(next_task_id));
            let (current_task, next_task) = if current_task_id < next_task_id {
                (&mut first[current_task_id], &mut second[0])
            } else {
                (&mut second[0], &mut first[next_task_id])
            };

            // 更新任务状态
            if current_task.task_status == TaskStatus::Running {
                current_task.task_status = TaskStatus::Ready;
            }
            next_task.task_status = TaskStatus::Running;

            // 更新当前任务ID
            self.current_task = next_task_id;

            // 返回上下文指针
            Some((
                &mut current_task.task_cx as *mut _,
                &next_task.task_cx as *const _,
            ))
        } else {
            None
        }
    }
}

pub static TASK_MANAGER: Once<TaskManager> = Once::new();

pub fn init() {
    TASK_MANAGER.call_once(|| {
        let num_app = get_num_app();
        println!("num_app = {}", num_app);
        let mut tasks: Vec<TaskControlBlock> = Vec::new();

        for i in 0..num_app {
            tasks.push(TaskControlBlock::new(get_app_data(i), i));
        }

        TaskManager {
            num_app,
            inner: UPSafeCell::new(TaskManagerInner {
                tasks,
                current_task: 0,
            }),
        }
    });
}

impl TaskManager {
    /// 获取第一个任务的上下文指针
    pub fn get_first_task_cx_ptr(&self) -> *const TaskContext {
        let inner = self.inner.exclusive_access();
        &inner.tasks[0].task_cx as *const _
    }

    /// 执行操作并自动管理borrowing
    fn with_current_task<T>(&self, f: impl FnOnce(&mut TaskControlBlock) -> T) -> T {
        let mut inner = self.inner.exclusive_access();
        let current_task_id = inner.current_task;
        f(&mut inner.tasks[current_task_id])
    }

    /// 获取当前任务的用户令牌
    pub fn current_user_token(&self) -> usize {
        self.with_current_task(|task| task.get_user_token())
    }

    /// 获取当前任务的TrapContext
    pub fn current_trap_cx(&self) -> &'static mut TrapContext {
        self.with_current_task(|task| task.get_trap_cx())
    }

    /// 退出当前任务并运行下一个
    pub fn exit_current_and_run_next(&self) {
        let switch_info = {
            let mut inner = self.inner.exclusive_access();
            inner.mark_current_exited();
            inner.switch_to_next_task()
        };

        match switch_info {
            Some((current_cx_ptr, next_cx_ptr)) => {
                unsafe {
                    crate::task::__switch(current_cx_ptr, next_cx_ptr);
                }
            }
            None => {
                println!("[kernel] All user tasks exited, shutting down...");
                sbi::shutdown().ok();
                loop {}
            }
        }
    }

    /// 挂起当前任务并运行下一个
    pub fn suspend_current_and_run_next(&self) {
        let switch_info = {
            let mut inner = self.inner.exclusive_access();
            inner.mark_current_suspended();
            inner.switch_to_next_task()
        };

        match switch_info {
            Some((current_cx_ptr, next_cx_ptr)) => {
                unsafe {
                    crate::task::__switch(current_cx_ptr, next_cx_ptr);
                }
            }
            None => {
                println!("[kernel] All user tasks exited, shutting down...");
                sbi::shutdown().ok();
                loop {}
            }
        }
    }
}

// 全局接口函数
pub fn run_first_task() -> ! {
    println!("[run_first_task] Starting first task");
    let first_task_ptr = {
        let task_manager = TASK_MANAGER.wait();
        task_manager.get_first_task_cx_ptr()
    };
    println!("[run_first_task] Got first task context ptr: {:#x}", first_task_ptr as usize);

    let mut zero_task_ptr = TaskContext::zero_init();
    println!("[run_first_task] Ready to switch to first task");
    unsafe {
        crate::task::__switch(&mut zero_task_ptr as *mut _, first_task_ptr);
    }
    println!("[run_first_task] Task switch completed - this should not be reached");
    unreachable!()
}

pub fn current_user_token() -> usize {
    TASK_MANAGER.wait().current_user_token()
}

pub fn current_trap_cx() -> &'static mut TrapContext {
    TASK_MANAGER.wait().current_trap_cx()
}

pub fn exit_current_and_run_next() {
    TASK_MANAGER.wait().exit_current_and_run_next()
}

pub fn suspend_current_and_run_next() {
    TASK_MANAGER.wait().suspend_current_and_run_next()
}
