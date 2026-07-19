#[path = "../../../kernel/src/sync/wait_completion.rs"]
mod wait_completion;
pub(crate) use wait_completion::WaitCompletion;

#[path = "../../../kernel/src/sync/task_mutex.rs"]
mod task_mutex;
pub(crate) use task_mutex::{TaskMutex, TaskMutexGuard};
