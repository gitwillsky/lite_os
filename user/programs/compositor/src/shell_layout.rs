//! 桌面外壳 surface（layer-shell）的定位与工作区计算。纯逻辑。

use crate::{
    desktop::Geometry,
    wayland::{
        LayerState,
        layer_edges::{ANCHOR_BOTTOM, ANCHOR_LEFT, ANCHOR_RIGHT, ANCHOR_TOP, exclusive_side},
    },
};

/// 按锚点与边距算出 layer surface 左上角的物理坐标。
///
/// # Parameters
///
/// - `size`: surface 内容的物理像素尺寸（客户端已按 configure 给出的大小画好）。
/// - `area`: 定位所在的物理矩形：独占区为 `0` 的 surface 按 layer-shell 规范避开其他 surface 的独占区，
///   由调用者传入工作区；独占区为 `-1` 或正数的 surface 传入整个输出。
/// - `scale`: 输出 scale；边距是逻辑像素。
///
/// 同时锚定两条对边时，surface 在这两条边（扣除各自边距）之间居中；没有锚定的方向居中于 `area`。
pub fn place(state: &LayerState, size: (i32, i32), area: Geometry, scale: i32) -> (i32, i32) {
    let [top, right, bottom, left] = state
        .margin
        .map(|margin| i64::from(margin) * i64::from(scale));
    let axis = |low: bool, high: bool, extent: i32, size: i32, near: i64, far: i64| {
        let (extent, size) = (i64::from(extent), i64::from(size));
        let coordinate = match (low, high) {
            (true, false) => near,
            (false, true) => extent - size - far,
            (true, true) => near + (extent - near - far - size) / 2,
            (false, false) => (extent - size) / 2,
        };
        coordinate.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    let (x, y) = (
        axis(
            state.anchor & ANCHOR_LEFT != 0,
            state.anchor & ANCHOR_RIGHT != 0,
            area.2,
            size.0,
            left,
            right,
        ),
        axis(
            state.anchor & ANCHOR_TOP != 0,
            state.anchor & ANCHOR_BOTTOM != 0,
            area.3,
            size.1,
            top,
            bottom,
        ),
    );
    (x.saturating_add(area.0), y.saturating_add(area.1))
}

/// 扣除所有独占区之后的工作区（最大化窗口占据的区域）。
///
/// 独占区（`exclusive_zone > 0`）作用在由锚点或 `exclusive_edge` 决定的那条边上，并加上该边的边距；
/// `0` 与 `-1`（"延伸到屏幕边缘"）不保留空间。
pub fn work_area<'a>(
    states: impl IntoIterator<Item = &'a LayerState>,
    screen: (i32, i32),
    scale: i32,
) -> Geometry {
    let (mut x, mut y, mut width, mut height) = (0, 0, screen.0, screen.1);
    for state in states {
        if state.exclusive_zone <= 0 {
            continue;
        }
        let Some(side) = exclusive_side(state) else {
            continue;
        };
        let [top, right, bottom, left] = state.margin;
        // 客户端可提交任意 i32 边距；用宽算术并限制在剩余区域内，避免溢出或负预留扩大输出。
        let reserved = |margin: i32, extent: i32| {
            ((i64::from(state.exclusive_zone) + i64::from(margin)) * i64::from(scale))
                .clamp(0, i64::from(extent.max(0))) as i32
        };
        match side {
            ANCHOR_TOP => {
                let amount = reserved(top, height);
                (y, height) = (y + amount, height - amount);
            }
            ANCHOR_BOTTOM => height -= reserved(bottom, height),
            ANCHOR_LEFT => {
                let amount = reserved(left, width);
                (x, width) = (x + amount, width - amount);
            }
            ANCHOR_RIGHT => width -= reserved(right, width),
            _ => {}
        }
    }
    (x, y, width.max(0), height.max(0))
}

#[cfg(test)]
mod tests;
