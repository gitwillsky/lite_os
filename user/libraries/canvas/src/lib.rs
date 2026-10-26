//! CPU 2D 光栅：填充、斜边、渐变、棋盘抖动、XOR、alpha blit 与蒙版着色。
//!
//! 只依赖 `core`，没有 unsafe 与分配；所有坐标是物理像素，HiDPI 缩放由调用者把逻辑尺寸乘以整数倍后传入。

#![no_std]

extern crate liteos_runtime as _;
mod color;
mod geometry;
mod sampling;
mod surface;
pub use sampling::ImageSource;

pub use color::Color;
pub use geometry::Rect;
pub use surface::{Image, Mask, Surface};

#[cfg(test)]
mod tests;
