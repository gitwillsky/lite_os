//! LiteOS graphical-session wire protocol.
//!
//! `compositor`、desktop-mode LiteUI 与 app-mode LiteUI 通过同一 AF_UNIX stream 使用本协议。
//! 协议只描述 flat scene、GPU display list、surface 与输入 mechanism；窗口 policy、React、CSS 与主题不进入此 seam。

mod accelerator;
mod clipboard;
mod codec;
mod geometry;
mod handshake;
mod input;
mod lifecycle;
mod paint;
mod scene;
mod surface;
mod transport;

pub use accelerator::{AcceleratorChord, AcceleratorChordIterator, AcceleratorSet};
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
pub use paint::{
    BorderStyle, DisplayCommand, DisplayCommands, DisplayListCommit, DisplayListWriter, Glyph,
    Glyphs, GradientStop, GradientStops, ImageRepeat, ImageSampling, TextureCreate, TextureDestroy,
    TextureFormat, TexturePublish, TextureRect, TextureWrite,
};
pub use scene::{
    ClipMask, ClipMasks, CornerRadius, Rectangles, SceneCommit, SceneNode, SceneNodeKind,
    SceneNodes,
};
pub use surface::{Accepted, Configure, ConfigureReady, Discarded, OutputConfigure, Presented};
pub use transport::{recv_frame_blocking, recv_message_owned, send_message};

/// The only supported protocol version; no negotiation or compat decoder.
pub const PROTOCOL_VERSION: u32 = 12;

/// compositor 监听的唯一 socket path。
pub const SOCKET_PATH: &str = "/run/display.sock";

/// frame header 字节数：`len: u32` 与 `kind: u32`。
pub const HEADER_LEN: usize = 8;

/// Maximum GPU paint operations in one immutable display list.
pub const MAX_DISPLAY_COMMANDS: usize = 2048;

/// Maximum glyph quads carried by one display-list command.
pub const MAX_GLYPHS_PER_RUN: usize = 256;

/// Maximum size of control, scene and staged-transfer frames.
pub const MAX_CONTROL_MESSAGE: usize = 64 * 1024;

/// Maximum size of one complete protocol frame.
///
/// A glyph run is the largest command: seven scalar fields followed by two
/// rectangles per glyph. Deriving this bound from the public command quotas is
/// required so a structurally valid display list cannot fail only at outer
/// framing when a dense terminal or document fills the viewport.
pub const MAX_MESSAGE: usize =
    HEADER_LEN + 3 * 8 + 16 + 4 + MAX_DISPLAY_COMMANDS * (7 * 4 + MAX_GLYPHS_PER_RUN * 2 * 16);

/// 一个 session 可同时存在的普通 app surface 上限。
pub const MAX_APP_SURFACES: usize = 32;

/// 一份完整 desktop scene 的 node 上限。
pub const MAX_SCENE_NODES: usize = 128;

/// 一份 scene 中全部 input rectangle 的总上限。
pub const MAX_INPUT_RECTS: usize = 256;

/// 单个 scene node 的 input rectangle 上限。
pub const MAX_NODE_INPUT_RECTS: usize = 64;

/// 单个 scene node 的 CSS rounded clip mask 上限。
pub const MAX_NODE_CLIP_MASKS: usize = 16;

/// 单个 scene node 允许的 damage rectangle 上限。
pub const MAX_NODE_DAMAGE_RECTS: usize = 64;

/// Maximum nested CSS clip or opacity groups in one display list.
pub const MAX_DISPLAY_STACK_DEPTH: usize = 16;

/// Maximum color stops in one CSS linear gradient primitive.
pub const MAX_GRADIENT_STOPS: usize = 16;

/// Maximum immutable textures owned by one display connection.
pub const MAX_CLIENT_TEXTURES: usize = 256;

/// Maximum chords in one desktop-submitted global accelerator table.
pub const MAX_ACCELERATORS: usize = 16;

/// 逻辑 CSS pixel 到物理 pixel 的固定比例。
pub const DEVICE_SCALE_FACTOR: u32 = 2;

/// Returns the finite pixel support used by the compositor's CSS blur kernel.
///
/// # Parameters
///
/// - `radius`: Non-negative physical-pixel CSS blur radius.
///
/// # Returns
///
/// Three standard deviations for the shared `sigma = radius / 2` kernel. Paint
/// damage and GPU sampling must use this same extent; a shorter damage bound
/// clips the visible falloff, while a longer one repaints unrelated content.
pub fn blur_support(radius: f32) -> f32 {
    radius.max(0.0) * 1.5
}

#[cfg(test)]
mod tests {
    #[test]
    fn blur_support_covers_three_sigma() {
        assert_eq!(super::blur_support(24.0), 36.0);
        assert_eq!(super::blur_support(-1.0), 0.0);
    }
}
