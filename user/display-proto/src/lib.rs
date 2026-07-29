//! LiteOS graphical-session wire protocol.
//!
//! `compositor`、desktop-mode LiteUI 与 app-mode LiteUI 通过同一 AF_UNIX stream 使用本协议。
//! 协议只描述 flat scene、surface、buffer 与输入 mechanism；窗口 policy、React、CSS 与主题不进入此 seam。

mod accelerator;
mod buffer;
mod clipboard;
mod codec;
mod geometry;
mod handshake;
mod input;
mod lifecycle;
mod scene;
mod surface;
mod transport;

pub use accelerator::{AcceleratorChord, AcceleratorChordIterator, AcceleratorSet};
pub use buffer::{BufferAlloc, BufferAllocated, BufferDescriptor, BufferRelease};
pub use clipboard::{ClipboardData, ClipboardRead, ClipboardWrite, MAX_CLIPBOARD_TEXT};
pub use codec::{Frame, FrameWriter, MessageKind, parse_frame};
pub use geometry::{Rect, Size};
pub use handshake::{HelloApp, HelloDesktop, Welcome};
pub use input::{
    CURSOR_DEFAULT, CURSOR_NONE, CURSOR_POINTER, CURSOR_RESIZE_EW, CURSOR_RESIZE_NESW,
    CURSOR_RESIZE_NS, CURSOR_RESIZE_NWSE, InputKey, InputPointer, InputScroll, PointerPhase,
    SetCursorShape,
};
pub use lifecycle::{
    AppClosed, AppOpened, CloseRequest, MoveBegin, MoveComplete, SurfaceActivated,
};
pub use scene::{Rectangles, SceneCommit, SceneNode, SceneNodeKind, SceneNodes};
pub use surface::{
    Accepted, Configure, ConfigureReady, DamageRectangles, Presented, SurfaceCommit,
};
pub use transport::{recv_frame_blocking, recv_message, send_message, send_message_with_fd};

/// The only supported protocol version; no negotiation or compat decoder.
pub const PROTOCOL_VERSION: u32 = 7;

/// compositor 监听的唯一 socket path。
pub const SOCKET_PATH: &str = "/run/display.sock";

/// frame header 字节数：`len: u32` 与 `kind: u32`。
pub const HEADER_LEN: usize = 8;

/// 单条完整 frame 的最大尺寸。
pub const MAX_MESSAGE: usize = 64 * 1024;

/// 一个 session 可同时存在的普通 app surface 上限。
pub const MAX_APP_SURFACES: usize = 32;

/// 一份完整 desktop scene 的 node 上限。
pub const MAX_SCENE_NODES: usize = 128;

/// 一份 scene 中全部 input rectangle 的总上限。
pub const MAX_INPUT_RECTS: usize = 256;

/// 单个 scene node 的 input rectangle 上限。
pub const MAX_NODE_INPUT_RECTS: usize = 64;

/// 单次像素提交允许的 damage rectangle 上限。
pub const MAX_DAMAGE_RECTS: usize = 64;

/// 每个 connection 最多持有的 full-frame equivalent 数量。
pub const MAX_CONNECTION_FRAME_EQUIVALENTS: u64 = 4;

/// 整个 session 最多持有的 client full-frame equivalent 数量。
///
/// 16 容纳 desktop triple buffering、默认 Files/Terminal 与八个当前尺寸的
/// 双缓冲 Aurora 窗口；每连接的独立上限仍阻止单个 client 吞掉 session 配额。
pub const MAX_SESSION_FRAME_EQUIVALENTS: u64 = 16;

/// Maximum chords in one desktop-submitted global accelerator table.
pub const MAX_ACCELERATORS: usize = 16;

/// 逻辑 CSS pixel 到物理 pixel 的固定比例。
pub const DEVICE_SCALE_FACTOR: u32 = 2;
