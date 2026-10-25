use canvas::{Color, Mask, Rect, Surface};
use classic_theme::{Edge, Rgb, SystemColor, Theme};
use typeface::Typeface;

/// 文字墨色：普通颜色，或浮雕式禁用样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Color(SystemColor),
    Disabled,
}

/// 绑定了主题、字体与整数 scale 的绘制器；坐标与尺寸都是逻辑像素。
pub struct Painter<'a> {
    pub(crate) surface: Surface<'a>,
    pub(crate) theme: &'a Theme,
    pub(crate) typeface: &'a Typeface,
    pub(crate) scale: i32,
}

pub(crate) fn to_color(rgb: Rgb) -> Color {
    Color::rgb(rgb.red, rgb.green, rgb.blue)
}

impl<'a> Painter<'a> {
    /// `scale` 小于 1 时按 1 处理。
    pub fn new(surface: Surface<'a>, theme: &'a Theme, typeface: &'a Typeface, scale: i32) -> Self {
        Self {
            surface,
            theme,
            typeface,
            scale: scale.max(1),
        }
    }

    pub fn scale(&self) -> i32 {
        self.scale
    }

    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// 逻辑像素换算成物理像素。
    pub(crate) fn px(&self, logical: i32) -> i32 {
        logical * self.scale
    }

    /// 物理像素向上取整换算成逻辑像素。
    pub(crate) fn logical_ceil(&self, physical: i32) -> i32 {
        (physical + self.scale - 1) / self.scale
    }

    pub(crate) fn physical(&self, rect: Rect) -> Rect {
        Rect::new(
            self.px(rect.x),
            self.px(rect.y),
            self.px(rect.width),
            self.px(rect.height),
        )
    }

    pub(crate) fn color(&self, color: SystemColor) -> Color {
        to_color(self.theme.color(color))
    }

    pub fn fill(&mut self, rect: Rect, color: SystemColor) {
        let (rect, color) = (self.physical(rect), self.color(color));
        self.surface.fill_rect(rect, color);
    }

    /// 1 逻辑像素宽的单色矩形外框。
    pub fn outline(&mut self, rect: Rect, color: SystemColor) {
        let (physical, color) = (self.physical(rect), self.color(color));
        self.surface.bevel(physical, color, color, self.scale);
    }

    /// 画立体边框并返回内部区域。
    ///
    /// 只画样式含有的层，依次从最外圈向内；因此双层样式内部收缩 2 逻辑像素，
    /// 单层样式（`RaisedInner`、`SunkenOuter`）线条在最外圈、只收缩 1 像素，与 `DrawEdge` 一致。
    pub fn edge(&mut self, rect: Rect, edge: Edge) -> Rect {
        let colors = self.theme.edge(edge);
        let mut ring = rect;
        for (top_left, bottom_right) in [colors.outer, colors.inner].into_iter().flatten() {
            let physical = self.physical(ring);
            self.surface.bevel(
                physical,
                to_color(top_left),
                to_color(bottom_right),
                self.scale,
            );
            ring = ring.inset(1);
        }
        ring
    }

    /// 异或绘制的点状焦点框（每隔一个逻辑像素一点）。
    pub fn focus_rect(&mut self, rect: Rect) {
        let physical = self.physical(rect);
        self.surface.invert_dotted_frame(physical, self.scale);
    }

    /// 文字行盒的高度（逻辑像素向上取整）。
    pub fn line_height(&self) -> i32 {
        let (ascent, descent) = self.typeface.metrics();
        self.logical_ceil(ascent + descent)
    }

    /// 文本的物理像素宽度。
    pub fn text_width(&self, text: &str, bold: bool) -> i32 {
        self.typeface.measure(text, bold)
    }

    /// 在逻辑坐标 `(x, y)`（行盒左上角）绘制单行文本，返回物理像素宽度。
    pub fn text(&mut self, x: i32, y: i32, text: &str, ink: Ink, bold: bool) -> i32 {
        self.ink_physical(self.px(x), self.px(y), text, ink, bold)
    }

    /// 禁用样式是浮雕：先在 (+1, +1) 逻辑像素用高光色，再原位用阴影色。
    fn ink_physical(&mut self, x: i32, y: i32, text: &str, ink: Ink, bold: bool) -> i32 {
        match ink {
            Ink::Color(color) => {
                let color = self.color(color);
                self.text_physical(x, y, text, color, bold)
            }
            Ink::Disabled => {
                let highlight = self.color(SystemColor::ButtonHilight);
                let shadow = self.color(SystemColor::ButtonShadow);
                self.text_physical(x + self.scale, y + self.scale, text, highlight, bold);
                self.text_physical(x, y, text, shadow, bold)
            }
        }
    }

    fn text_physical(&mut self, x: i32, y: i32, text: &str, color: Color, bold: bool) -> i32 {
        let baseline = y + self.typeface.metrics().0;
        let mut pen = x;
        for character in text.chars() {
            let glyph = self.typeface.glyph(character, bold);
            if glyph.width > 0 {
                let mask = Mask {
                    coverage: &glyph.coverage,
                    width: glyph.width,
                    height: glyph.height,
                    stride: glyph.width as usize,
                };
                self.surface
                    .draw_mask(mask, pen + glyph.left, baseline - glyph.top, color);
            }
            pen += glyph.advance;
        }
        pen - x
    }

    /// 在 `rect` 内水平、垂直居中绘制一行文本。
    pub fn text_centered(&mut self, rect: Rect, text: &str, ink: Ink, bold: bool) {
        let physical = self.physical(rect);
        let (ascent, descent) = self.typeface.metrics();
        let width = self.text_width(text, bold);
        let x = physical.x + (physical.width - width) / 2;
        let y = physical.y + (physical.height - (ascent + descent)) / 2;
        self.ink_physical(x, y, text, ink, bold);
    }

    /// 将 ASCII 点阵按逻辑像素绘制：`#` 为前景像素，其余透明；每个逻辑像素画成 `scale` × `scale`。
    pub(crate) fn pixels(&mut self, x: i32, y: i32, rows: &[&str], color: SystemColor) {
        let color = self.color(color);
        for (row, line) in rows.iter().enumerate() {
            for (column, mark) in line.bytes().enumerate() {
                if mark == b'#' {
                    let cell = Rect::new(
                        self.px(x + column as i32),
                        self.px(y + row as i32),
                        self.scale,
                        self.scale,
                    );
                    self.surface.fill_rect(cell, color);
                }
            }
        }
    }

    /// 把 `draw` 里的所有绘制限制在逻辑矩形 `rect` 内（与当前裁剪区求交），结束后恢复。
    pub fn clipped(&mut self, rect: Rect, draw: impl FnOnce(&mut Self)) {
        let previous = self.surface.clip();
        let physical = self.physical(rect);
        self.surface.set_clip(previous.intersect(physical));
        draw(self);
        self.surface.set_clip(previous);
    }

    pub fn into_surface(self) -> Surface<'a> {
        self.surface
    }
}
