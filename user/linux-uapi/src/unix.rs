//! Polling and Unix ancillary-data operations absent from [`std`].

use std::{
    ffi::{c_int, c_void},
    io,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;

use crate::raw;

/// Linux `SO_PEERCRED` identity for a connected Unix socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PeerCredentials {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// Returns the kernel-authenticated process identity of a connected Unix peer.
#[cfg(target_os = "linux")]
pub fn peer_credentials(socket: BorrowedFd<'_>) -> io::Result<PeerCredentials> {
    const SO_PEERCRED: c_int = 17;
    let mut credentials = PeerCredentials {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = size_of::<PeerCredentials>() as u32;
    let result = unsafe {
        raw::getsockopt(
            socket.as_raw_fd(),
            raw::SOL_SOCKET,
            SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != size_of::<PeerCredentials>() || credentials.pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an invalid credential record",
        ));
    }
    Ok(credentials)
}

/// Non-Linux hosts cannot provide the Linux peer PID contract.
#[cfg(not(target_os = "linux"))]
pub fn peer_credentials(_socket: BorrowedFd<'_>) -> io::Result<PeerCredentials> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_PEERCRED is only available on Linux",
    ))
}

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
#[cfg(target_os = "linux")]
const RECV_CONTROL_LEN: usize = 32;

#[repr(C)]
struct FdControl {
    header: raw::CmsgHdr,
    fd: c_int,
    padding: c_int,
}

#[cfg(target_os = "linux")]
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

/// Receives bytes and zero or exactly one owned `SCM_RIGHTS` descriptor.
pub fn recv_fd(socket: BorrowedFd<'_>, bytes: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut vector = raw::IoVec {
        base: bytes.as_mut_ptr().cast(),
        len: bytes.len(),
    };
    #[cfg(target_os = "linux")]
    let mut control = RecvControl([0; RECV_CONTROL_LEN]);
    #[cfg(target_os = "linux")]
    let mut message = raw::MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut vector,
        iov_len: 1,
        control: control.0.as_mut_ptr().cast(),
        control_len: RECV_CONTROL_LEN,
        flags: 0,
    };
    #[cfg(not(target_os = "linux"))]
    let mut message = raw::MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut vector,
        iov_len: 1,
        control: std::ptr::null_mut(),
        control_len: 0,
        flags: 0,
    };
    #[cfg(target_os = "linux")]
    let receive_flags = raw::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let receive_flags = 0;
    let result = unsafe { raw::recvmsg(socket.as_raw_fd(), &raw mut message, receive_flags) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    let received = parse_received_control(&control.0, message.control_len)?;
    #[cfg(not(target_os = "linux"))]
    let received = None;
    #[cfg(target_os = "linux")]
    if message.flags & raw::MSG_CTRUNC != 0 {
        drop(received);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SCM_RIGHTS control data was truncated",
        ));
    }
    Ok((result as usize, received))
}

#[cfg(target_os = "linux")]
fn parse_received_control(bytes: &[u8], length: usize) -> io::Result<Option<OwnedFd>> {
    const HEADER_BYTES: usize = 16;
    if length > bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SCM_RIGHTS control length exceeds receive storage",
        ));
    }
    let mut offset = 0usize;
    let mut received = None;
    let mut invalid = false;
    while offset + HEADER_BYTES <= length {
        let header_length = usize::from_ne_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("bounded control length"),
        );
        if header_length < HEADER_BYTES || header_length > length - offset {
            invalid = true;
            break;
        }
        let level = i32::from_ne_bytes(
            bytes[offset + 8..offset + 12]
                .try_into()
                .expect("bounded control level"),
        );
        let kind = i32::from_ne_bytes(
            bytes[offset + 12..offset + 16]
                .try_into()
                .expect("bounded control kind"),
        );
        let descriptor_bytes = header_length - HEADER_BYTES;
        if level != raw::SOL_SOCKET
            || kind != raw::SCM_RIGHTS
            || descriptor_bytes == 0
            || !descriptor_bytes.is_multiple_of(size_of::<c_int>())
        {
            invalid = true;
        } else {
            for descriptor in bytes[offset + HEADER_BYTES..offset + header_length]
                .as_chunks::<{ size_of::<c_int>() }>()
                .0
            {
                let raw_fd = i32::from_ne_bytes(*descriptor);
                if raw_fd < 0 {
                    invalid = true;
                    continue;
                }
                let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
                if received.is_some() {
                    invalid = true;
                } else {
                    received = Some(owned);
                }
            }
        }
        offset = offset.saturating_add((header_length + 7) & !7);
    }
    if invalid {
        drop(received);
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected zero or exactly one SCM_RIGHTS descriptor",
        ))
    } else {
        Ok(received)
    }
}

#[cfg(all(test, not(target_os = "linux")))]
mod host_tests {
    use std::{
        io::Write,
        os::{fd::AsFd, unix::net::UnixStream},
    };

    use super::recv_fd;

    #[test]
    fn recv_fd_receives_plain_payload_without_linux_ancillary_layout() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(b"plain-control-frame").unwrap();
        let mut output = [0u8; 32];
        let (count, fd) = recv_fd(receiver.as_fd(), &mut output).unwrap();
        assert_eq!(&output[..count], b"plain-control-frame");
        assert!(fd.is_none());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        ffi::c_void,
        fs::File,
        os::{
            fd::{AsFd, AsRawFd},
            unix::net::{UnixDatagram, UnixStream},
        },
    };

    use super::*;

    #[repr(C)]
    struct TwoFdControl {
        header: raw::CmsgHdr,
        descriptors: [c_int; 2],
    }

    #[test]
    fn recv_fd_rejects_and_closes_multiple_rights() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        let first = File::open("/dev/null").unwrap();
        let second = File::open("/dev/null").unwrap();
        let mut byte = [1u8];
        let mut vector = raw::IoVec {
            base: byte.as_mut_ptr().cast::<c_void>(),
            len: byte.len(),
        };
        let mut control = TwoFdControl {
            header: raw::CmsgHdr {
                len: size_of::<TwoFdControl>(),
                level: raw::SOL_SOCKET,
                kind: raw::SCM_RIGHTS,
            },
            descriptors: [first.as_raw_fd(), second.as_raw_fd()],
        };
        let message = raw::MsgHdr {
            name: std::ptr::null_mut(),
            name_len: 0,
            iov: &mut vector,
            iov_len: 1,
            control: (&raw mut control).cast(),
            control_len: size_of::<TwoFdControl>(),
            flags: 0,
        };
        assert_eq!(
            unsafe { raw::sendmsg(sender.as_raw_fd(), &raw const message, 0) },
            1
        );
        let mut output = [0u8; 1];
        let error = recv_fd(receiver.as_fd(), &mut output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn peer_credentials_returns_kernel_authenticated_pid() {
        let (peer, _other) = UnixStream::pair().unwrap();
        let credentials = peer_credentials(peer.as_fd()).unwrap();
        assert_eq!(credentials.pid as u32, std::process::id());
    }
}
