mod sem_undo;
mod semaphore;
pub(crate) use sem_undo::{SemaphoreUndoHandle, SemaphoreUndoList};
pub(crate) use semaphore::{SemaphoreChange, SemaphoreOperation, SemaphoreSet, SemaphoreSnapshot};
mod ids;
mod shared_memory;
mod shared_memory_creator;
pub(crate) use shared_memory::{
    SharedMemoryAttachReservation, SharedMemorySegment, SharedMemorySnapshot,
};
pub(crate) use shared_memory_creator::SharedMemoryCreatorList;
mod message_queue;
mod namespace;
mod permissions;
pub(crate) use message_queue::{
    MessageQueue, MessageQueueSnapshot, MessageQueueWait, MessageReceive, PreparedMessage,
};
pub(crate) use namespace::{
    IpcDescription, IpcLimit, IpcLimitValue, IpcNamespace, IpcTable, MessageLimit,
    SharedMemoryLimit, initial_namespace, initialize_namespaces,
};
pub(crate) use permissions::{IpcAccess, IpcPermission};

mod error;
pub(crate) use error::IpcError;

#[path = "receive_buffer.rs"]
mod receive_buffer;
pub(crate) use receive_buffer::ReceiveBuffer;

#[path = "eventfd.rs"]
mod eventfd;
pub(crate) use eventfd::{EventFd, EventFdRead, EventFdWrite};

#[path = "pipe.rs"]
mod pipe;
pub(crate) use pipe::{
    PIPE_BUF, Pipe, PipeDirection, PipeEnd, PipeOpenAccess, PipeOpenError, PipePollState, PipeRead,
    PipeResizeError, PipeScheduler, PipeWaitCondition, PipeWrite, install_pipe_scheduler,
};
