use alloc::vec::Vec;
use core::cell::RefCell;
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
    pub fn get_first_task_cx_ptr(&self) -> *const crate::task::context::TaskContext {
        let inner = self.inner.exclusive_access();
        &inner.tasks[0].task_cx as *const _
    }

    pub fn tasks_mut(&self) -> core::cell::RefMut<'_, Vec<TaskControlBlock>> {
        core::cell::RefMut::map(self.inner.exclusive_access(), |inner| &mut inner.tasks)
    }

    pub fn current_task_mut(&self) -> core::cell::RefMut<'_, TaskControlBlock> {
        core::cell::RefMut::map(self.inner.exclusive_access(), |inner| {
            &mut inner.tasks[inner.current_task]
        })
    }

    pub fn mark_current_exited(&self) {
        self.current_task_mut().task_status = TaskStatus::Exited;
    }

    pub fn mark_current_suspended(&self) {
        let mut current_task = self.current_task_mut();
        if current_task.task_status == TaskStatus::Running {
            current_task.task_status = TaskStatus::Ready;
        }
    }

    pub fn find_next_task(&self) -> Option<usize> {
        let inner = self.inner.exclusive_access();
        let n = inner.tasks.len();
        let mut cur = inner.current_task;
        for _ in 0..n {
            cur = (cur + 1) % n;
            if inner.tasks[cur].task_status == TaskStatus::Ready {
                return Some(cur);
            }
        }
        None
    }

    pub fn switch_to_next(&self) {
        let mut inner = self.inner.exclusive_access();
        if let Some(next) = self.find_next_task() {
            let current = inner.current_task;
            let (first, second) = inner.tasks.split_at_mut(current.max(next));
            let (current_task, next_task) = if current < next {
                (&mut first[current], &mut second[0])
            } else {
                (&mut second[0], &mut first[next])
            };
            if current_task.task_status == TaskStatus::Running {
                current_task.task_status = TaskStatus::Ready;
            }
            next_task.task_status = TaskStatus::Running;
            let current_cx_ptr = &mut current_task.task_cx as *mut _;
            let next_cx_ptr = &next_task.task_cx as *const _;
            inner.current_task = next;
            drop(inner);
            unsafe {
                crate::task::__switch(current_cx_ptr, next_cx_ptr);
            }
        } else {
            println!("[kernel] All user tasks exited, shutting down...");
            sbi::shutdown().ok();
            loop {}
        }
    }
}

pub fn run_first_task() -> ! {
    let first_task_ptr = {
        let task_manager = TASK_MANAGER.wait();
        task_manager.get_first_task_cx_ptr()
    };

    let mut zero_task_ptr = TaskContext::zero_init();
    unsafe {
        crate::task::__switch(&mut zero_task_ptr as *mut _, first_task_ptr);
    }
    unreachable!()
}

pub fn current_user_token() -> usize {
    TASK_MANAGER
        .wait()
        .current_task_mut()
        .get_user_token()
}

pub fn current_trap_cx() -> &'static mut TrapContext {
    TASK_MANAGER.wait().current_task_mut().get_trap_cx()
}

pub fn exit_current_and_run_next() {
    let task_manager = TASK_MANAGER.wait();
    task_manager.mark_current_exited();
    task_manager.switch_to_next();
}

pub fn suspend_current_and_run_next() {
    let task_manager = TASK_MANAGER.wait();
    task_manager.mark_current_suspended();
    task_manager.switch_to_next();
}
