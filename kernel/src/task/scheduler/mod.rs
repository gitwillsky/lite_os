use alloc::sync::Arc;

use crate::task::TaskControlBlock;

pub mod cfs_scheduler;
pub mod fifo_scheduler;
pub mod priority_scheduler;

pub trait Scheduler: Send {
    fn add_task(&mut self, task: Arc<TaskControlBlock>);
    fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>>;
    fn ready_task_count(&self) -> usize;
    fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>>;
}
