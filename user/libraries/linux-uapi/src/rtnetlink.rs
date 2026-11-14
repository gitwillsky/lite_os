//! 读取 Linux RTM_GETLINK 的实际接口及 64 位计数。

use crate::{
    raw,
    unix::{PollEvents, PollFd, poll},
};
use std::{
    io,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    ptr,
    time::{Duration, Instant},
};

const AF_NETLINK: u16 = 16;
const HEADER: usize = 16;
const LINK_INFO: usize = 16;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const NLM_F_DUMP_INTR: u16 = 0x10;
const MSG_TRUNC: i32 = 0x20;

/// 一条标准接口记录的身份和收发统计。
#[derive(Clone, Copy)]
pub struct LinkStatistics {
    pub index: u32,
    /// Linux 接口名，尾部以零填充。
    pub name: [u8; 16],
    /// 收到、发出的报文数、字节数、错误数和丢弃数，各组接收在前。
    pub counters: [u64; 8],
}

/// 独占的只读路由套接字，供同一个观察会话重复查询。
pub struct LinkReader {
    socket: OwnedFd,
    buffer: Vec<u8>,
    sequence: u32,
}

impl LinkReader {
    /// 打开不订阅组播的 NETLINK_ROUTE 套接字。
    /// # Errors
    /// 套接字创建或接收缓冲分配失败。
    pub fn open() -> io::Result<Self> {
        const SOCK_RAW_NONBLOCK_CLOEXEC: i32 = 3 | 0x800 | 0x80000;
        // SAFETY: 参数均为 Linux socket 常量，返回的独占描述符立刻交给 OwnedFd。
        let fd = unsafe { raw::socket(i32::from(AF_NETLINK), SOCK_RAW_NONBLOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(32 * 1024)?;
        buffer.resize(32 * 1024, 0);
        Ok(Self {
            socket,
            buffer,
            sequence: 0,
        })
    }

    /// 遍历一次完整的 GETLINK 回复。调用方须在成功返回后发布采样。
    /// # Errors
    /// 套接字故障、超时、截断、内核中止或回复无效。出错后应释放本 reader。
    pub fn read(
        &mut self,
        timeout: Duration,
        mut visit: impl FnMut(LinkStatistics),
    ) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        self.sequence = self.sequence.wrapping_add(1);
        let mut request = [0; HEADER + LINK_INFO];
        request[..4].copy_from_slice(&((HEADER + LINK_INFO) as u32).to_ne_bytes());
        request[4..6].copy_from_slice(&RTM_GETLINK.to_ne_bytes());
        // NLM_F_REQUEST | NLM_F_DUMP，ifinfomsg 的 family 为 AF_UNSPEC。
        request[6..8].copy_from_slice(&0x301u16.to_ne_bytes());
        request[8..12].copy_from_slice(&self.sequence.to_ne_bytes());
        let mut kernel = raw::SockAddrNl {
            family: AF_NETLINK,
            padding: 0,
            port_id: 0,
            groups: 0,
        };
        let mut message = Datagram::new(&mut kernel, &mut request);
        // SAFETY: msghdr、iov、地址与请求字节在调用期间均有效。
        let sent = unsafe { raw::sendmsg(self.socket.as_raw_fd(), &message.header(), 0) };
        if sent != request.len() as isize {
            return Err(if sent < 0 {
                io::Error::last_os_error()
            } else {
                io::ErrorKind::WriteZero.into()
            });
        }
        let mut descriptor = [PollFd::new(self.socket.as_fd(), PollEvents::READ)];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match poll(&mut descriptor, Some(remaining)) {
                Ok(0) => return Err(io::ErrorKind::TimedOut.into()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                other => {
                    other?;
                }
            }
            let mut source = raw::SockAddrNl {
                family: 0,
                padding: 0,
                port_id: 0,
                groups: 0,
            };
            let mut message = Datagram::new(&mut source, &mut self.buffer);
            let mut header = message.header();
            // SAFETY: header 指向本次调用独占的地址和缓冲，内核不保存这些指针。
            let received = unsafe { raw::recvmsg(self.socket.as_raw_fd(), &mut header, 0) };
            if received < 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    continue;
                }
                return Err(error);
            }
            if header.flags & MSG_TRUNC != 0 || received == 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            if header.name_len != size_of::<raw::SockAddrNl>() as u32
                || source.family != AF_NETLINK
                || source.port_id != 0
                || source.groups != 0
            {
                continue;
            }
            let mut bytes = &self.buffer[..received as usize];
            while !bytes.is_empty() {
                let head = bytes
                    .first_chunk::<HEADER>()
                    .ok_or(io::ErrorKind::InvalidData)?;
                let length = u32::from_ne_bytes(head[..4].try_into().expect("nlmsg_len")) as usize;
                let body = bytes
                    .get(HEADER..length)
                    .ok_or(io::ErrorKind::InvalidData)?;
                bytes = bytes
                    .get(length.next_multiple_of(4)..)
                    .ok_or(io::ErrorKind::InvalidData)?;
                if u32::from_ne_bytes(head[8..12].try_into().expect("nlmsg_seq")) != self.sequence {
                    continue;
                }
                let flags = u16::from_ne_bytes(head[6..8].try_into().expect("nlmsg_flags"));
                if flags & NLM_F_DUMP_INTR != 0 {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let kind = u16::from_ne_bytes(head[4..6].try_into().expect("nlmsg_type"));
                match kind {
                    RTM_NEWLINK => visit(parse_link(body)?),
                    2 | 3 => {
                        let status = i32::from_ne_bytes(
                            *body.first_chunk().ok_or(io::ErrorKind::InvalidData)?,
                        );
                        return if status == 0 && kind == 3 {
                            Ok(())
                        } else if status < 0 {
                            Err(io::Error::from_raw_os_error(status.saturating_neg()))
                        } else {
                            Err(io::ErrorKind::InvalidData.into())
                        };
                    }
                    _ => return Err(io::ErrorKind::InvalidData.into()),
                }
            }
        }
    }
}

// Storage for one iovec. header() is used directly for the synchronous
// syscall while this storage and its borrowed buffers are still alive.
struct Datagram<'a> {
    address: &'a mut raw::SockAddrNl,
    bytes: &'a mut [u8],
    vector: raw::IoVec,
}

impl<'a> Datagram<'a> {
    fn new(address: &'a mut raw::SockAddrNl, bytes: &'a mut [u8]) -> Self {
        let vector = raw::IoVec {
            base: bytes.as_mut_ptr().cast(),
            len: bytes.len(),
        };
        Self {
            address,
            bytes,
            vector,
        }
    }

    fn header(&mut self) -> raw::MsgHdr {
        self.vector.base = self.bytes.as_mut_ptr().cast();
        raw::MsgHdr {
            name: (self.address as *mut raw::SockAddrNl).cast(),
            name_len: size_of::<raw::SockAddrNl>() as u32,
            iov: &mut self.vector,
            iov_len: 1,
            control: ptr::null_mut(),
            control_len: 0,
            flags: 0,
        }
    }
}

fn parse_link(body: &[u8]) -> io::Result<LinkStatistics> {
    let (info, mut attributes) = body
        .split_at_checked(LINK_INFO)
        .ok_or(io::ErrorKind::InvalidData)?;
    let index = i32::from_ne_bytes(info[4..8].try_into().expect("ifi_index"));
    if index <= 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut name = None;
    let mut counters = None;
    while !attributes.is_empty() {
        let header = attributes
            .first_chunk::<4>()
            .ok_or(io::ErrorKind::InvalidData)?;
        let length = u16::from_ne_bytes([header[0], header[1]]) as usize;
        let value = attributes
            .get(4..length)
            .ok_or(io::ErrorKind::InvalidData)?;
        attributes = attributes
            .get(length.next_multiple_of(4)..)
            .ok_or(io::ErrorKind::InvalidData)?;
        match u16::from_ne_bytes([header[2], header[3]]) & 0x3fff {
            3 if name.is_none() => {
                let text = value
                    .strip_suffix(&[0])
                    .filter(|text| !text.is_empty() && text.len() < 16 && !text.contains(&0))
                    .ok_or(io::ErrorKind::InvalidData)?;
                let mut padded = [0; 16];
                padded[..text.len()].copy_from_slice(text);
                name = Some(padded);
            }
            23 if counters.is_none() => {
                let prefix = value
                    .first_chunk::<64>()
                    .ok_or(io::ErrorKind::InvalidData)?;
                let (fields, _) = prefix.as_chunks::<8>();
                counters = Some(std::array::from_fn(|index| {
                    u64::from_ne_bytes(fields[index])
                }));
            }
            3 | 23 => return Err(io::ErrorKind::InvalidData.into()),
            _ => {}
        }
    }
    Ok(LinkStatistics {
        index: index as u32,
        name: name.ok_or(io::ErrorKind::InvalidData)?,
        counters: counters.ok_or(io::ErrorKind::InvalidData)?,
    })
}
