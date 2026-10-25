//! 滑块条（trackbar）：凹槽、刻度与带尖角的凸起滑块，Windows 2000 的音量与平衡滑块。

use canvas::Rect;
use classic_theme::SystemColor as C;

use crate::Painter;

/// 滑块沿轨道方向的厚度与垂直轨道方向的长度。
const THUMB_ALONG: i32 = 11;
const THUMB_ACROSS: i32 = 20;
/// 轨道两端为滑块留出的余量。
const END_MARGIN: i32 = 5;

impl Painter<'_> {
    /// 画一个滑块条。
    ///
    /// 竖向时滑块指向右、刻度在右侧，`position` 从上往下数；横向时滑块指向下、刻度在下方，`position` 从左往右数。
    /// `rect` 沿轨道方向留出两端各 5 像素给滑块，垂直方向至少 28 像素。`position` 取 `0..=maximum`，`ticks` 是刻度间隔数。
    pub fn trackbar(
        &mut self,
        rect: Rect,
        vertical: bool,
        (position, maximum): (i32, i32),
        ticks: i32,
    ) {
        // 统一用（沿轨道，垂直轨道）坐标作图，再映射回 x/y。
        let (origin_along, length, origin_across) = if vertical {
            (rect.y, rect.height, rect.x)
        } else {
            (rect.x, rect.width, rect.y)
        };
        let start = origin_along + END_MARGIN;
        let end = origin_along + length - END_MARGIN - 1;
        let put = |p: &mut Painter<'_>, along: i32, across: i32, w: i32, h: i32, color: C| {
            let cell = if vertical {
                Rect::new(origin_across + across, along, w, h)
            } else {
                Rect::new(along, origin_across + across, h, w)
            };
            p.fill(cell, color);
        };
        // 凹槽：4 像素宽的凹陷边框，两端各伸出 1 像素。
        let channel = if vertical {
            Rect::new(origin_across + 8, start - 1, 4, end - start + 3)
        } else {
            Rect::new(start - 1, origin_across + 8, end - start + 3, 4)
        };
        self.edge(channel, classic_theme::Edge::Sunken);
        // 刻度。
        for tick in 0..=ticks.max(1) {
            let along = start + tick * (end - start) / ticks.max(1);
            put(self, along, THUMB_ACROSS + 3, 3, 1, C::ButtonShadow);
        }
        // 滑块。
        let centre = start + position.clamp(0, maximum.max(1)) * (end - start) / maximum.max(1);
        let half = THUMB_ALONG / 2;
        for step in 0..THUMB_ALONG {
            let along = centre - half + step;
            // 尖端在中线：每远离中线一行，末端缩进一像素。
            let across_end = THUMB_ACROSS - 5 + (half - (step - half).abs());
            put(self, along, 0, across_end, 1, C::ButtonFace);
            put(self, along, 0, 1, 1, C::ButtonHilight);
            put(self, along, across_end - 1, 1, 1, C::ButtonDkShadow);
            if step > 0 {
                put(self, along, across_end - 2, 1, 1, C::ButtonShadow);
            }
            if step == 0 {
                put(self, along, 0, across_end - 5, 1, C::ButtonHilight);
            } else if step == THUMB_ALONG - 1 {
                put(self, along, 0, across_end, 1, C::ButtonDkShadow);
            }
        }
    }
}
