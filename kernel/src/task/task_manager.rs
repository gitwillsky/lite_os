use core::{cell::RefCell, num};

use alloc::vec::Vec;
use spin::{Mutex, Once};

use crate::{
    loader::{get_app_data, get_num_app},
    task::task::TaskControlBlock,
};

pub struct TaskManager {
    num_app: usize,

    inner: RefCell<TaskManagerInner>,
}

struct TaskManagerInner {
    tasks: Vec<TaskControlBlock>,
    current_task: usize,
}

pub static TASK_MANAGER: Once<Mutex<TaskManager>> = Once::new();

pub fn init() {
    TASK_MANAGER.call_once(|| {
        let num_app = get_num_app();
        println!("num_app = {}", num_app);
        let mut tasks: Vec<TaskControlBlock> = Vec::new();

        for i in 0..num_app {
            tasks.push(TaskControlBlock::new(get_app_data(i), i));
        }

        Mutex::new(TaskManager {
            num_app,
            inner: RefCell::new(TaskManagerInner {
                tasks,
                current_task: 0,
            }),
        })
    });
}

impl TaskManager {
    pub fn get_first_task_cx_ptr(&self) -> *const crate::task::context::TaskContext {
        let inner = self.inner.borrow();
        &inner.tasks[0].task_cx as *const _
    }

    pub fn tasks_mut(&self) -> core::cell::RefMut<'_, Vec<TaskControlBlock>> {
        core::cell::RefMut::map(self.inner.borrow_mut(), |inner| &mut inner.tasks)
    }

    pub fn current_task_mut(&self) -> core::cell::RefMut<'_, usize> {
        core::cell::RefMut::map(self.inner.borrow_mut(), |inner| &mut inner.current_task)
    }

    pub fn mark_current_exited(&self) {
        let mut inner = self.inner.borrow_mut();
        let current = inner.current_task;
        inner.tasks[current].task_status = crate::task::task::TaskStatus::Exited;
    }

    pub fn find_next_task(&self) -> Option<usize> {
        let inner = self.inner.borrow();
        let n = inner.tasks.len();
        let mut cur = inner.current_task;
        for _ in 0..n {
            cur = (cur + 1) % n;
            if inner.tasks[cur].task_status == crate::task::task::TaskStatus::Ready {
                return Some(cur);
            }
        }
        None
    }

    pub fn switch_to_next(&self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(next) = self.find_next_task() {
            let current = inner.current_task;
            let n = inner.tasks.len();
            let (first, second) = inner.tasks.split_at_mut(current.max(next));
            let (current_task, next_task) = if current < next {
                (&mut first[current], &mut second[0])
            } else {
                (&mut second[0], &mut first[next])
            };
            if current_task.task_status == crate::task::task::TaskStatus::Running {
                current_task.task_status = crate::task::task::TaskStatus::Ready;
            }
            next_task.task_status = crate::task::task::TaskStatus::Running;
            let current_cx_ptr = &mut current_task.task_cx as *mut _;
            let next_cx_ptr = &next_task.task_cx as *const _;
            inner.current_task = next;
            drop(inner);
            unsafe {
                crate::task::__switch(current_cx_ptr, next_cx_ptr);
            }
        } else {
            println!("[kernel] All user tasks exited, shutting down...");
            crate::arch::sbi::shutdown().ok();
            loop {}
        }
    }
}

pub fn run_first_task() -> ! {
    let task_cx_ptr = {
        let task_manager = TASK_MANAGER.wait().lock();
        let inner = task_manager.inner.borrow();
        let first_task = &inner.tasks[0];
        let ptr = &first_task.task_cx as *const _;
        ptr
    }; // 锁在这里释放

    // 为第一次任务切换创建一个临时的任务上下文
    let mut dummy_cx = crate::task::context::TaskContext::zero_init();
    let dummy_cx_ptr = &mut dummy_cx as *mut _;

    unsafe {
        crate::task::__switch(dummy_cx_ptr, task_cx_ptr);
    }
    panic!("run_first_task should never return");
}

pub fn current_user_token() -> usize {
    let task_manager = TASK_MANAGER.wait();
    let inner = task_manager.lock();
    let current = inner.inner.borrow().current_task;
    let token = inner.inner.borrow().tasks[current].get_user_token();
    token
}
