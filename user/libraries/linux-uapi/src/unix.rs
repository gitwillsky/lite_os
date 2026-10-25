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

/// 接管并验证一个 AF_UNIX SOCK_STREAM 监听描述符。
///
/// # Returns
///
/// 已验证的标准 UnixListener；所有错误都关闭传入 fd，调用方不保留第二个 owner。
///
/// # Errors
///
/// 非 UNIX、非 stream、尚未 listen，或 getsockopt 失败。非 Linux 目标明确返回 Unsupported。
pub fn unix_listener_from_fd(fd: OwnedFd) -> io::Result<std::os::unix::net::UnixListener> {
    let listener = std::os::unix::net::UnixListener::from(fd);
    #[cfg(not(target_os = "linux"))]
    {
        drop(listener);
        Err(io::ErrorKind::Unsupported.into())
    }
    #[cfg(target_os = "linux")]
    {
        listener.local_addr()?;
        let (level, kind, accepting) = (1, 3, 30);
        for (option, expected) in [(kind, 1), (accepting, 1)] {
            let mut value: c_int = 0;
            let mut length = size_of::<c_int>() as u32;
            // SAFETY: 指针覆盖完整 c_int 和 socklen_t，listener 保活 fd；只查询实际内核 socket 状态。
            let result = unsafe {
                raw::getsockopt(
                    listener.as_raw_fd(),
                    level,
                    option,
                    (&raw mut value).cast(),
                    &raw mut length,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            if length as usize != size_of::<c_int>() || value != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a listening UNIX stream socket",
                ));
            }
        }
        Ok(listener)
    }
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
/// One `recvmsg` accepts at most this many descriptors (libwayland's `MAX_FDS_OUT`); more are `MSG_CTRUNC`.
pub const MAX_RECEIVED_FDS: usize = 28;
#[cfg(target_os = "linux")]
const RECV_CONTROL_LEN: usize = 16 + MAX_RECEIVED_FDS * size_of::<c_int>();

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

/// Receives bytes and up to [`MAX_RECEIVED_FDS`] owned `SCM_RIGHTS` descriptors, in arrival order.
///
/// Malformed or truncated ancillary data closes every descriptor that did arrive and fails, so a
/// peer cannot leak descriptors into this process by sending bad control messages.
pub fn recv_fds(socket: BorrowedFd<'_>, bytes: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
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
    let received = Vec::new();
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
fn parse_received_control(bytes: &[u8], length: usize) -> io::Result<Vec<OwnedFd>> {
    const HEADER_BYTES: usize = 16;
    if length > bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SCM_RIGHTS control length exceeds receive storage",
        ));
    }
    let mut offset = 0usize;
    let mut received = Vec::new();
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
                received.push(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            }
        }
        offset = offset.saturating_add((header_length + 7) & !7);
    }
    if invalid {
        drop(received);
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed SCM_RIGHTS control message",
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

    use super::recv_fds;

    #[test]
    fn recv_fd_receives_plain_payload_without_linux_ancillary_layout() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(b"plain-control-frame").unwrap();
        let mut output = [0u8; 32];
        let (count, fds) = recv_fds(receiver.as_fd(), &mut output).unwrap();
        assert_eq!(&output[..count], b"plain-control-frame");
        assert!(fds.is_empty());
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

    /// 经 `sendmsg` 在一条数据报里发送 `count` 个 `/dev/null` 描述符。
    fn send_rights(sender: &UnixDatagram, count: usize) {
        let files: Vec<File> = (0..count)
            .map(|_| File::open("/dev/null").unwrap())
            .collect();
        let descriptors: Vec<c_int> = files.iter().map(|file| file.as_raw_fd()).collect();
        let mut control = vec![0u8; 16 + descriptors.len() * size_of::<c_int>()];
        let length = control.len();
        control[..8].copy_from_slice(&length.to_ne_bytes());
        control[8..12].copy_from_slice(&raw::SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&raw::SCM_RIGHTS.to_ne_bytes());
        for (slot, descriptor) in control[16..].chunks_exact_mut(4).zip(&descriptors) {
            slot.copy_from_slice(&descriptor.to_ne_bytes());
        }
        let mut byte = [1u8];
        let mut vector = raw::IoVec {
            base: byte.as_mut_ptr().cast::<c_void>(),
            len: byte.len(),
        };
        let message = raw::MsgHdr {
            name: std::ptr::null_mut(),
            name_len: 0,
            iov: &mut vector,
            iov_len: 1,
            control: control.as_mut_ptr().cast(),
            control_len: control.len(),
            flags: 0,
        };
        assert_eq!(
            unsafe { raw::sendmsg(sender.as_raw_fd(), &raw const message, 0) },
            1
        );
    }

    #[test]
    fn recv_fds_receives_every_right_of_one_message() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        send_rights(&sender, 3);
        let mut output = [0u8; 1];
        let (count, fds) = recv_fds(receiver.as_fd(), &mut output).unwrap();
        assert_eq!((count, fds.len()), (1, 3));
    }

    #[test]
    fn recv_fds_accepts_the_maximum_and_rejects_more_without_leaking() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        send_rights(&sender, MAX_RECEIVED_FDS);
        let mut output = [0u8; 1];
        assert_eq!(
            recv_fds(receiver.as_fd(), &mut output).unwrap().1.len(),
            MAX_RECEIVED_FDS
        );
        send_rights(&sender, MAX_RECEIVED_FDS + 1);
        let error = recv_fds(receiver.as_fd(), &mut output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn peer_credentials_returns_kernel_authenticated_pid() {
        let (peer, _other) = UnixStream::pair().unwrap();
        let credentials = peer_credentials(peer.as_fd()).unwrap();
        assert_eq!(credentials.pid as u32, std::process::id());
    }
}
