//! 传输 helper：本 crate 唯一的 unsafe / extern 边界。
//!
//! 集中声明 socket 收发所需的 libc 接口（`write` / `close` / `sendmsg` /
//! `recvmsg` / `__errno_location`）与 64 位 musl 的 `msghdr` / `iovec` /
//! `cmsghdr` 布局（`size_t` = `usize`，`ssize_t` = `isize`，`socklen_t` = `u32`）。

use core::ffi::{c_int, c_void};

const SOL_SOCKET: c_int = 1;
const SCM_RIGHTS: c_int = 1;
/// 接收 SCM_RIGHTS fd 时同时置 `O_CLOEXEC`。
const MSG_CMSG_CLOEXEC: c_int = 0x4000_0000;
/// control buffer 不足、辅助数据被截断。
const MSG_CTRUNC: c_int = 0x8;
const EINTR: c_int = 4;
const EIO: c_int = 5;

/// `CMSG_LEN(4)`：cmsghdr(16) + 一个 i32 fd。
const CMSG_FD_LEN: usize = 20;
/// `CMSG_SPACE(4)`：对齐后的单 fd control 占用。
const CMSG_FD_SPACE: usize = 24;
/// 接收侧 control buffer 大小，对单个 fd 的 24 字节需求留有余量。
const RECV_CONTROL_LEN: usize = 32;

#[repr(C)]
struct IoVec {
    base: *mut c_void,
    len: usize,
}

#[repr(C)]
struct MsgHdr {
    name: *mut c_void,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: usize,
    control: *mut c_void,
    control_len: usize,
    flags: c_int,
}

#[repr(C)]
struct CmsgHdr {
    len: usize,
    level: c_int,
    kind: c_int,
}

/// 发送侧 control 布局：cmsghdr + 一个 fd + 对齐填充，正好 [`CMSG_FD_SPACE`] 字节。
#[repr(C)]
struct FdControl {
    header: CmsgHdr,
    fd: c_int,
    padding: c_int,
}

/// 接收侧 control buffer，按 usize 对齐以满足 cmsghdr 对齐要求。
#[repr(align(8))]
struct RecvControl([u8; RECV_CONTROL_LEN]);

const _: () = assert!(size_of::<MsgHdr>() == 56);
const _: () = assert!(size_of::<CmsgHdr>() == 16);
const _: () = assert!(size_of::<FdControl>() == CMSG_FD_SPACE);

unsafe extern "C" {
    fn write(fd: c_int, input: *const c_void, length: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn sendmsg(fd: c_int, message: *const MsgHdr, flags: c_int) -> isize;
    fn recvmsg(fd: c_int, message: *mut MsgHdr, flags: c_int) -> isize;
    fn __errno_location() -> *mut c_int;
}

fn errno() -> c_int {
    // SAFETY: musl 为调用线程暴露唯一 thread-local errno 指针。
    unsafe { *__errno_location() }
}

/// 把 `buf` 全量写入 `fd`（不带辅助数据）。
///
/// 循环处理短写，`EINTR` 时重试。
///
/// # 参数
///
/// - `fd`：已连接的 stream socket。
/// - `buf`：完整的消息帧（通常由消息类型的 `encode` 产生）。
///
/// # 返回值
///
/// 成功返回 `Ok(())`。
///
/// # 错误
///
/// `Err(-errno)`：写失败（`EINTR` 除外，已内部重试）；
/// `write` 返回 0（对端异常）按 `Err(-EIO)` 处理。
pub fn send_message(fd: i32, buf: &[u8]) -> Result<(), i32> {
    let mut sent = 0;
    while sent < buf.len() {
        // SAFETY: buf 切片在调用期间有效，write 只读取其中未发送部分。
        let n = unsafe { write(fd, buf[sent..].as_ptr().cast(), buf.len() - sent) };
        if n < 0 {
            let error = errno();
            if error == EINTR {
                continue;
            }
            return Err(-error);
        }
        if n == 0 {
            return Err(-EIO);
        }
        sent += n as usize;
    }
    Ok(())
}

/// 发送消息帧并随附一个 fd（`SCM_RIGHTS`），用于 [`crate::WELCOME`] 握手传递 DRM fd。
///
/// `EINTR` 时重试。消息不超过 [`crate::MAX_MESSAGE`]，远小于 socket 缓冲，
/// 短写只会在对端异常时出现，按错误处理。
///
/// # 参数
///
/// - `fd`：已连接的 stream socket。
/// - `buf`：完整的消息帧。
/// - `pass_fd`：要传递给对端的 fd；内核会为接收方 dup 一份。
///
/// # 返回值
///
/// 成功返回 `Ok(())`。
///
/// # 错误
///
/// `Err(-errno)`：sendmsg 失败；短写返回 `Err(-EIO)`。
pub fn send_message_with_fd(fd: i32, buf: &[u8], pass_fd: i32) -> Result<(), i32> {
    let mut control = FdControl {
        header: CmsgHdr {
            len: CMSG_FD_LEN,
            level: SOL_SOCKET,
            kind: SCM_RIGHTS,
        },
        fd: pass_fd,
        padding: 0,
    };
    let mut iov = IoVec {
        base: buf.as_ptr().cast::<c_void>().cast_mut(),
        len: buf.len(),
    };
    let msg = MsgHdr {
        name: core::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: (&raw mut control).cast(),
        control_len: CMSG_FD_SPACE,
        flags: 0,
    };
    loop {
        // SAFETY: msg 引用的 iov 与 control 在调用期间有效，sendmsg 只读不修改。
        let n = unsafe { sendmsg(fd, &raw const msg, 0) };
        if n < 0 {
            let error = errno();
            if error == EINTR {
                continue;
            }
            return Err(-error);
        }
        if n as usize != buf.len() {
            return Err(-EIO);
        }
        return Ok(());
    }
}

/// 从 `fd` 接收一条消息帧，可同时收回一个经 `SCM_RIGHTS` 传来的 fd。
///
/// 接收到的 fd 带 `O_CLOEXEC`（`MSG_CMSG_CLOEXEC`）。调用方使用非阻塞 fd +
/// poll 驱动，故 `EAGAIN` 原样以 `Err(-EAGAIN)` 返回。
///
/// # 参数
///
/// - `fd`：已连接的非阻塞 stream socket。
/// - `buf`：接收缓冲，应至少 [`crate::MAX_MESSAGE`] 字节；帧比缓冲长时超出的字节被截断
///   （流 socket 下不丢失，下次读取继续，但本条消息已不完整，调用方应视为协议错误）。
/// - `fd_out`：输出参数，若本条消息附带 `SCM_RIGHTS` fd 则置为 `Some(fd)`，
///   否则置为 `None`。
///
/// # 返回值
///
/// `Ok(n)` 为收到的帧字节数；`Ok(0)` 表示对端有序关闭连接。
///
/// # 错误
///
/// `Err(-errno)`：recvmsg 失败（含 `EAGAIN`）。control 数据被截断（`MSG_CTRUNC`）时，
/// 已收到的 fd 会被立即 close 防止泄漏，并返回 `Err(-EIO)`。
///
/// # SCM_RIGHTS barrier 拆分
///
/// 内核把 fd 附着在写入的首个字节上：携带 fd 的 N 字节写入会以「1 字节 + fd」和
/// 「剩余 N-1 字节」两次到达（barrier 保证一次 read 不跨越附着点）。面向完整帧的
/// 调用方必须使用 [`recv_frame_blocking`] 或自行拼帧，不能把一次 `recv_message`
/// 当作一条完整消息。
pub fn recv_message(fd: i32, buf: &mut [u8], fd_out: &mut Option<i32>) -> Result<usize, i32> {
    *fd_out = None;
    let mut iov = IoVec {
        base: buf.as_mut_ptr().cast(),
        len: buf.len(),
    };
    let mut control = RecvControl([0u8; RECV_CONTROL_LEN]);
    let mut msg = MsgHdr {
        name: core::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: control.0.as_mut_ptr().cast(),
        control_len: RECV_CONTROL_LEN,
        flags: 0,
    };
    // SAFETY: msg 引用的 iov、buf 与 control 在调用期间有效且可写；
    // recvmsg 按 control_len 上限写入并回写实际长度与 flags。
    let n = unsafe { recvmsg(fd, &raw mut msg, MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(-errno());
    }
    if msg.control_len >= CMSG_FD_LEN {
        let cmsg_len = usize::from_ne_bytes(
            control.0[0..8]
                .try_into()
                .expect("RecvControl 至少 8 字节"),
        );
        let level =
            i32::from_ne_bytes(control.0[8..12].try_into().expect("RecvControl 至少 12 字节"));
        let kind = i32::from_ne_bytes(
            control.0[12..16]
                .try_into()
                .expect("RecvControl 至少 16 字节"),
        );
        if cmsg_len >= CMSG_FD_LEN && level == SOL_SOCKET && kind == SCM_RIGHTS {
            *fd_out = Some(i32::from_ne_bytes(
                control.0[16..20]
                    .try_into()
                    .expect("RecvControl 至少 20 字节"),
            ));
        }
    }
    if msg.flags & MSG_CTRUNC != 0 {
        if let Some(received) = fd_out.take() {
            // SAFETY: received 是内核刚通过 SCM_RIGHTS 安装的有效 fd；
            // 控制数据已截断、协议帧不可信，立即关闭防止泄漏。
            let _ = unsafe { close(received) };
        }
        return Err(-EIO);
    }
    Ok(n as usize)
}

/// 阻塞接收一条完整帧，吸收 SCM_RIGHTS barrier 拆分（见 [`recv_message`] 文档）。
///
/// 循环调用 [`recv_message`] 拼接字节流，直到 [`crate::parse_header`] 能切出一条
/// 完整帧为止；任一 chunk 携带的 fd 汇总进 `fd_out`（协议上只有 [`crate::WELCOME`]
/// 携带 fd；收到多个 fd 时后者立即 close 防泄漏）。
///
/// # 参数
///
/// - `fd`：已连接的**阻塞** stream socket。
/// - `buf`：帧缓冲，至少 [`crate::MAX_MESSAGE`] 字节。
/// - `fd_out`：输出参数，收到 `SCM_RIGHTS` fd 时置为 `Some(fd)`。
///
/// # 返回值
///
/// `Ok(n)` 为完整帧长度（帧内容在 `buf[..n]`）；`Ok(0)` 表示对端有序关闭且此前
/// 无任何未拼完的字节。
///
/// # 错误
///
/// `Err(-errno)`：recvmsg 失败；帧声明长度超 [`crate::MAX_MESSAGE`]、缓冲不足或
/// 连接中途断开（半截帧后 EOF）返回 `Err(-EIO)`。
pub fn recv_frame_blocking(fd: i32, buf: &mut [u8], fd_out: &mut Option<i32>) -> Result<usize, i32> {
    *fd_out = None;
    let mut filled = 0usize;
    loop {
        // 已收字节是否已构成完整帧：先凑齐头部，再等 header.len 声明的全部字节。
        if filled >= crate::HEADER_LEN
            && let Some((header, _)) = crate::parse_header(&buf[..filled])
        {
            return Ok(header.len as usize);
        }
        if filled == buf.len() {
            // 缓冲已满仍无法成帧：对端不遵守 MAX_MESSAGE 上限，协议错误。
            return Err(-EIO);
        }
        let mut chunk_fd = None;
        let n = match recv_message(fd, &mut buf[filled..], &mut chunk_fd) {
            Ok(n) => n,
            Err(error) if error == -EINTR => continue,
            Err(error) => return Err(error),
        };
        if let Some(received) = chunk_fd
            && let Some(existing) = fd_out.replace(received)
        {
            // SAFETY: existing 是此前 chunk 安装的有效 fd；协议不允许一帧多 fd。
            let _ = unsafe { close(existing) };
        }
        if n == 0 {
            // EOF：干净断开（无半截帧）返回 0，否则协议错误。
            return if filled == 0 { Ok(0) } else { Err(-EIO) };
        }
        filled += n;
    }
}
