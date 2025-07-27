use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};

use crate::{sync::SpinLock, task::TaskControlBlock};

/// Sleep queue for managing sleeping tasks
pub struct SleepQueue {
    /// Tasks sleeping until a specific time
    pub sleeping_tasks: BTreeMap<u64, Vec<Arc<TaskControlBlock>>>,
}

impl SleepQueue {
    fn new() -> Self {
        Self {
            sleeping_tasks: BTreeMap::new(),
        }
    }

    /// Add a task to sleep until the specified time
    pub fn add_sleeping_task(&mut self, wake_time: u64, task: Arc<TaskControlBlock>) {
        debug!("Task {} sleeping until {}μs", task.pid(), wake_time);
        self.sleeping_tasks
            .entry(wake_time)
            .or_insert_with(Vec::new)
            .push(task);
    }

    /// Wake up tasks that should wake up before or at the specified time
    pub fn wake_tasks_before(&mut self, current_time: u64) -> Vec<Arc<TaskControlBlock>> {
        let mut woken_tasks = Vec::new();

        // Find all tasks that should wake up
        let wake_times: Vec<u64> = self
            .sleeping_tasks
            .range(..=current_time)
            .map(|(&time, _)| time)
            .collect();

        // Remove and collect all tasks that should wake up
        for wake_time in wake_times {
            if let Some(tasks) = self.sleeping_tasks.remove(&wake_time) {
                for task in tasks {
                    debug!("Waking up task {} at {}μs", task.pid(), current_time);
                    woken_tasks.push(task);
                }
            }
        }

        woken_tasks
    }

    /// Get the next wake time
    pub fn next_wake_time(&self) -> Option<u64> {
        self.sleeping_tasks.keys().next().copied()
    }

    /// Get the number of sleeping tasks
    pub fn len(&self) -> usize {
        self.sleeping_tasks.values().map(|v| v.len()).sum()
    }
}

/// Global sleep queue
pub static SLEEP_QUEUE: SpinLock<SleepQueue> = SpinLock::new(SleepQueue {
    sleeping_tasks: BTreeMap::new(),
});