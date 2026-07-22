//! LiteOS graphical-session wire protocol.
//!
//! `compositor`、desktop-mode LiteUI 与 app-mode LiteUI 通过同一 AF_UNIX stream 使用本协议。
//! 协议只描述 flat scene、surface、buffer 与输入 mechanism；窗口 policy、React、CSS 与主题不进入此 seam。

mod buffer;
mod codec;
mod geometry;
mod handshake;
mod input;
mod lifecycle;
mod scene;
mod surface;
mod transport;

pub use buffer::{BufferAlloc, BufferAllocated, BufferDescriptor, BufferRelease};
pub use codec::{Frame, FrameWriter, MessageKind, parse_frame};
pub use geometry::{Rect, Size};
pub use handshake::{HelloApp, HelloDesktop, Welcome};
pub use input::{InputKey, InputPointer, PointerPhase};
pub use lifecycle::{AppClosed, AppOpened, CloseRequest};
pub use scene::{Rectangles, SceneCommit, SceneNode, SceneNodeKind, SceneNodes};
pub use surface::{
    Accepted, Configure, ConfigureReady, DamageRectangles, Presented, SurfaceCommit,
};
pub use transport::{recv_frame_blocking, recv_message, send_message, send_message_with_fd};

/// 唯一受支持的协议版本；不提供版本协商或兼容 decoder。
pub const PROTOCOL_VERSION: u32 = 2;

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
pub const MAX_SESSION_FRAME_EQUIVALENTS: u64 = 8;

/// 逻辑 CSS pixel 到物理 pixel 的固定比例。
pub const DEVICE_SCALE_FACTOR: u32 = 2;
