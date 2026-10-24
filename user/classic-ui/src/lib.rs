//! 经典控件的绘制：把 `classic-theme` 的事实、`typeface` 的字形和 `canvas` 的光栅组合成 Win2000 外观。
//!
//! 所有几何以 96 DPI 的逻辑像素给出，[`Painter`] 在落笔时乘以整数 scale。文字宽度依赖字体，
//! 以物理像素返回，需要排版时用 [`Painter::text_width`] 度量。

mod controls;
mod menu;
mod painter;
mod window;

pub use controls::{ButtonState, SCROLLBAR_WIDTH, ScrollbarPart, thumb_span};
pub use menu::MenuItemState;
pub use painter::{Ink, Painter};
pub use window::{CaptionButton, FrameKind};

#[cfg(test)]
mod tests;
