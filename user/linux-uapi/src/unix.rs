//! Polling and Unix ancillary-data operations absent from [`std`].

use std::{
    ffi::{c_int, c_void},
    io,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    time::Duration,
};

use crate::raw;

/// Events requested from or returned by [`poll`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PollEvents(i16);

impl PollEvents {
    pub const READ: Self = Self(raw::POLLIN);
    pub const WRITE: Self = Self(raw::POLLOUT);
    pub const ERROR: Self = Self(raw::POLLERR);
    pub const HANGUP: Self = Self(raw::POLLHUP);
    pub const EMPTY: Self = Self(0);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl std::ops::BitOr for PollEvents {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// One non-owning descriptor in a synchronous [`poll`] operation.
///
/// The descriptor owner must remain alive until the enclosing [`poll`] call returns.
pub struct PollFd {
    raw: raw::PollFd,
}

impl PollFd {
    pub fn new(fd: BorrowedFd<'_>, events: PollEvents) -> Self {
        Self {
            raw: raw::PollFd {
                fd: fd.as_raw_fd(),
                events: events.0,
                returned: 0,
            },
        }
    }

    pub fn returned(&self) -> PollEvents {
        PollEvents(self.raw.returned)
    }
}

/// Waits for readiness without hiding `EINTR` from the caller.
pub fn poll(descriptors: &mut [PollFd], timeout: Option<Duration>) -> io::Result<usize> {
    let timeout = match timeout {
        None => -1,
        Some(duration) => i32::try_from(duration.as_millis()).unwrap_or(i32::MAX),
    };
    let result = unsafe {
        raw::poll(
            descriptors.as_mut_ptr().cast::<raw::PollFd>(),
            descriptors.len(),
            timeout,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

const CMSG_FD_LEN: usize = 20;
const CMSG_FD_SPACE: usize = 24;
const RECV_CONTROL_LEN: usize = 32;

#[repr(C)]
struct FdControl {
    header: raw::CmsgHdr,
    fd: c_int,
    padding: c_int,
}

#[repr(align(8))]
struct RecvControl([u8; RECV_CONTROL_LEN]);

const _: () = assert!(size_of::<FdControl>() == CMSG_FD_SPACE);

/// Sends one buffer and one borrowed descriptor in a single `SCM_RIGHTS` message.
pub fn send_fd(socket: BorrowedFd<'_>, bytes: &[u8], fd: BorrowedFd<'_>) -> io::Result<usize> {
    let mut control = FdControl {
        header: raw::CmsgHdr {
            len: CMSG_FD_LEN,
            level: raw::SOL_SOCKET,
            kind: raw::SCM_RIGHTS,
        },
        fd: fd.as_raw_fd(),
        padding: 0,
    };
    let mut vector = raw::IoVec {
        base: bytes.as_ptr().cast::<c_void>().cast_mut(),
        len: bytes.len(),
    };
    let message = raw::MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut vector,
        iov_len: 1,
        control: (&raw mut control).cast(),
        control_len: CMSG_FD_SPACE,
        flags: 0,
    };
    let result = unsafe { raw::sendmsg(socket.as_raw_fd(), &raw const message, 0) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// Receives bytes and at most one owned `SCM_RIGHTS` descriptor.
pub fn recv_fd(socket: BorrowedFd<'_>, bytes: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut vector = raw::IoVec {
        base: bytes.as_mut_ptr().cast(),
        len: bytes.len(),
    };
    let mut control = RecvControl([0; RECV_CONTROL_LEN]);
    let mut message = raw::MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut vector,
        iov_len: 1,
        control: control.0.as_mut_ptr().cast(),
        control_len: RECV_CONTROL_LEN,
        flags: 0,
    };
    let result =
        unsafe { raw::recvmsg(socket.as_raw_fd(), &raw mut message, raw::MSG_CMSG_CLOEXEC) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let received = if message.control_len >= CMSG_FD_LEN {
        let length = usize::from_ne_bytes(control.0[0..8].try_into().expect("control length"));
        let level = i32::from_ne_bytes(control.0[8..12].try_into().expect("control level"));
        let kind = i32::from_ne_bytes(control.0[12..16].try_into().expect("control kind"));
        if length >= CMSG_FD_LEN && level == raw::SOL_SOCKET && kind == raw::SCM_RIGHTS {
            let fd = i32::from_ne_bytes(control.0[16..20].try_into().expect("control fd"));
            Some(unsafe { OwnedFd::from_raw_fd(fd) })
        } else {
            None
        }
    } else {
        None
    };
    if message.flags & raw::MSG_CTRUNC != 0 {
        drop(received);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SCM_RIGHTS control data was truncated",
        ));
    }
    Ok((result as usize, received))
}
