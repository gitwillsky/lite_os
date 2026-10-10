// 实际 IPC namespace/queue/permission/error owner 的 host 装配；不复制 IPC 状态或实现。
#[path = "../../../kernel/src/ipc/sem_undo.rs"]
mod sem_undo;
#[path = "../../../kernel/src/ipc/semaphore.rs"]
mod semaphore;
#[allow(
    unused_imports,
    reason = "独立 IPC fixture 只使用完整 owner 的部分导出"
)]
pub(crate) use sem_undo::{SemaphoreUndoHandle, SemaphoreUndoList};
#[allow(
    unused_imports,
    reason = "独立 IPC fixture 只使用完整 owner 的部分导出"
)]
pub(crate) use semaphore::{SemaphoreChange, SemaphoreOperation, SemaphoreSet, SemaphoreSnapshot};
#[path = "../../../kernel/src/ipc/error.rs"]
mod error;
#[path = "../../../kernel/src/ipc/ids.rs"]
mod ids;
#[path = "../../../kernel/src/ipc/shared_memory.rs"]
mod shared_memory;
#[path = "../../../kernel/src/ipc/shared_memory_creator.rs"]
mod shared_memory_creator;
#[allow(
    unused_imports,
    reason = "独立 IPC fixture 只使用完整 owner 的部分导出"
)]
pub(crate) use error::IpcError;
#[allow(
    unused_imports,
    reason = "独立 IPC fixture 只使用完整 owner 的部分导出"
)]
pub(crate) use shared_memory::{
    SharedMemoryAttachReservation, SharedMemorySegment, SharedMemorySnapshot,
};
#[allow(
    unused_imports,
    reason = "独立 IPC fixture 只使用完整 owner 的部分导出"
)]
pub(crate) use shared_memory_creator::SharedMemoryCreatorList;
#[path = "../../../kernel/src/ipc/permissions.rs"]
mod permissions;
#[allow(
    unused_imports,
    reason = "独立 IPC fixture 只使用完整 owner 的部分导出"
)]
pub(crate) use permissions::{IpcAccess, IpcPermission};
#[path = "../../../kernel/src/ipc/message_queue.rs"]
mod message_queue;
#[allow(
    unused_imports,
    reason = "独立 IPC fixture 只使用完整 owner 的部分导出"
)]
pub(crate) use message_queue::{
    MessageQueue, MessageQueueSnapshot, MessageQueueWait, MessageReceive, PreparedMessage,
};
#[path = "../../../kernel/src/ipc/namespace.rs"]
mod namespace;
#[allow(
    unused_imports,
    reason = "独立 IPC fixture 只使用完整 owner 的部分导出"
)]
pub(crate) use namespace::{
    IpcDescription, IpcLimit, IpcLimitValue, IpcNamespace, IpcTable, MessageLimit,
    SharedMemoryLimit, initial_namespace, initialize_namespaces,
};
