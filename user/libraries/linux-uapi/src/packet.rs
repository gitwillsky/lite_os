//! AF_PACKET 的按接口接收、纳秒时间戳和丢弃计数。

use crate::raw;
use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
};

const AF_PACKET: u16 = 17;
const SOL_PACKET: i32 = 263;
const SO_TIMESTAMPNS_NEW: i32 = 64;
const PACKET_STATISTICS: i32 = 6;
const SO_ERROR: i32 = 4;

#[repr(C)]
#[derive(Default)]
struct SockaddrLl {
    family: u16,
    protocol: u16,
    index: i32,
    hardware: u16,
    kind: u8,
    address_length: u8,
    address: [u8; 8],
}

const _: () = assert!(size_of::<SockaddrLl>() == 20);

/// 一次成功接收的原始链路帧信息。
pub struct Frame {
    pub full_length: usize,
    pub seconds: u64,
    pub nanoseconds: u32,
    /// 此帧是接口发送方向的镜像。
    pub outgoing: bool,
    /// Linux loopback 会同时交付发送与接收副本。
    pub loopback_outgoing: bool,
}

/// 独占的 Linux 接收描述符。读取保持非阻塞。
pub struct Receiver {
    fd: OwnedFd,
    index: i32,
}

impl Receiver {
    /// 绑定一个 Ethernet 或 loopback 接口并请求纳秒时间戳。
    /// # Errors
    /// 缺少 CAP_NET_RAW、接口无效或系统调用失败。
    pub fn open(index: u32) -> io::Result<Self> {
        let index = i32::try_from(index)
            .ok()
            .filter(|&index| index != 0)
            .ok_or(io::ErrorKind::InvalidInput)?;
        const SOCK_RAW: i32 = 3;
        const ETH_P_ALL: u16 = 3;
        // SAFETY: 创建一个独占 Linux packet socket，protocol 0 在 bind 前不会收到报文。
        let fd = unsafe {
            raw::socket(
                i32::from(AF_PACKET),
                SOCK_RAW | raw::O_NONBLOCK | raw::O_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd 是上一次成功创建且还未被接管的描述符。
        let receiver = Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            index,
        };
        let timestamp = 1i32;
        let address = SockaddrLl {
            family: AF_PACKET,
            protocol: ETH_P_ALL.to_be(),
            index,
            ..SockaddrLl::default()
        };
        // SAFETY: 两个操作只同步读取相应的 LP64 Linux 参数，描述符由 receiver 保活。
        if unsafe {
            raw::setsockopt(
                fd,
                raw::SOL_SOCKET,
                SO_TIMESTAMPNS_NEW,
                (&raw const timestamp).cast(),
                4,
            )
        } < 0
            || unsafe {
                raw::bind(
                    fd,
                    (&raw const address).cast(),
                    size_of::<SockaddrLl>() as u32,
                )
            } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(receiver)
    }

    /// 读取一帧。仅复制缓冲容纳的前缀，返回该帧的原长。
    /// # Errors
    /// 空队列返回 WouldBlock。无效地址、时间戳或其他接收故障均返回错误。
    pub fn receive(&self, bytes: &mut [u8]) -> io::Result<Frame> {
        let mut address = SockaddrLl::default();
        let mut timestamp = [0u64; 4];
        let mut vector = raw::IoVec {
            base: bytes.as_mut_ptr().cast(),
            len: bytes.len(),
        };
        let mut message = raw::MsgHdr {
            name: (&raw mut address).cast(),
            name_len: size_of::<SockaddrLl>() as u32,
            iov: &mut vector,
            iov_len: 1,
            control: timestamp.as_mut_ptr().cast(),
            control_len: size_of_val(&timestamp),
            flags: 0,
        };
        // SAFETY: iovec、sockaddr、对齐的 control 均是本次调用独占的存储。
        let received = unsafe { raw::recvmsg(self.fd.as_raw_fd(), &mut message, raw::MSG_TRUNC) };
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        let [length, kind_and_level, seconds, nanoseconds] = timestamp;
        let ancillary = (SO_TIMESTAMPNS_NEW as u64) << 32 | raw::SOL_SOCKET as u64;
        if received < 14
            || message.name_len != size_of::<SockaddrLl>() as u32
            || address.family != AF_PACKET
            || address.index != self.index
            || !matches!(address.hardware, 1 | 772)
            || message.flags & raw::MSG_CTRUNC != 0
            || message.control_len != 32
            || length != 32
            || kind_and_level != ancillary
            || nanoseconds >= 1_000_000_000
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Frame {
            full_length: received as usize,
            seconds,
            nanoseconds: nanoseconds as u32,
            outgoing: address.kind == 4,
            loopback_outgoing: address.hardware == 772 && address.kind == 4,
        })
    }

    /// 返回并清零匹配总数和其中的丢弃数。
    /// # Errors
    /// 查询失败或结果布局不符。
    pub fn take_statistics(&self) -> io::Result<[u32; 2]> {
        self.option(SOL_PACKET, PACKET_STATISTICS)
    }

    /// 消费接口停用或移除产生的异步错误。
    /// # Errors
    /// 查询失败，或存在异步错误。
    pub fn check_error(&self) -> io::Result<()> {
        let [error] = self.option(raw::SOL_SOCKET, SO_ERROR)?;
        if error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(error as i32))
        }
    }

    fn option<const N: usize>(&self, level: i32, option: i32) -> io::Result<[u32; N]> {
        let mut values = [0; N];
        let mut length = size_of_val(&values) as u32;
        // SAFETY: 参数指向本调用独占的结果及长度，并由 self 保活 socket。
        if unsafe {
            raw::getsockopt(
                self.fd.as_raw_fd(),
                level,
                option,
                values.as_mut_ptr().cast(),
                &mut length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if length as usize != size_of_val(&values) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(values)
    }
}

impl AsFd for Receiver {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
