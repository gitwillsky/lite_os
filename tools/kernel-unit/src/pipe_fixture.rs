//! 单独装配实际 Pipe/EventFd leaves；不把不相关的 SysV namespace/storage 放入 endpoint fixture。
#[path = "../../../kernel/src/ipc/receive_buffer.rs"]
mod receive_buffer;
#[allow(unused_imports, reason = "host 用例只使用共享 owner 的部分导出")]
pub(crate) use receive_buffer::ReceiveBuffer;
#[path = "../../../tools/kernel-view/ipc/pipe/pipe/pipe.rs"]
mod pipe;
#[allow(unused_imports, reason = "host 用例只使用共享 owner 的部分导出")]
pub(crate) use pipe::{
    PIPE_BUF, PIPE_MAX_SIZE, Pipe, PipeDirection, PipeEnd, PipeOpenAccess, PipeOpenError,
    PipeOpened, PipePollState, PipeRead, PipeResizeError, PipeScheduler, PipeWaitCondition,
    PipeWrite, install_pipe_scheduler,
};
#[path = "../../../kernel/src/ipc/eventfd.rs"]
mod eventfd;
#[allow(unused_imports, reason = "host 用例只使用共享 owner 的部分导出")]
pub(crate) use eventfd::{EventFd, EventFdRead, EventFdWrite};
