use canvas::{Color, Mask, Rect, Surface};
use classic_theme::{Edge, Rgb, SystemColor, Theme};
use typeface::Typeface;

use crate::{FontRole, Fonts};

/// 文字墨色：普通颜色，或浮雕式禁用样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Color(SystemColor),
    /// 专用画布的明确 RGB，仍复用相同的文字布局与裁剪路径。
    Rgb(Rgb),
    Disabled,
}

/// 绑定了主题、字体与整数 scale 的绘制器；坐标与尺寸都是逻辑像素。
pub struct Painter<'a> {
    pub(crate) surface: Surface<'a>,
    pub(crate) theme: &'a Theme,
    /// 当前 scale 的系统字体集合。
    pub(crate) fonts: &'a Fonts,
    /// 正在使用的字体；默认是消息字体，`with_role`/`with_font` 期间临时替换。
    pub(crate) typeface: &'a Typeface,
    pub(crate) scale: i32,
    /// 是否画助记键下划线（Windows 2000 在按 Alt 或 Tab 之前隐藏它，见 `interaction.md` N1/N2）。
    cues: bool,
}

pub(crate) fn to_color(rgb: Rgb) -> Color {
    Color::rgb(rgb.red, rgb.green, rgb.blue)
}

impl<'a> Painter<'a> {
    /// 以消息字体为默认字体；`scale` 小于 1 时按 1 处理。
    pub fn new(surface: Surface<'a>, theme: &'a Theme, fonts: &'a Fonts, scale: i32) -> Self {
        Self {
            surface,
            theme,
            fonts,
            typeface: fonts.role(FontRole::Message),
            scale: scale.max(1),
            cues: false,
        }
    }

    /// 本绘制器使用的系统字体集合。
    /// 按 ASCII 点阵画一个小字形（`#` 为前景），用于下拉箭头等控件标记；坐标为逻辑像素。
    pub fn glyph(&mut self, x: i32, y: i32, rows: &[&str], color: SystemColor) {
        self.pixels(x, y, rows, color);
    }

    /// 用某个系统字体量出的逻辑宽度（向上取整）；用于在绘制前按该字体布局。
    pub fn measure_role(&self, role: FontRole, text: &str, bold: bool) -> i32 {
        self.logical_ceil(self.fonts.role(role).measure(text, bold))
    }

    /// 本绘制器使用的系统字体集合。
    pub fn fonts(&self) -> &'a Fonts {
        self.fonts
    }

    /// 在 `draw` 期间改用某个系统字体（标题栏、菜单、状态栏、图标标题）；度量与绘制都随之切换。
    pub fn with_role<R>(&mut self, role: FontRole, draw: impl FnOnce(&mut Self) -> R) -> R {
        let previous = self.typeface;
        self.typeface = self.fonts.role(role);
        let result = draw(self);
        self.typeface = previous;
        result
    }

    /// 在 `draw` 期间改用任意已解析字体（例如由 [`Fonts::custom`] 得到的派生样式）。
    pub fn with_font<'c, R>(
        &'c mut self,
        font: &'c Typeface,
        draw: impl FnOnce(&mut Painter<'c>) -> R,
    ) -> R {
        let mut painter = Painter {
            surface: self.surface.reborrow(),
            theme: self.theme,
            fonts: self.fonts,
            typeface: font,
            scale: self.scale,
            cues: self.cues,
        };
        draw(&mut painter)
    }

    /// 设置是否显示助记键下划线；默认隐藏。
    pub fn set_cues(&mut self, visible: bool) {
        self.cues = visible;
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

    /// 用任意 RGB 填充矩形：只给主题没有对应系统色的专用绘制（性能图表的绿色网格与曲线）使用。
    pub fn fill_rgb(&mut self, rect: Rect, rgb: Rgb) {
        let (rect, color) = (self.physical(rect), to_color(rgb));
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

    /// 以逻辑像素为单位的棋盘格填充：`(x + y)` 为偶数的点用 `even`，其余用 `odd`。
    /// 用于按下状态的任务按钮与"选中的"工具栏按钮底（Windows 的 dither brush）。
    pub fn checker(&mut self, rect: Rect, even: SystemColor, odd: SystemColor) {
        self.fill(rect, odd);
        let color = self.color(even);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                if (x + y) % 2 == 0 {
                    let cell = Rect::new(self.px(x), self.px(y), self.scale, self.scale);
                    self.surface.fill_rect(cell, color);
                }
            }
        }
    }

    /// 异或绘制的点状焦点框（每隔一个逻辑像素一点）。
    pub fn focus_rect(&mut self, rect: Rect) {
        let physical = self.physical(rect);
        self.surface.invert_dotted_frame(physical, self.scale);
    }

    /// 当前字体基线到行盒顶部的距离（逻辑像素向上取整）；下划线等装饰据此定位。
    pub fn ascent(&self) -> i32 {
        self.logical_ceil(self.typeface.metrics().0)
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

    /// 把 `text` 截到不超过 `max_width` 逻辑像素，超出时以"..."结尾；不超出则原样返回。
    ///
    /// 界面上任何由数据或语言决定长度的文字（文件名、窗口标题、翻译后的标签）在画之前都应先经过它或
    /// [`Painter::text_fit`]，这样换字体、换语言都不会画出所属区域。
    pub fn fit(&self, text: &str, max_width: i32) -> String {
        self.truncate_to_width(text, max_width.max(0) * self.scale, false)
    }

    /// 在逻辑坐标 `(x, y)` 绘制单行文本，宽度超过 `max_width` 逻辑像素时截断成"..."结尾。
    pub fn text_fit(&mut self, x: i32, y: i32, max_width: i32, text: &str, ink: Ink, bold: bool) {
        let shown = self.truncate_to_width(text, max_width.max(0) * self.scale, bold);
        self.clipped(Rect::new(x, y, max_width, self.line_height()), |p| {
            p.ink_physical(p.px(x), p.px(y), &shown, ink, bold);
        });
    }

    /// 带助记键的 [`Painter::text_fit`]。
    pub fn label_fit(&mut self, x: i32, y: i32, max_width: i32, label: &str, ink: Ink) {
        self.label_fit_weighted(x, y, max_width, label, ink, false);
    }

    /// [`Painter::label_fit`] 的粗体版本（菜单默认项）。
    pub fn label_fit_bold(&mut self, x: i32, y: i32, max_width: i32, label: &str, ink: Ink) {
        self.label_fit_weighted(x, y, max_width, label, ink, true);
    }

    fn label_fit_weighted(
        &mut self,
        x: i32,
        y: i32,
        max_width: i32,
        label: &str,
        ink: Ink,
        bold: bool,
    ) {
        let shown = self.truncate_to_width(label, max_width.max(0) * self.scale, bold);
        self.clipped(Rect::new(x, y, max_width, self.line_height()), |p| {
            p.label_physical(p.px(x), p.px(y), &shown, ink, bold);
        });
    }

    /// 返回给定逻辑宽度下完整文本所需的高度，与 text_wrapped 共用断行规则。
    /// 非正宽度返回零；无错误返回。
    pub fn wrapped_height(&self, width: i32, text: &str, bold: bool) -> i32 {
        if width <= 0 {
            return 0;
        }
        let count = typeface::wrap(text, self.px(width), |ch| {
            self.typeface.glyph(ch, bold).advance
        })
        .len();
        (count as i32).saturating_mul(self.line_height())
    }

    /// 在逻辑矩形中按词与中文断行规则绘制文本，并裁剪到矩形边界。
    ///
    /// # Parameters
    /// - `rect`: 排版宽度与可见区域；非正尺寸不绘制。
    /// - `text`: 文本，换行符强制换行；复用 `typeface::wrap` 的断行规则。
    /// - `ink`、`bold`: 文字颜色与粗体。
    ///
    /// # Returns
    /// 全部行所需的逻辑高度；可能大于 `rect.height`，空矩形返回 0。无错误返回。
    pub fn text_wrapped(&mut self, rect: Rect, text: &str, ink: Ink, bold: bool) -> i32 {
        if rect.is_empty() {
            return 0;
        }
        let lines = typeface::wrap(text, self.px(rect.width), |ch| {
            self.typeface.glyph(ch, bold).advance
        });
        let line_height = self.line_height();
        self.clipped(rect, |p| {
            for (index, line) in lines.iter().enumerate() {
                let y = rect.y + index as i32 * line_height;
                if y >= rect.bottom() {
                    break;
                }
                p.ink_physical(p.px(rect.x), p.px(y), &text[line.clone()], ink, bold);
            }
        });
        lines.len() as i32 * line_height
    }

    /// 用系统等宽字体绘制 8×16 逻辑像素网格文字；宽字符占两格。
    /// font 为资源上下文拥有的 atlas，未知字符显示其替代字形；矩形外全部裁剪，无错误返回。
    pub fn monospace(
        &mut self,
        rect: Rect,
        text: &str,
        font: &terminal_font::TerminalFont,
        rgb: Rgb,
        bold: bool,
    ) {
        use unicode_width::UnicodeWidthChar;
        self.clipped(rect, |p| {
            let mut x = rect.x;
            for character in text.chars() {
                let columns = character.width().unwrap_or(1).clamp(1, 2) as i32;
                if x >= rect.right() {
                    break;
                }
                let mask = Mask {
                    coverage: font.bitmap(font.glyph(character), bold),
                    width: columns * 16,
                    height: terminal_font::CELL_HEIGHT as i32,
                    stride: terminal_font::BITMAP_WIDTH,
                };
                let target = p.physical(Rect::new(x, rect.y, columns * 8, 16));
                p.surface.draw_mask_scaled(mask, target, to_color(rgb));
                x += columns * 8;
            }
        });
    }

    /// 在逻辑坐标 `(x, y)`（行盒左上角）绘制单行文本，返回物理像素宽度。
    pub fn text(&mut self, x: i32, y: i32, text: &str, ink: Ink, bold: bool) -> i32 {
        self.ink_physical(self.px(x), self.px(y), text, ink, bold)
    }

    /// 禁用样式是浮雕：先在 (+1, +1) 逻辑像素用高光色，再原位用阴影色。
    fn ink_physical(&mut self, x: i32, y: i32, text: &str, ink: Ink, bold: bool) -> i32 {
        match ink {
            Ink::Rgb(rgb) => self.text_physical(x, y, text, to_color(rgb), bold),
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

    /// 助记键字母的字符区间：标签里形如 `(F)` 的括号内单个 ASCII 字母，返回字母在 `label` 中的字节范围。
    fn mnemonic_span(label: &str) -> Option<std::ops::Range<usize>> {
        let open = label.rfind('(')?;
        let bytes = label.as_bytes();
        (bytes.get(open + 2) == Some(&b')') && bytes[open + 1].is_ascii_alphabetic())
            .then(|| open + 1..open + 2)
    }

    /// 画一个带助记键的标签；`cues` 打开时在助记字母下画 1 逻辑像素下划线。
    fn label_physical(&mut self, x: i32, y: i32, label: &str, ink: Ink, bold: bool) {
        self.ink_physical(x, y, label, ink, bold);
        let (true, Some(span)) = (self.cues, Self::mnemonic_span(label)) else {
            return;
        };
        let before = self.text_width(&label[..span.start], bold);
        let width = self.text_width(&label[span], bold);
        let (ascent, _) = self.typeface.metrics();
        let color = match ink {
            Ink::Rgb(rgb) => to_color(rgb),
            Ink::Color(color) => self.color(color),
            Ink::Disabled => self.color(SystemColor::ButtonShadow),
        };
        let rect = Rect::new(x + before, y + ascent + self.scale, width, self.scale);
        self.surface.fill_rect(rect, color);
    }

    /// 在逻辑坐标 `(x, y)` 画一个带助记键的标签（菜单项、按钮名）。
    pub fn label(&mut self, x: i32, y: i32, label: &str, ink: Ink, bold: bool) {
        self.label_physical(self.px(x), self.px(y), label, ink, bold);
    }

    /// 在 `rect` 内居中画一个带助记键的标签。
    pub fn label_centered(&mut self, rect: Rect, label: &str, ink: Ink, bold: bool) {
        let physical = self.physical(rect);
        let (ascent, descent) = self.typeface.metrics();
        let label = self.truncate_to_width(label, physical.width, bold);
        let width = self.text_width(&label, bold);
        let x = physical.x + (physical.width - width) / 2;
        let y = physical.y + (physical.height - (ascent + descent)) / 2;
        self.clipped(rect, |p| p.label_physical(x, y, &label, ink, bold));
    }

    /// 用任意 RGB 在逻辑坐标 `(x, y)` 画一行文字；与 [`Painter::fill_rgb`] 一样只给性能图表这类没有系统色对应的绘制用。
    pub fn text_rgb(&mut self, x: i32, y: i32, text: &str, rgb: Rgb) {
        let color = to_color(rgb);
        self.text_physical(self.px(x), self.px(y), text, color, false);
    }

    /// 在 `rect` 内水平、垂直居中绘制一行文本。
    pub fn text_centered(&mut self, rect: Rect, text: &str, ink: Ink, bold: bool) {
        let physical = self.physical(rect);
        let (ascent, descent) = self.typeface.metrics();
        let text = self.truncate_to_width(text, physical.width, bold);
        let width = self.text_width(&text, bold);
        let x = physical.x + (physical.width - width) / 2;
        let y = physical.y + (physical.height - (ascent + descent)) / 2;
        self.clipped(rect, |p| {
            p.ink_physical(x, y, &text, ink, bold);
        });
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
