//! LiteOS 桌面显示协议定义。
//!
//! 本 crate 定义 `user/desktop`（合成器，服务端）与显示客户端（如 `user/terminal`）
//! 之间的 Unix domain socket 协议：消息帧布局、逐字段编解码与传输 helper。
//! 服务端与客户端以 path 依赖共用本 crate，保证双方对 wire 格式只有一份定义。
//!
//! # 传输
//!
//! - 服务端监听 `AF_UNIX` `SOCK_STREAM` socket [`SOCKET_PATH`]，每个客户端一条连接。
//! - 每条消息为 [`Header`] `{ len: u32, kind: u32 }` + payload，`len` 含头部自身，
//!   单条消息不超过 [`MAX_MESSAGE`] 字节。所有整数字段小端编码。
//! - 握手时服务端通过 `SCM_RIGHTS` 把 `/dev/dri/card0` 的 dup fd 随 [`WELCOME`] 发给客户端；
//!   双方共享同一 OFD / GEM handle namespace。
//!
//! # 握手序列
//!
//! 1. C→S [`HELLO`]：客户端报告协议版本 [`PROTOCOL_VERSION`]。
//! 2. S→C [`WELCOME`] + `SCM_RIGHTS` 附带 DRM fd：服务端确认版本。
//! 3. C→S [`CREATE_SURFACE`]：携带尺寸、GEM handle 与标题；
//!    S→C [`SURFACE_CREATED`] 应答 surface id 或 errno。
//! 4. 之后进入事件循环：C→S [`COMMIT`] 推送 damage，S→C [`INPUT_KEY`] /
//!    [`INPUT_POINTER`] / [`FOCUS`] / [`CLOSE_REQUEST`] 等投递事件。
//!
//! # buffer 所有权
//!
//! 客户端在共享 DRM fd 上 `CREATE_DUMB` 创建 GEM buffer，handle 随 [`CREATE_SURFACE`]
//! 提及时**转移给桌面**：桌面负责最终 `DESTROY_DUMB`，客户端此后绝不销毁该 handle。
//! 桌面通过 `MAP_DUMB` + `mmap` 读取像素合成。damage clip 为 `{x1, y1, x2, y2}`
//! 半开矩形，单条 [`COMMIT`] 上限 [`MAX_DAMAGE_RECTS`]（内核 `DIRTYFB` 限制）。
//!
//! # 连接生命周期
//!
//! socket 断开即视为客户端退出：桌面销毁该客户端的全部 surface 及其 GEM handle。
//!
//! # unsafe 边界
//!
//! 编解码全部逐字段进行（`to_le_bytes` / `from_le_bytes`），不依赖宿主结构体布局，
//! 不含任何 unsafe。唯一的 unsafe / extern 集中在 [`transport`] 模块
//! （[`send_message`] / [`send_message_with_fd`] / [`recv_message`]）。

#![no_std]

mod message;
mod transport;

pub use message::{
    CloseRequest, Commit, CommitRects, Configure, CreateSurface, DestroySurface, Focus, Hello,
    InputKey, InputPointer, InputSyncReset, SetBuffer, SetTitle, SurfaceCreated, Welcome,
};
pub use transport::{recv_frame_blocking, recv_message, send_message, send_message_with_fd};

/// 协议版本。握手时客户端在 [`Hello`] 中报告，服务端在 [`Welcome`] 中确认；
/// 双方版本不一致时应立即断开连接。
pub const PROTOCOL_VERSION: u32 = 1;

/// 服务端监听的 Unix domain socket 路径。
pub const SOCKET_PATH: &str = "/run/display.sock";

/// 单条消息（含 [`Header`]）的最大字节数。编码超过此上限或接收帧声明超过
/// 此上限均视为协议错误。
pub const MAX_MESSAGE: usize = 4096;

/// 单条 [`COMMIT`] 允许携带的 damage clip 上限，与内核 `DIRTYFB` 的 clip 上限一致。
pub const MAX_DAMAGE_RECTS: usize = 32;

/// 消息头长度（`len` + `kind`）。
pub const HEADER_LEN: usize = 8;

/// `HELLO`（C→S）：握手请求，payload 为 [`Hello`]。
pub const HELLO: u32 = 1;
/// `WELCOME`（S→C）：握手应答，payload 为 [`Welcome`]，经 `SCM_RIGHTS` 附带 DRM fd。
pub const WELCOME: u32 = 2;
/// `CREATE_SURFACE`（C→S）：创建 surface，payload 为 [`CreateSurface`] + title 字节。
pub const CREATE_SURFACE: u32 = 3;
/// `SURFACE_CREATED`（S→C）：创建应答，payload 为 [`SurfaceCreated`]。
pub const SURFACE_CREATED: u32 = 4;
/// `COMMIT`（C→S）：提交 damage，payload 为 [`Commit`] + `num_rects` 个 `[u16; 4]`。
pub const COMMIT: u32 = 5;
/// `INPUT_KEY`（S→C）：按键事件，payload 为 [`InputKey`]。
pub const INPUT_KEY: u32 = 6;
/// `INPUT_POINTER`（S→C）：指针事件，payload 为 [`InputPointer`]。
pub const INPUT_POINTER: u32 = 7;
/// `FOCUS`（S→C）：焦点变化，payload 为 [`Focus`]。
pub const FOCUS: u32 = 8;
/// `CLOSE_REQUEST`（S→C）：请求关闭，payload 为 [`CloseRequest`]。
pub const CLOSE_REQUEST: u32 = 9;
/// `SET_TITLE`（C→S）：更新标题，payload 为 [`SetTitle`] + title 字节。
pub const SET_TITLE: u32 = 10;
/// `DESTROY_SURFACE`（C→S）：销毁 surface，payload 为 [`DestroySurface`]。
pub const DESTROY_SURFACE: u32 = 11;
/// `CONFIGURE`（S→C）：建议尺寸，payload 为 [`Configure`]。
pub const CONFIGURE: u32 = 12;
/// `INPUT_SYNC_RESET`（S→C）：输入状态重置，payload 为 [`InputSyncReset`]。
pub const INPUT_SYNC_RESET: u32 = 13;
/// `SET_BUFFER`（C→S）：替换 surface 的 backing buffer，payload 为 [`SetBuffer`]。
///
/// 客户端响应 [`CONFIGURE`]（或自行改变内容尺寸）时创建新 dumb buffer 并经本消息
/// 提交：新 handle 所有权随消息转移给桌面，桌面完成切换后 unmap 并 `DESTROY_DUMB`
/// 旧 handle，窗口内容尺寸以本消息的 `width`/`height` 为准（客户端可按自身网格
/// 对齐，不必等于 CONFIGURE 的请求值）。切换后的首个 [`COMMIT`] 针对新 buffer。
pub const SET_BUFFER: u32 = 14;

/// [`CreateSurface::flags`] 位：surface 不带装饰（无边框 / 标题栏）。
pub const SURFACE_FLAG_UNDECORATED: u32 = 1;

/// 消息头：所有字段小端编码。
///
/// 仅描述 wire 布局；编解码走 [`parse_header`] 与各消息的 `encode`，
/// 不直接按内存布局收发。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Header {
    /// 帧总长度（含头部自身 8 字节）。
    pub len: u32,
    /// 消息种类，取值为 [`HELLO`] 等常量之一。
    pub kind: u32,
}

/// 从接收到的字节流中解析消息头，返回头部与等长 payload 切片。
///
/// `frame` 至少为一条完整帧；`len` 小于 [`HEADER_LEN`]、大于 [`MAX_MESSAGE`]
/// 或超过 `frame` 实际长度时返回 `None`。
pub fn parse_header(frame: &[u8]) -> Option<(Header, &[u8])> {
    if frame.len() < HEADER_LEN {
        return None;
    }
    let header = Header {
        len: read_u32(frame, 0)?,
        kind: read_u32(frame, 4)?,
    };
    let len = header.len as usize;
    if !(HEADER_LEN..=MAX_MESSAGE).contains(&len) || len > frame.len() {
        return None;
    }
    Some((header, &frame[HEADER_LEN..len]))
}

const _: () = assert!(size_of::<Header>() == 8);

pub(crate) fn read_u32(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes = buf.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

pub(crate) fn read_i32(buf: &[u8], offset: usize) -> Option<i32> {
    Some(read_u32(buf, offset)? as i32)
}

pub(crate) fn write_u32(buf: &mut [u8], offset: usize, value: u32) -> Option<()> {
    buf.get_mut(offset..offset.checked_add(4)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

pub(crate) fn write_i32(buf: &mut [u8], offset: usize, value: i32) -> Option<()> {
    write_u32(buf, offset, value as u32)
}

pub(crate) fn write_u16(buf: &mut [u8], offset: usize, value: u16) -> Option<()> {
    buf.get_mut(offset..offset.checked_add(2)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// 写入 header 并检查容量与 [`MAX_MESSAGE`] 上限，返回帧总长度。
pub(crate) fn encode_frame(buf: &mut [u8], kind: u32, payload_len: usize) -> Option<usize> {
    let total = HEADER_LEN.checked_add(payload_len)?;
    if total > MAX_MESSAGE || buf.len() < total {
        return None;
    }
    write_u32(buf, 0, total as u32)?;
    write_u32(buf, 4, kind)?;
    Some(total)
}
