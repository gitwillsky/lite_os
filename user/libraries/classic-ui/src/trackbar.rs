//! 滑块条（trackbar）：凹槽、刻度与带尖角的凸起滑块，Windows 2000 的音量与平衡滑块。

use canvas::Rect;
use classic_theme::SystemColor as C;

use crate::Painter;

/// 滑块沿轨道方向的厚度与垂直轨道方向的长度。
const THUMB_ALONG: i32 = 11;
const THUMB_ACROSS: i32 = 20;
/// 轨道两端为滑块留出的余量。
const END_MARGIN: i32 = 5;

/// 滑块条的绘制与输入共用几何；坐标为逻辑像素，不保存控件数值。
#[derive(Clone, Copy)]
pub struct Trackbar {
    rect: Rect,
    vertical: bool,
}

/// 一次滑块抓取的起点；只在按下至释放期间保留，防止滑块边缘跳到指针中心。
pub struct TrackbarDrag {
    pointer: f64,
    position: i32,
}

impl Trackbar {
    /// 根据控件边界和方向建立共用几何。
    ///
    /// # Parameters
    /// - `rect`：逻辑像素边界。
    /// - `vertical`：竖向为 true，数值沿屏幕 y 方向增长；否则沿 x 方向增长。
    ///
    /// # Returns
    /// 无分配的几何视图；过短的轨道行程为零。
    pub fn new(rect: Rect, vertical: bool) -> Self {
        Self { rect, vertical }
    }

    fn axis(self, point: (f64, f64)) -> f64 {
        if self.vertical { point.1 } else { point.0 }
    }

    fn span(self) -> (i32, i32) {
        let (origin, length) = if self.vertical {
            (self.rect.y, self.rect.height)
        } else {
            (self.rect.x, self.rect.width)
        };
        (origin + END_MARGIN, (length - 2 * END_MARGIN - 1).max(0))
    }

    fn centre(self, position: i32, maximum: i32) -> i32 {
        let (start, span) = self.span();
        start
            + (i64::from(position.clamp(0, maximum.max(0))) * i64::from(span)
                / i64::from(maximum.max(1))) as i32
    }

    /// 开始抓取：滑块内保留原抓取点，轨道上点击则立即定位。
    ///
    /// # Parameters
    /// - `position`：当前数值，按 `0..=maximum` 裁剪。
    /// - `maximum`：范围上限，非正值视为零。
    /// - `point`：逻辑像素指针坐标。
    ///
    /// # Returns
    /// 只保存本次指针与数值起点的抓取状态，不拥有可写控件数值。
    pub fn press(self, position: i32, maximum: i32, point: (f64, f64)) -> TrackbarDrag {
        let centre = self.centre(position, maximum);
        let thumb = if self.vertical {
            Rect::new(
                self.rect.x,
                centre - THUMB_ALONG / 2,
                THUMB_ACROSS,
                THUMB_ALONG,
            )
        } else {
            Rect::new(
                centre - THUMB_ALONG / 2,
                self.rect.y,
                THUMB_ALONG,
                THUMB_ACROSS,
            )
        };
        let on_thumb = point.0 >= f64::from(thumb.x)
            && point.0 < f64::from(thumb.right())
            && point.1 >= f64::from(thumb.y)
            && point.1 < f64::from(thumb.bottom());
        TrackbarDrag {
            pointer: if on_thumb {
                self.axis(point)
            } else {
                f64::from(self.span().0)
            },
            position: if on_thumb {
                position.clamp(0, maximum.max(0))
            } else {
                0
            },
        }
    }
}

impl TrackbarDrag {
    /// 将相对抓取起点的位移映射为数值；零位移保留原值，避免量化误差导致按下跳动。
    ///
    /// # Parameters
    /// - `bar`：当前布局的共用几何。
    /// - `maximum`：数值上限，非正值视为零。
    /// - `point`：当前逻辑像素指针坐标，允许移出控件。
    ///
    /// # Returns
    /// 四舍五入并裁剪到 `0..=maximum` 的数值；零行程保持抓取值。
    pub fn position(&self, bar: Trackbar, maximum: i32, point: (f64, f64)) -> i32 {
        let span = bar.span().1;
        let delta = if span == 0 {
            0.
        } else {
            (bar.axis(point) - self.pointer) * f64::from(maximum.max(0)) / f64::from(span)
        };
        (f64::from(self.position) + delta)
            .round()
            .clamp(0., f64::from(maximum.max(0))) as i32
    }
}

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
        let origin_across = if vertical { rect.x } else { rect.y };
        let geometry = Trackbar::new(rect, vertical);
        let (start, span) = geometry.span();
        let end = start + span;
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
        let centre = geometry.centre(position, maximum);
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
