use alloc::{collections::vec_deque::VecDeque, sync::Arc};
use lazy_static::lazy_static;

use crate::{sync::UPSafeCell, task::task::TaskControlBlock};

struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
    pub init_proc: Option<Arc<TaskControlBlock>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
            init_proc: None,
        }
    }

    pub fn set_init_proc(&mut self, init_proc: Arc<TaskControlBlock>) {
        self.add_task(init_proc.clone());
        self.init_proc = Some(init_proc);
    }

    /// 将任务添加到就绪队列队尾
    pub fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }

    pub fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.ready_queue.pop_front()
    }
}

lazy_static! {
    static ref TASK_MANAGER: UPSafeCell<TaskManager> = UPSafeCell::new(TaskManager::new());
}

pub fn add_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().add_task(task);
}

pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().fetch_task()
}

pub fn set_init_proc(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().set_init_proc(task);
}

pub fn get_init_proc() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER
        .exclusive_access()
        .init_proc
        .as_ref()
        .map(|f| f.clone())
}
