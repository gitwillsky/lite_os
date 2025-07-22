use core::cmp::Ordering;

use alloc::{collections::binary_heap::BinaryHeap, sync::Arc};

use crate::task::{TaskControlBlock, scheduler::Scheduler};

pub struct CFScheduler {
    tasks: BinaryHeap<CFSTask>,
}

impl CFScheduler {
    pub fn new() -> Self {
        Self {
            tasks: BinaryHeap::new(),
        }
    }
}

impl Scheduler for CFScheduler {
    fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        self.tasks.push(CFSTask::new(task));
    }

    fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.tasks.pop().map(|cfs_task| cfs_task.0)
    }

    fn count(&self) -> usize {
        self.tasks.len()
    }

    fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        self.tasks
            .iter()
            .find(|t| t.0.pid() == pid)
            .map(|t| t.0.clone())
    }

    fn get_all_tasks(&self) -> alloc::vec::Vec<Arc<TaskControlBlock>> {
        self.tasks.iter().map(|cfs_task| cfs_task.0.clone()).collect()
    }
}

/// CFS调度器中的任务包装器，用于按vruntime排序
#[derive(Debug)]
struct CFSTask(Arc<TaskControlBlock>);

impl CFSTask {
    fn new(task: Arc<TaskControlBlock>) -> Self {
        Self(task)
    }
}

impl PartialEq for CFSTask {
    fn eq(&self, other: &Self) -> bool {
        self.0.sched.lock().vruntime == other.0.sched.lock().vruntime
    }
}

impl Eq for CFSTask {}

impl PartialOrd for CFSTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CFSTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // 最小堆：vruntime小的任务优先级高
        other
            .0
            .sched
            .lock()
            .vruntime
            .cmp(&self.0.sched.lock().vruntime)
    }
}
