use alloc::{collections::vec_deque::VecDeque, sync::Arc};
use spin::Mutex;

use crate::task::{scheduler::Scheduler, TaskControlBlock};

pub struct PriorityScheduler {
    /// 多级优先级队列 (0-39)
    priority_queues: [VecDeque<Arc<TaskControlBlock>>; 40],
}

impl PriorityScheduler {
    pub fn new() -> Self {
        Self {
            priority_queues: core::array::from_fn(|_| VecDeque::new()),
        }
    }
}

impl Scheduler for PriorityScheduler {
    fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        let priority = task.sched.lock().get_dynamic_priority() as usize;
        let priority = priority.min(39); // 确保不越界
        self.priority_queues[priority].push_back(task);
    }

    fn fetch_ready_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        // 从高优先级到低优先级查找任务
        for queue in self.priority_queues.iter_mut() {
            while let Some(task) = queue.pop_front() {
                if task.is_ready() {
                    return Some(task);
                }
            }
        }
        None
    }

    fn ready_task_count(&self) -> usize {
        self.priority_queues.iter().map(|queue| {
            queue.iter().filter(|t| t.is_ready()).count()
        }).sum()
    }

    fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        self.priority_queues.iter().find(|queue| {
            queue.iter().find(|t| t.pid() == pid).is_some()
        }).map(|queue| queue.iter().find(|t| t.pid() == pid).unwrap().clone())
    }
}
