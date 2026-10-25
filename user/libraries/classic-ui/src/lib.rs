//! 经典控件的绘制、Wayland 窗口与输入：把 `classic-theme` 的事实、`typeface` 的字形和 `canvas` 的光栅组合成 Win2000 外观。
//!
//! 所有几何以 96 DPI 的逻辑像素给出，[`Painter`] 在落笔时乘以整数 scale。文字宽度依赖字体，
//! 以物理像素返回，需要排版时用 [`Painter::text_width`] 度量。

extern crate liteos_runtime as _;
mod app_icons;
mod bitmap;
pub use app_icons::AppIcons;
pub use bitmap::Bitmap;
mod column_header;
mod controls;
mod decoration;
mod dropdown;
mod editing;
mod menu;
mod menubar;
mod painter;
mod prompt;
mod resources;
mod window_host;
pub use resources::Resources;
pub use window_host::{HostEvent, WindowHost};
mod scroll;
mod tooltip;
mod trackbar;
mod wayland;
mod window;

pub use column_header::ColumnHeader;
pub use decoration::{Decoration, DecorationAction};
pub use dropdown::{DropItem, DropMenu, Popup, title_rects};
pub use editing::{Edit, FocusChain};
pub use wayland::{
    Configuration, CursorShape, Display, Event, EventKind, Key, KeyboardInteractivity, Layer,
    LayerSettings, ShellConnection, ShellLease, Toplevel, ToplevelId, ToplevelRequest, WindowId,
    WindowRequest, layer_anchor,
};

pub use controls::{ButtonState, SCROLLBAR_WIDTH, ScrollbarPart, thumb_span};
pub use menu::MenuItemState;
pub use menubar::{MenuBar, MenuEvent, MenuVisual};
pub use painter::{Ink, Painter};
pub use prompt::Prompt;
pub use scroll::Scroll;
pub use tooltip::Tooltip;
pub use window::{CaptionButton, FrameKind};

#[cfg(test)]
mod tests;
