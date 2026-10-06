//! 字符设备 OFD 的通用 syscall 投影：用户内存拷贝、分块读写、ioctl 与 mmap 请求。
//!
//! 设备语义（记录边界、阻塞、UAPI 编解码）由设备子系统经 [`DeviceFile`] 实现；这里只拥有
//! user-copy 游标、`O_NONBLOCK`、部分完成与 errno 映射。

use alloc::vec::Vec;

use crate::{
    fs::{
        O_NONBLOCK, OpenFileDescription,
        device::{
            DeviceError, DeviceFile, IoctlCall, MapRequest, UserFault, UserInput, UserMemory,
            UserOutput,
        },
    },
    task::TaskControlBlock,
};

use super::{
    INTERNAL_RESTART_SYS,
    user_iovec::{UserIoCursor, UserIoVec},
};

/// 当前 task 地址空间上的 [`UserMemory`]。
struct TaskUserMemory<'a>(&'a TaskControlBlock);

impl UserMemory for TaskUserMemory<'_> {
    fn read(&self, address: usize, bytes: &mut [u8]) -> Result<(), UserFault> {
        if address == 0 {
            return Err(UserFault);
        }
        self.0.copy_from_user(address, bytes).map_err(|_| UserFault)
    }

    fn write(&self, address: usize, bytes: &[u8]) -> Result<(), UserFault> {
        if address == 0 {
            return Err(UserFault);
        }
        self.0.copy_to_user(address, bytes).map_err(|_| UserFault)
    }

    fn validate_write(&self, address: usize, length: usize) -> Result<(), UserFault> {
        if address == 0 {
            return Err(UserFault);
        }
        self.0
            .validate_user_write(address, length)
            .map_err(|_| UserFault)
    }

    fn read_c_string(&self, address: usize, maximum: usize) -> Result<Vec<u8>, UserFault> {
        if address == 0 {
            return Err(UserFault);
        }
        self.0
            .copy_user_c_string(address, maximum)
            .map_err(|_| UserFault)
    }
}

/// 把设备错误映射为 syscall 返回值。
pub(super) fn device_error(error: DeviceError) -> isize {
    match error {
        DeviceError::Restart => INTERNAL_RESTART_SYS,
        DeviceError::WouldBlock | DeviceError::Errno(_) => -error.errno(),
    }
}

fn progress_or(completed: usize, result: Result<(), DeviceError>) -> isize {
    match result {
        Err(error) if completed == 0 => device_error(error),
        _ => completed as isize,
    }
}

/// read/readv 的用户 iovec 目标游标。
struct TaskUserOutput<'a> {
    task: &'a TaskControlBlock,
    cursor: UserIoCursor<'a>,
    total_length: usize,
}

impl UserOutput for TaskUserOutput<'_> {
    fn remaining(&self) -> usize {
        self.total_length - self.cursor.completed()
    }

    fn reserve(&self, length: usize) -> Result<(), UserFault> {
        self.cursor
            .validate_write_prefix(self.task, length)
            .map_err(|()| UserFault)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault> {
        debug_assert!(bytes.len() <= self.remaining());
        self.cursor
            .copy_to_user(self.task, bytes)
            .map(|_| ())
            .map_err(|()| UserFault)
    }

    fn zero_remaining(&mut self) -> Result<(), UserFault> {
        self.cursor
            .zero_to_user(self.task)
            .map(|_| ())
            .map_err(|()| UserFault)
    }
}

/// write/writev 的用户 iovec 源游标。
struct TaskUserInput<'a> {
    task: &'a TaskControlBlock,
    cursor: UserIoCursor<'a>,
    total_length: usize,
}

impl UserInput for TaskUserInput<'_> {
    fn remaining(&self) -> usize {
        self.total_length - self.cursor.completed()
    }

    fn copy(&self, bytes: &mut [u8]) -> Result<(), UserFault> {
        debug_assert!(bytes.len() <= self.remaining());
        let staged = self.cursor.stage_from_user(self.task, bytes);
        if staged.faulted || staged.count != bytes.len() {
            return Err(UserFault);
        }
        Ok(())
    }

    fn consume(&mut self, count: usize) {
        debug_assert!(count <= self.remaining());
        self.cursor.advance(count);
    }
}

/// 设备 read/readv：设备经游标直接交付；出错时已交付的进度优先返回。
pub(super) fn read_device(
    task: &TaskControlBlock,
    ofd: &OpenFileDescription,
    file: &dyn DeviceFile,
    vectors: &[UserIoVec],
    total_length: usize,
) -> isize {
    let mut output = TaskUserOutput {
        task,
        cursor: UserIoCursor::new(vectors),
        total_length,
    };
    let result = file.read(&mut output, *ofd.flags.lock() & O_NONBLOCK != 0);
    progress_or(output.cursor.completed(), result)
}

/// 设备 write/writev：返回设备实际提交的字节数；出错时已提交的进度优先返回。
pub(super) fn write_device(
    task: &TaskControlBlock,
    ofd: &OpenFileDescription,
    file: &dyn DeviceFile,
    vectors: &[UserIoVec],
    total_length: usize,
) -> isize {
    let mut input = TaskUserInput {
        task,
        cursor: UserIoCursor::new(vectors),
        total_length,
    };
    let result = file.write(&mut input, *ofd.flags.lock() & O_NONBLOCK != 0);
    progress_or(input.cursor.completed(), result)
}

/// 设备 ioctl：UAPI 编解码由设备完成，这里只提供用户内存与调用者属性。
pub(super) fn ioctl_device(
    task: &TaskControlBlock,
    ofd: &OpenFileDescription,
    file: &dyn DeviceFile,
    request: usize,
    argument: usize,
) -> isize {
    let call = IoctlCall {
        request,
        argument,
        user: &TaskUserMemory(task),
        nonblocking: *ofd.flags.lock() & O_NONBLOCK != 0,
        privileged: task.credential_id(true, true) == 0,
    };
    file.ioctl(&call).unwrap_or_else(device_error)
}

/// 构造设备裁决 mmap 所需的请求。
pub(super) fn map_request(
    shared: bool,
    writable: bool,
    executable: bool,
    fd_writable: bool,
) -> MapRequest {
    MapRequest {
        shared,
        writable,
        executable,
        fd_readable: true,
        fd_writable,
    }
}
