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

    fn fetch_ready_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        if let Some(cfs_task) = self.tasks.pop() {
            if cfs_task.0.is_ready() {
                return Some(cfs_task.0);
            }
        }
        None
    }

    fn ready_task_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.0.is_ready()).count()
    }

    fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        self.tasks.iter().find(|t| t.0.pid() == pid).map(|t| t.0.clone())
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
