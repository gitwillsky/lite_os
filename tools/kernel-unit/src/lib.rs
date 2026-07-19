#![cfg_attr(test, feature(allocator_api))]
// Host fixtures load selected production leaves both alone and through their real parent module.
#![cfg_attr(test, allow(clippy::duplicate_mod))]

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InodeType {
    File,
    Directory,
    SymLink,
    CharacterDevice,
    Fifo,
    Socket,
}

#[cfg(test)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileSystemError {
    AlreadyExists,
    Busy,
    CrossDevice,
    DirectoryNotEmpty,
    InvalidFileSystem,
    InvalidOperation,
    InvalidPath,
    IoError,
    IsDirectory,
    NoSpace,
    NotDirectory,
    NotFound,
    OutOfMemory,
    PermissionDenied,
    TooManyLinks,
}

#[cfg(test)]
macro_rules! error {
    ($($argument:tt)*) => {{ let _ = core::format_args!($($argument)*); }};
}

#[cfg(test)]
#[allow(unused_macros)]
macro_rules! debug {
    ($($argument:tt)*) => {{ let _ = core::format_args!($($argument)*); }};
}

#[cfg(test)]
#[allow(unused_macros)]
macro_rules! info {
    ($($argument:tt)*) => {{ let _ = core::format_args!($($argument)*); }};
}

#[cfg(test)]
#[allow(dead_code)]
mod drivers;

#[cfg(test)]
mod sync;

#[cfg(test)]
#[path = "../../../kernel/src/ipc/receive_buffer.rs"]
#[allow(dead_code)]
mod receive_buffer;

#[cfg(test)]
mod timer {
    pub(crate) fn get_realtime_ns() -> u64 {
        1_800_000_000_000_000_000
    }
}

#[cfg(test)]
#[allow(dead_code)]
mod fs;

#[cfg(test)]
mod ext2_cost_tests;

#[cfg(test)]
#[path = "../../../kernel/src/fs/directory.rs"]
#[allow(dead_code)]
mod directory_stream;

#[cfg(test)]
#[path = "../../../kernel/src/fs/ext2/directory_cursor.rs"]
mod ext2_directory_cursor;

#[cfg(test)]
#[path = "../../../kernel/src/fallible_tree.rs"]
#[allow(dead_code)]
mod fallible_tree;

#[cfg(test)]
mod fallible_tree_tests;

#[cfg(test)]
#[path = "../../../kernel/src/socket/inet/port_namespace.rs"]
mod inet_port_namespace;

#[cfg(test)]
mod inet_port_namespace_tests;

#[cfg(test)]
#[path = "../../../kernel/src/drivers/block.rs"]
#[allow(dead_code)]
mod block_device;

#[cfg(test)]
#[path = "../../../kernel/src/drivers/virtio_blk/policy.rs"]
mod virtio_blk_policy;

#[cfg(test)]
#[path = "../../../kernel/src/drivers/virtio_rng/completion_policy.rs"]
mod virtio_rng_completion_policy;

#[cfg(test)]
#[path = "../../../kernel/src/fs/file/indexed_slots.rs"]
mod indexed_slots;

#[cfg(test)]
#[path = "../../../kernel/src/id.rs"]
#[allow(dead_code)]
mod kernel_id;

#[cfg(test)]
#[path = "../../../kernel/src/fs/file/position.rs"]
mod file_position;

#[cfg(test)]
#[path = "../../../kernel/src/fs/ext2/journal_layout.rs"]
mod journal_layout;

#[cfg(test)]
#[path = "../../../kernel/src/fs/page_cache/writeback_batch.rs"]
mod writeback_batch;

#[cfg(test)]
#[path = "../../../kernel/src/syscall/fs/io/write_limit.rs"]
mod regular_write_policy;

#[cfg(test)]
#[path = "kernel_memory.rs"]
mod memory;

#[cfg(test)]
#[path = "../../../kernel/src/memory/mm/file_page_range.rs"]
mod file_page_range;

#[cfg(test)]
#[path = "../../../kernel/src/memory/mm/fault_preflight.rs"]
mod fault_preflight;

#[cfg(test)]
mod memory_retire;

#[cfg(test)]
#[path = "../../../kernel/src/task/model/user_context.rs"]
mod task_user_context;

#[cfg(test)]
#[path = "../../../kernel/src/drivers/virtio_net/rx_slots.rs"]
mod virtio_net_rx_slots;

#[cfg(test)]
#[path = "../../../kernel/src/drivers/virtio_gpu/sequence_policy.rs"]
#[allow(dead_code)]
mod virtio_gpu_sequence_policy;

#[cfg(test)]
#[path = "../../../kernel/src/timer/deadline.rs"]
mod timer_deadline;

#[cfg(test)]
#[path = "../../../kernel/src/platform/qemu_virt/plic_policy.rs"]
mod plic_policy;

#[cfg(test)]
#[path = "../../../kernel/src/arch/riscv64/sv39.rs"]
mod sv39;

#[cfg(test)]
#[path = "../../../kernel/src/arch/riscv64/pte.rs"]
mod riscv_pte;

#[cfg(test)]
#[path = "../../../kernel/src/arch/riscv64/fp_instruction.rs"]
mod riscv_fp_instruction;

#[cfg(test)]
mod riscv_page_table_fixture;

#[cfg(test)]
#[path = "../../../kernel/src/socket/unix/datagram_queue.rs"]
mod unix_datagram_queue;

#[cfg(test)]
#[path = "../../../kernel/src/socket/unix/stream_backlog.rs"]
mod unix_stream_backlog;

#[cfg(test)]
#[path = "../../../kernel/src/syscall/user_iovec.rs"]
#[allow(dead_code)]
mod user_iovec;

#[cfg(test)]
mod user_iovec_tests;

#[cfg(test)]
#[path = "../../../kernel/src/socket/message_limits.rs"]
mod socket_message_limits;

#[cfg(test)]
#[path = "../../../kernel/src/syscall/socket/receive_publication.rs"]
mod socket_receive_publication;

#[cfg(test)]
#[path = "../../../kernel/src/drm/publication_order.rs"]
mod drm_publication;

#[cfg(test)]
#[path = "../../../kernel/src/socket/device_error.rs"]
mod network_device_error;

#[cfg(test)]
#[path = "../../../kernel/src/drivers/network.rs"]
#[allow(dead_code)]
mod network_transmit;

#[cfg(test)]
#[path = "../../../kernel/src/fs/file/terminal_flush.rs"]
mod terminal_flush;

#[cfg(test)]
#[path = "../../../kernel/src/fs/file/terminal/input_batch.rs"]
mod terminal_input_batch;

#[cfg(test)]
#[path = "../../../kernel/src/syscall/errno.rs"]
#[allow(dead_code)]
mod errno;

#[cfg(test)]
#[path = "../../../kernel/src/syscall/clone_errno.rs"]
mod clone_errno;

#[cfg(test)]
#[path = "../../../kernel/src/fs/pty/input_notification.rs"]
mod pty_input_notification;

#[cfg(test)]
#[path = "../../../kernel/src/task/task_manager/console_batch.rs"]
mod console_batch;

#[cfg(test)]
#[path = "../../../kernel/src/task/model/clone_tid_store.rs"]
mod clone_tid_store;

#[cfg(test)]
#[path = "../../../kernel/src/task/task_manager/thread_activation.rs"]
mod thread_activation;

#[cfg(test)]
#[path = "../../../kernel/src/task/task_manager/wait_publication.rs"]
mod wait_publication;

#[cfg(test)]
#[path = "../../../kernel/src/task/task_manager/snapshot_staging.rs"]
mod snapshot_staging;

#[cfg(test)]
#[path = "../../../kernel/src/task/task_manager/timer_queue/preparation_policy.rs"]
mod timer_preparation_policy;

#[cfg(test)]
#[path = "../../../kernel/src/task/task_manager/timer_queue/transaction_loop.rs"]
mod timer_transaction_loop;

#[cfg(test)]
#[path = "tests/terminal_output_order.rs"]
mod terminal_output_order;

#[cfg(test)]
#[path = "../../../kernel/src/fs/ext2/link_count.rs"]
mod ext2_link_count;

#[cfg(test)]
mod file_position_tests;

#[cfg(test)]
mod regular_write_batch_tests;

#[cfg(test)]
#[path = "tests/filesystem_storage.rs"]
mod filesystem_storage_tests;

#[cfg(test)]
#[path = "tests/memory.rs"]
mod memory_tests;

#[cfg(test)]
#[path = "tests/platform_execution.rs"]
mod platform_execution_tests;

#[cfg(test)]
#[path = "tests/task_execution.rs"]
mod task_execution_tests;

#[cfg(test)]
#[path = "tests/socket_abi.rs"]
mod socket_abi_tests;

#[cfg(test)]
#[path = "tests/unix_stream_backlog.rs"]
mod unix_stream_backlog_tests;
