//! comctl32 列表项的绘制：大图标（LVS_ICON）与小图标/列表/详细资料首列的“图标 + 标签”。
//! 几何取 comctl32 的列表视图常量：图标距格顶 4，标签距格顶 图标高 + 6，标签框比文字左右各宽 2。
//! 选中时只高亮标签矩形，控件有焦点时图标按 ILD_BLEND50 与高亮色混合；焦点框只围标签。
use crate::{Bitmap, Painter, painter::Ink};
use canvas::Rect;
use classic_theme::SystemColor;

/// 图标顶部内边距（ICON_TOP_PADDING）。
const ICON_TOP: i32 = 4;
/// 图标与标签之间的间距（IMAGE_PADDING）。
const IMAGE_PADDING: i32 = 2;
/// 标签框相对文字的左右内边距。
const LABEL_PADDING: i32 = 2;

/// 列表项的状态。
#[derive(Clone, Copy)]
pub struct ItemLook {
    pub selected: bool,
    /// 控件拥有键盘焦点：选中项用高亮色，否则用按钮面色（LVS_SHOWSELALWAYS）。
    pub active: bool,
    /// 本项是焦点项且应显示焦点框。
    pub focused: bool,
    /// 未选中时的文字颜色（列表为 WindowText，桌面为 HilightText）。
    pub text: SystemColor,
}

impl ItemLook {
    fn colors(self) -> (Option<SystemColor>, SystemColor) {
        match (self.selected, self.active) {
            (true, true) => (Some(SystemColor::Hilight), SystemColor::HilightText),
            (true, false) => (Some(SystemColor::ButtonFace), SystemColor::ButtonText),
            _ => (None, self.text),
        }
    }

    fn icon(self, p: &mut Painter<'_>, icon: Option<&Bitmap>, x: i32, y: i32) {
        if let Some(icon) = icon {
            if self.selected && self.active {
                icon.paint_blended(p, x, y, SystemColor::Hilight);
            } else {
                icon.paint(p, x, y);
            }
        }
    }
}

/// 大图标项的标签几何：标签框与各行文字（已按行数上限截断，末行带省略号）。
pub struct IconLabel {
    pub rect: Rect,
    pub lines: Vec<String>,
}

impl Painter<'_> {
    /// 大图标项在 `cell`（图标间距大小的格子）中的标签几何；`full` 为焦点项，显示全部行（可伸出格子）。
    pub fn icon_label(&self, cell: Rect, icon_height: i32, text: &str, full: bool) -> IconLabel {
        let width = cell.width - 2 * LABEL_PADDING;
        let top = cell.y + ICON_TOP + icon_height + IMAGE_PADDING;
        let line = self.line_height().max(1);
        let ranges = self.wrap_lines(width, text);
        let limit = if full {
            ranges.len()
        } else {
            ((cell.bottom() - top - 1) / line).max(1) as usize
        };
        let mut lines: Vec<String> = ranges
            .iter()
            .take(limit)
            .map(|range| text[range.clone()].trim_end().to_owned())
            .collect();
        if ranges.len() > limit
            && let Some(start) = ranges.get(limit - 1).map(|range| range.start)
        {
            // DT_END_ELLIPSIS：末行装不下的余文以省略号结尾。
            lines[limit - 1] = self.fit(&text[start..], width);
        }
        let widest = lines
            .iter()
            .map(|line| self.logical_ceil(self.text_width(line, false)))
            .max()
            .unwrap_or(0);
        let label_width = (widest + 2 * LABEL_PADDING).min(cell.width);
        IconLabel {
            rect: Rect::new(
                cell.x + (cell.width - label_width) / 2,
                top,
                label_width,
                lines.len() as i32 * line + 1,
            ),
            lines,
        }
    }

    /// 画一个大图标项；`text=None` 时只画图标，原地编辑器独占标签区域。
    /// 返回标签矩形（供命中与原地重命名）；没有标签时返回空矩形。
    pub fn large_icon_item(
        &mut self,
        cell: Rect,
        icon: Option<&Bitmap>,
        text: Option<&str>,
        look: ItemLook,
    ) -> Rect {
        let size = icon.map_or(32, Bitmap::height);
        look.icon(
            self,
            icon,
            cell.x + (cell.width - size) / 2,
            cell.y + ICON_TOP,
        );
        let Some(text) = text else {
            return Rect::new(cell.x, cell.y, 0, 0);
        };
        let label = self.icon_label(cell, size, text, look.focused);
        let (background, ink) = look.colors();
        if let Some(background) = background {
            self.fill(label.rect, background);
        }
        let line = self.line_height();
        for (index, text) in label.lines.iter().enumerate() {
            self.text_centered(
                Rect::new(
                    label.rect.x,
                    label.rect.y + index as i32 * line,
                    label.rect.width,
                    line + 1,
                ),
                text,
                Ink::Color(ink),
                false,
            );
        }
        if look.focused {
            self.focus_rect(label.rect);
        }
        label.rect
    }

    /// 画一个小图标项（小图标、列表视图，或详细资料的首列）：16 像素图标在左，标签框紧贴文字、
    /// 不超过 `cell` 右边；`text=None` 时只画图标，返回空矩形，否则返回标签矩形。
    pub fn small_icon_item(
        &mut self,
        cell: Rect,
        icon: Option<&Bitmap>,
        text: Option<&str>,
        look: ItemLook,
    ) -> Rect {
        let leading = if let Some(icon) = icon {
            look.icon(
                self,
                Some(icon),
                cell.x + IMAGE_PADDING,
                cell.y + (cell.height - icon.height()) / 2,
            );
            IMAGE_PADDING + icon.width() + IMAGE_PADDING
        } else {
            0
        };
        let Some(text) = text else {
            return Rect::new(cell.x, cell.y, 0, 0);
        };
        let available = (cell.width - leading).max(0);
        let shown = self.fit(text, available - 2 * LABEL_PADDING);
        let width =
            (self.logical_ceil(self.text_width(&shown, false)) + 2 * LABEL_PADDING).min(available);
        let label = Rect::new(cell.x + leading, cell.y, width, cell.height);
        let (background, ink) = look.colors();
        if let Some(background) = background {
            self.fill(label, background);
        }
        self.text_fit(
            label.x + LABEL_PADDING,
            cell.y + (cell.height - self.line_height()) / 2,
            (label.width - 2 * LABEL_PADDING).max(0),
            &shown,
            Ink::Color(ink),
            false,
        );
        if look.focused {
            self.focus_rect(label);
        }
        label
    }
}
