//! 交互式移动与缩放的几何：指针抓取开始时记下窗口几何，之后每次指针移动都由它算出新几何。纯逻辑。

use wayland_wire::wire::ObjectId;

use crate::{desktop::Geometry, wayland::ClientId};

/// 窗口至少要留在屏内、能被再次抓住的部分，逻辑像素（标题栏高度与一段宽度）。
const VISIBLE_WIDTH: i32 = 64;
const VISIBLE_HEIGHT: i32 = 19;
/// 缩放的最小尺寸，逻辑像素：足够容纳标题栏按钮。
pub const MIN_WIDTH: i32 = 112;
pub const MIN_HEIGHT: i32 = 27;

/// 缩放抓住了窗口的哪几条边。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl Edges {
    /// 解码 `xdg_toplevel.resize_edge`（top=1、bottom=2、left=4、right=8 的位组合）；
    /// `none` 与任何不合法的组合（如上下同时）返回 `None`。
    pub fn from_xdg(value: u32) -> Option<Self> {
        let edges = Self {
            top: value & 1 != 0,
            bottom: value & 2 != 0,
            left: value & 4 != 0,
            right: value & 8 != 0,
        };
        let known = value & !0b1111 == 0;
        let consistent = !(edges.top && edges.bottom) && !(edges.left && edges.right);
        (known && consistent && value != 0).then_some(edges)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Move,
    Resize(Edges),
}

/// 一次进行中的抓取。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grab {
    pub client: ClientId,
    pub surface: ObjectId,
    pub kind: Kind,
    /// 抓取开始时的指针位置与窗口几何。
    pub pointer: (i32, i32),
    pub start: Geometry,
}

impl Grab {
    /// 移动：窗口跟随指针位移，但不能完全移出屏幕——标题栏必须留在可再次抓住的位置。
    pub fn moved_to(&self, pointer: (i32, i32), screen: (i32, i32), scale: i32) -> (i32, i32) {
        let (x, y, width, _) = self.start;
        let visible_width = (VISIBLE_WIDTH * scale).min(width);
        let x =
            (x + pointer.0 - self.pointer.0).clamp(visible_width - width, screen.0 - visible_width);
        let y =
            (y + pointer.1 - self.pointer.1).clamp(0, (screen.1 - VISIBLE_HEIGHT * scale).max(0));
        (x, y)
    }

    /// 缩放：被抓住的边跟随指针，对边保持不动；不小于最小尺寸。
    ///
    /// # Returns
    ///
    /// 新的窗口几何；非缩放抓取返回起始几何。
    pub fn resized_to(&self, pointer: (i32, i32), scale: i32) -> Geometry {
        let Kind::Resize(edges) = self.kind else {
            return self.start;
        };
        let (x, y, width, height) = self.start;
        let (dx, dy) = (pointer.0 - self.pointer.0, pointer.1 - self.pointer.1);
        let new_width = (width
            + if edges.right {
                dx
            } else if edges.left {
                -dx
            } else {
                0
            })
        .max(MIN_WIDTH * scale);
        let new_height = (height
            + if edges.bottom {
                dy
            } else if edges.top {
                -dy
            } else {
                0
            })
        .max(MIN_HEIGHT * scale);
        // 左/上边被拖动时，右/下边是锚点：位置随尺寸变化反向补偿。
        let new_x = if edges.left { x + width - new_width } else { x };
        let new_y = if edges.top {
            y + height - new_height
        } else {
            y
        };
        (new_x, new_y, new_width, new_height)
    }
}

#[cfg(test)]
mod tests;
