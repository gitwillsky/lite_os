use alloc::{collections::vec_deque::VecDeque, sync::Arc};
use spin::Mutex;

use crate::task::{TaskControlBlock, scheduler::Scheduler};

pub struct FIFOScheduler {
    tasks: VecDeque<Arc<TaskControlBlock>>,
}

impl FIFOScheduler {
    pub fn new() -> Self {
        Self {
            tasks: VecDeque::new(),
        }
    }
}

impl Scheduler for FIFOScheduler {
    fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        self.tasks.push_back(task);
    }

    fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.tasks.pop_front().map(|task| task)
    }

    fn ready_task_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.is_ready()).count()
    }

    fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        self.tasks
            .iter()
            .find(|t| t.pid() == pid)
            .map(|t| t.clone())
    }
}
