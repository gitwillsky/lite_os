use super::*;

mod write_limit;
use write_limit::{bounded_regular_write, file_size_exceeded, regular_write_chunk};

mod positioned;
pub(in crate::syscall) use positioned::{
    AsyncPositionedRead, AsyncReadStep, begin_current_position_read, begin_positioned_read,
    positioned_read, positioned_write,
};
pub(crate) use positioned::{
    sys_pread64, sys_preadv, sys_preadv2, sys_pwrite64, sys_pwritev, sys_pwritev2,
};

mod sendfile;
pub(crate) use sendfile::{sys_copy_file_range, sys_sendfile};

mod regular;
use regular::{
    PreparedRegularWriteStaging, ReadProgress, read_vectors as read_regular_vectors,
    read_vectors_from, write_vectors as write_regular_vectors,
};

use crate::syscall::user_iovec::{
    ImportError, TotalLengthError, UserInputStaging, UserIoCursor, UserIoVec, checked_total_length,
    fallible_staging_capacity, import_iovecs as import_raw_iovecs, with_prepared_staging,
};

/// fs vector I/O policy wrapper；raw ABI import 与 SSIZE_MAX owner 保持分离。
pub(in crate::syscall) fn import_iovecs(
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
pub(in crate::syscall) use sequential::{
    check_readable, check_writable, perform_read, perform_write,
};
pub(crate) use sequential::{sys_read, sys_readv, sys_write, sys_writev};
