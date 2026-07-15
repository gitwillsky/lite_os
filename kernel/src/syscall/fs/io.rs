use super::*;

mod write_limit;
use write_limit::{bounded_regular_write, file_size_exceeded};

mod positioned;
pub(crate) use positioned::{
    sys_pread64, sys_preadv, sys_preadv2, sys_pwrite64, sys_pwritev, sys_pwritev2,
};

mod sendfile;
pub(crate) use sendfile::sys_sendfile;

mod regular;
use regular::{read_vectors as read_regular_vectors, write_vectors as write_regular_vectors};

use crate::syscall::user_iovec::{
    ImportError, TotalLengthError, UserIoCursor, UserIoVec, checked_total_length,
    import_iovecs as import_raw_iovecs,
};

/// @description fs vector I/O policy wrapper；raw ABI import 与 SSIZE_MAX owner 保持分离。
fn import_iovecs(
    task: &TaskControlBlock,
    iovector: usize,
    count: usize,
) -> Result<(Vec<UserIoVec>, usize), isize> {
    let vectors = import_raw_iovecs(task, iovector, count).map_err(|error| match error {
        ImportError::TooMany => -errno::EINVAL,
        ImportError::NullArray | ImportError::AddressOverflow | ImportError::CopyFault => {
            -errno::EFAULT
        }
        ImportError::NoMemory => -errno::ENOMEM,
    })?;
    let total =
        checked_total_length(&vectors, isize::MAX as usize).map_err(|error| match error {
            TotalLengthError::Overflow | TotalLengthError::Limit => -errno::EINVAL,
        })?;
    Ok((vectors, total))
}

mod sequential;
pub(crate) use sequential::{sys_read, sys_readv, sys_write, sys_writev};
