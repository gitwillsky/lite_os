//! Linux 路由 netlink 的只读 dump 会话。

use super::{LinkAddress, LinkStatistics};
use crate::{
    raw,
    unix::{PollEvents, PollFd, poll},
};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    ptr,
    time::{Duration, Instant},
};

const AF_NETLINK: u16 = 16;
const HEADER: usize = 16;
const LINK_INFO: usize = 16;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const ADDRESS_INFO: usize = 8;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const NLM_F_DUMP_INTR: u16 = 0x10;
const MSG_TRUNC: i32 = 0x20;

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
        self.dump(RTM_GETLINK, RTM_NEWLINK, &[0; LINK_INFO], timeout, |body| {
            visit(parse_link(body)?);
            Ok(())
        })
    }

    /// 遍历一次完整的双栈 GETADDR 回复。调用方须在成功返回后发布采样。
    /// # Errors
    /// 与 `read` 相同；无法解析地址、前缀或寿命时拒绝整个采样。
    pub fn read_addresses(
        &mut self,
        timeout: Duration,
        mut visit: impl FnMut(LinkAddress),
    ) -> io::Result<()> {
        self.dump(
            RTM_GETADDR,
            RTM_NEWADDR,
            &[0; ADDRESS_INFO],
            timeout,
            |body| {
                visit(parse_address(body)?);
                Ok(())
            },
        )
    }

    fn dump(
        &mut self,
        request_kind: u16,
        reply_kind: u16,
        payload: &[u8],
        timeout: Duration,
        mut visit: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        self.sequence = self.sequence.wrapping_add(1);
        let mut storage = [0; HEADER + LINK_INFO];
        let length = HEADER + payload.len();
        let request = &mut storage[..length];
        request[..4].copy_from_slice(&(length as u32).to_ne_bytes());
        request[4..6].copy_from_slice(&request_kind.to_ne_bytes());
        request[HEADER..].copy_from_slice(payload);
        // NLM_F_REQUEST | NLM_F_DUMP.
        request[6..8].copy_from_slice(&0x301u16.to_ne_bytes());
        request[8..12].copy_from_slice(&self.sequence.to_ne_bytes());
        let mut kernel = raw::SockAddrNl {
            family: AF_NETLINK,
            padding: 0,
            port_id: 0,
            groups: 0,
        };
        let mut message = Datagram::new(&mut kernel, request);
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
                    kind if kind == reply_kind => visit(body)?,
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

fn attribute<'a>(bytes: &mut &'a [u8]) -> io::Result<(u16, &'a [u8])> {
    let header = bytes.first_chunk::<4>().ok_or(io::ErrorKind::InvalidData)?;
    let length = u16::from_ne_bytes([header[0], header[1]]) as usize;
    let value = bytes.get(4..length).ok_or(io::ErrorKind::InvalidData)?;
    *bytes = bytes
        .get(length.next_multiple_of(4)..)
        .ok_or(io::ErrorKind::InvalidData)?;
    Ok((u16::from_ne_bytes([header[2], header[3]]) & 0x3fff, value))
}

fn parse_address(body: &[u8]) -> io::Result<LinkAddress> {
    let (info, mut attributes) = body
        .split_at_checked(ADDRESS_INFO)
        .ok_or(io::ErrorKind::InvalidData)?;
    let family = info[0];
    let prefix_length = info[1];
    let index = u32::from_ne_bytes(info[4..8].try_into().expect("ifa_index"));
    let limit = match family {
        AF_INET => 32,
        AF_INET6 => 128,
        _ => return Err(io::ErrorKind::InvalidData.into()),
    };
    if index == 0 || prefix_length > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut address = None;
    let mut local = None;
    let mut lifetimes = None;
    let mut flags = None;
    let decode_address = |value: &[u8]| -> io::Result<IpAddr> {
        let invalid = |_| io::ErrorKind::InvalidData;
        Ok(if family == AF_INET {
            Ipv4Addr::from(<[u8; 4]>::try_from(value).map_err(invalid)?).into()
        } else {
            Ipv6Addr::from(<[u8; 16]>::try_from(value).map_err(invalid)?).into()
        })
    };
    while !attributes.is_empty() {
        let (kind, value) = attribute(&mut attributes)?;
        match kind {
            1 if address.is_none() => address = Some(decode_address(value)?),
            2 if local.is_none() => local = Some(decode_address(value)?),
            6 if lifetimes.is_none() => {
                let info = <&[u8; 16]>::try_from(value).map_err(|_| io::ErrorKind::InvalidData)?;
                let (fields, _) = info.as_chunks::<4>();
                lifetimes = Some((u32::from_ne_bytes(fields[0]), u32::from_ne_bytes(fields[1])));
            }
            8 if flags.is_none() => {
                flags = Some(u32::from_ne_bytes(
                    value.try_into().map_err(|_| io::ErrorKind::InvalidData)?,
                ));
            }
            1 | 2 | 6 | 8 => return Err(io::ErrorKind::InvalidData.into()),
            _ => {}
        }
    }
    Ok(LinkAddress {
        index,
        address: local.or(address).ok_or(io::ErrorKind::InvalidData)?,
        prefix_length,
        flags: flags.unwrap_or_else(|| u32::from(info[2])),
        lifetimes,
    })
}

fn parse_link(body: &[u8]) -> io::Result<LinkStatistics> {
    let (info, mut attributes) = body
        .split_at_checked(LINK_INFO)
        .ok_or(io::ErrorKind::InvalidData)?;
    let index = i32::from_ne_bytes(info[4..8].try_into().expect("ifi_index"));
    if index <= 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let ethernet = u16::from_ne_bytes(info[2..4].try_into().expect("ifi_type")) == 1;
    let flags = u32::from_ne_bytes(info[8..12].try_into().expect("ifi_flags"));
    let mut name = None;
    let mut ethernet_address = None;
    let mut counters = None;
    while !attributes.is_empty() {
        let (kind, value) = attribute(&mut attributes)?;
        match kind {
            1 if ethernet => {
                if ethernet_address.is_some() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                ethernet_address = Some(value.try_into().map_err(|_| io::ErrorKind::InvalidData)?);
            }
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
        ethernet_address,
        up: flags & 1 != 0,
        counters: counters.ok_or(io::ErrorKind::InvalidData)?,
    })
}
