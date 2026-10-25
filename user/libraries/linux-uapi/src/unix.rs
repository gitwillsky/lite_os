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

/// 设置已有 Linux OFD 的非阻塞状态，不改变访问模式；别名共享这一状态。
///
/// fd 借用在调用期间有效；fcntl 失败返回原 errno，非 Linux host 明确不支持。
#[cfg(target_os = "linux")]
pub fn set_nonblocking(fd: BorrowedFd<'_>, enabled: bool) -> io::Result<()> {
    const F_GETFL: c_int = 3;
    const F_SETFL: c_int = 4;
    // SAFETY: fcntl 的这两个命令只消费整数，不访问用户指针；fd 的 owner 仍存活。
    let flags = unsafe { raw::fcntl(fd.as_raw_fd(), F_GETFL, 0) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let updated = if enabled {
        flags | raw::O_NONBLOCK
    } else {
        flags & !raw::O_NONBLOCK
    };
    if unsafe { raw::fcntl(fd.as_raw_fd(), F_SETFL, updated) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 非 Linux host 不提供 Linux OFD flag 修改。
#[cfg(not(target_os = "linux"))]
pub fn set_nonblocking(_fd: BorrowedFd<'_>, _enabled: bool) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

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
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct PollFd<'fd> {
    raw: raw::PollFd,
    descriptor: std::marker::PhantomData<BorrowedFd<'fd>>,
}

impl<'fd> PollFd<'fd> {
    pub fn new(fd: BorrowedFd<'fd>, events: PollEvents) -> Self {
        Self {
            raw: raw::PollFd {
                fd: fd.as_raw_fd(),
                events: events.0,
                returned: 0,
            },
            descriptor: std::marker::PhantomData,
        }
    }

    pub fn returned(&self) -> PollEvents {
        PollEvents(self.raw.returned)
    }

    /// 开始新一轮等待前清空旧 readiness；不修改 fd 或 interest。
    pub fn clear_readiness(&mut self) {
        self.raw.returned = 0;
    }

    /// 将同一 fd/interest 的批量 poll 结果写回原槽位，不转移或延长描述符借用。
    ///
    /// fd 或 interest 不匹配返回 InvalidInput，避免把另一个源的 readiness 误投递。
    pub fn copy_readiness(&mut self, source: &PollFd<'_>) -> io::Result<()> {
        if self.raw.fd != source.raw.fd || self.raw.events != source.raw.events {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "poll source mismatch",
            ));
        }
        self.raw.returned = source.raw.returned;
        Ok(())
    }
}

/// Waits for readiness without hiding `EINTR` from the caller.
pub fn poll(descriptors: &mut [PollFd<'_>], timeout: Option<Duration>) -> io::Result<usize> {
    poll_raw(descriptors.as_mut_ptr().cast(), descriptors.len(), timeout)
}
fn poll_raw(
    descriptors: *mut raw::PollFd,
    count: usize,
    timeout: Option<Duration>,
) -> io::Result<usize> {
    let timeout = timeout.map_or(-1, |duration| {
        duration
            .as_millis()
            .saturating_add(u128::from(duration.subsec_nanos() % 1_000_000 != 0))
            .min(i32::MAX as u128) as i32
    });
    // SAFETY: 仅 poll/PollSet::wait 调用，指针指向本轮独占的 count 个 repr(C) 项；
    // PollFd 的借用或 PollSet 的生命周期证明 fd 存活，系统调用不会保留该数组。
    let result = unsafe { raw::poll(descriptors, count, timeout) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// 可复用的 poll 存储；自身不拥有、也不能轮询任何 FD，必须每轮重新建立有效借用。
#[derive(Default)]
pub struct PollBuffer {
    slots: Vec<raw::PollFd>,
}
impl PollBuffer {
    /// 清除上轮 FD 数字并保留容量，返回本轮借用集合。
    pub fn prepare(&mut self) -> PollSet<'_> {
        self.slots.clear();
        PollSet {
            buffer: self,
            descriptors: std::marker::PhantomData,
        }
    }
}
/// 一轮 FD 借用；加入的描述符 owner 必须活到集合最后使用结束，析构保留容量并清除旧编号。
pub struct PollSet<'fd> {
    buffer: &'fd mut PollBuffer,
    descriptors: std::marker::PhantomData<BorrowedFd<'fd>>,
}
impl<'fd> PollSet<'fd> {
    /// 加入一个有效借用的源，返回稳定的本轮索引。
    pub fn push(&mut self, descriptor: PollFd<'fd>) -> usize {
        let index = self.buffer.slots.len();
        self.buffer.slots.push(descriptor.raw);
        index
    }
    /// 批量加入源；顺序与迭代器一致。
    pub fn extend(&mut self, sources: impl IntoIterator<Item = PollFd<'fd>>) {
        for source in sources {
            self.push(source);
        }
    }
    /// 借用本轮完整切片以组合上层等待；不能通过它延长任何 FD owner 的生命周期。
    pub fn sources(&mut self) -> &mut [PollFd<'fd>] {
        // SAFETY: PollFd repr(transparent) 包装同一 raw 布局；每个 slot 只由 push 的有效 'fd 借用创建。
        // buffer 在此期间被独占；切片不拥有 FD，集合析构清空旧数字，下一轮不能轮询过期 slot。
        unsafe {
            std::slice::from_raw_parts_mut(
                self.buffer.slots.as_mut_ptr().cast(),
                self.buffer.slots.len(),
            )
        }
    }
    /// 等待本轮全部源，保留 EINTR 供上层重新计算 deadline。
    pub fn wait(&mut self, timeout: Option<Duration>) -> io::Result<usize> {
        poll(self.sources(), timeout)
    }
    /// 查询本轮某个源的结果；未知索引返回 None。
    pub fn returned(&self, index: usize) -> Option<PollEvents> {
        self.buffer
            .slots
            .get(index)
            .map(|slot| PollEvents(slot.returned))
    }
}
impl Drop for PollSet<'_> {
    fn drop(&mut self) {
        self.buffer.slots.clear();
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
