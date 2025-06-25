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
    pub fn run_first_task(&self) -> ! {
        let inner = self.inner.borrow();
        let first_task = &inner.tasks[0];
        let task_cx_ptr = &first_task.task_cx as *const _;
        drop(inner);
        unsafe {
            crate::task::__switch(core::ptr::null_mut(), task_cx_ptr);
        }
        panic!("run_first_task should never return");
    }

    pub fn tasks_mut(&self) -> core::cell::RefMut<'_, Vec<TaskControlBlock>> {
        core::cell::RefMut::map(self.inner.borrow_mut(), |inner| &mut inner.tasks)
    }

    pub fn current_task_mut(&self) -> core::cell::RefMut<'_, usize> {
        core::cell::RefMut::map(self.inner.borrow_mut(), |inner| &mut inner.current_task)
    }
}
