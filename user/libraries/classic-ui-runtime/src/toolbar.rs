//! comctl32 平面工具栏（TBSTYLE_FLAT | TBSTYLE_LIST，TBSTYLE_EX_DRAWDDARROWS / MIXEDBUTTONS）：
//! 按钮矩形在布局时算出，绘制、命中与下拉锚点共用；按钮数据由应用提供，悬停/按下状态由运行库独占。
use super::widgets::{Node, Tree};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{EventKind, Ink, Painter};
use classic_ui_abi::{self as abi, event, toolbar as style, widget};
use std::{
    rc::Rc,
    time::{Duration, Instant},
};

/// comctl32 默认按钮内边距（TBSTYLE 的 cxPad / cyPad）。
const PAD_X: i32 = 7;
const PAD_Y: i32 = 6;
/// 分体下拉箭头部分的宽度（comctl32 DDARROW_WIDTH）。
const ARROW: i32 = 11;
/// 分隔条宽度（平面工具栏默认 8）。
const SEPARATOR: i32 = 8;
/// 列表样式中图像与文字、文字与右边框的间距。
const TEXT_GAP: i32 = 2;
const DOWN: [&str; 3] = ["#####", ".###.", "..#.."];
/// 工具提示的初始延时：comctl32 TTDT_INITIAL 默认取双击时间（500 毫秒）。
const TIP_DELAY: Duration = Duration::from_millis(500);
/// 提示框相对指针向下的偏移：标准箭头光标的高度。
const TIP_OFFSET: i32 = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    Button,
    Arrow,
}

pub struct Button {
    id: u64,
    /// 默认（灰度冷图）、悬停（彩色热图）、禁用三条位图；缺失时不画图像。
    strips: [Option<Rc<classic_ui::Bitmap>>; 3],
    frame: i32,
    text: String,
    style: u32,
    state: u32,
}

#[derive(Default)]
pub struct Toolbar {
    buttons: Vec<Button>,
    /// 与 buttons 一一对应的按钮矩形（布局结果）。
    rects: Vec<Rect>,
    hot: Option<(usize, Part)>,
    pressed: Option<(usize, Part)>,
    /// 悬停按钮的工具提示：（按钮下标，显示时刻）；按下或离开即取消。
    tip: Option<(usize, Instant)>,
}

/// 位图条的帧宽：宽度能被高度整除时为正方形帧，否则整张位图是单帧（如 18×16 的“转到”箭头）。
fn frame_width(strip: &classic_ui::Bitmap) -> i32 {
    let height = strip.height().max(1);
    if strip.width() % height == 0 {
        height
    } else {
        strip.width()
    }
}

impl Button {
    pub fn new(
        id: u64,
        strips: [Option<Rc<classic_ui::Bitmap>>; 3],
        frame: i32,
        text: String,
        style: u32,
        state: u32,
    ) -> Self {
        Self {
            id,
            strips,
            frame,
            text,
            style,
            state,
        }
    }
}

impl Toolbar {
    /// 替换按钮；同一组按钮只刷新状态（启用、选中）时保留悬停与按下，按钮组变化才复位交互状态。
    pub fn set_buttons(&mut self, buttons: Vec<Button>) {
        let same = self.buttons.len() == buttons.len()
            && self
                .buttons
                .iter()
                .zip(&buttons)
                .all(|(old, new)| old.id == new.id);
        self.buttons = buttons;
        if !same {
            self.hot = None;
            self.pressed = None;
            self.tip = None;
        }
    }

    /// 工具提示的显示截止时刻（尚未显示时）。
    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        self.tip.map(|(_, due)| due).filter(|due| *due > now)
    }

    /// 到期的提示需要重绘时返回 true。
    pub fn tick(&mut self, now: Instant, last: Instant) -> bool {
        self.tip.is_some_and(|(_, due)| due <= now && due > last)
    }

    /// 已到期、应显示的提示文字。
    pub fn tip(&self, now: Instant) -> Option<&str> {
        let (index, due) = self.tip?;
        (due <= now && self.pressed.is_none())
            .then(|| self.buttons.get(index).map(|button| button.text.as_str()))
            .flatten()
            .filter(|text| !text.is_empty())
    }

    /// 画已到期的工具提示；位置在指针下方，超出 `bounds` 时向内平移。
    pub fn paint_tip(&self, p: &mut Painter<'_>, pointer: (i32, i32), bounds: Rect, now: Instant) {
        let Some(text) = self.tip(now) else {
            return;
        };
        let width = p.measure_role(classic_ui::FontRole::Status, text, false) + 8;
        let x = pointer.0.min(bounds.right() - width).max(bounds.x);
        p.tooltip(x, pointer.1 + TIP_OFFSET, text);
    }

    /// 工具栏的图像高度（comctl32 的位图尺寸对整条工具栏统一）。
    fn image(&self) -> i32 {
        self.buttons
            .iter()
            .find_map(|button| button.strips[0].as_ref().map(|strip| strip.height()))
            .unwrap_or(16)
    }

    fn image_width(&self, button: &Button) -> i32 {
        button.strips[0]
            .as_deref()
            .map_or_else(|| self.image(), frame_width)
    }

    fn split(button: &Button) -> bool {
        button.style & style::DROPDOWN != 0
    }

    /// 按钮的自然宽度：内边距 + 图像 + 可选文字 + 可选下拉箭头。
    fn width(&self, button: &Button, font: &Painter<'_>) -> i32 {
        if button.style & style::SEPARATOR != 0 {
            return SEPARATOR;
        }
        let mut width = PAD_X + self.image_width(button);
        if button.style & style::SHOW_TEXT != 0 && !button.text.is_empty() {
            // comctl32 工具栏默认使用图标标题字体（SPI_GETICONTITLELOGFONT）。
            width += TEXT_GAP
                + font.measure_role(classic_ui::FontRole::Icon, &button.text, false)
                + TEXT_GAP;
        }
        if button.style & (style::DROPDOWN | style::WHOLE_DROPDOWN) != 0 {
            width += ARROW;
        }
        width
    }

    /// 自然尺寸：按钮宽度之和，高度为图像与文字行高的较大者加上下内边距。
    pub fn natural(&self, font: &Painter<'_>) -> (i32, i32) {
        let width = self
            .buttons
            .iter()
            .map(|button| self.width(button, font))
            .sum();
        (width, self.image().max(font.line_height()) + PAD_Y)
    }

    pub fn layout(&mut self, rect: Rect, font: &Painter<'_>) {
        let height = self.image().max(font.line_height()) + PAD_Y;
        let y = rect.y + (rect.height - height) / 2;
        let mut x = rect.x;
        self.rects = self
            .buttons
            .iter()
            .map(|button| {
                let width = self.width(button, font);
                let area = Rect::new(x, y, width, height);
                x += width;
                area
            })
            .collect();
    }

    fn contains(rect: Rect, point: (i32, i32)) -> bool {
        point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
    }

    fn hit(&self, point: (i32, i32)) -> Option<(usize, Part)> {
        let index = self
            .rects
            .iter()
            .position(|rect| Self::contains(*rect, point))?;
        let button = &self.buttons[index];
        if button.style & style::SEPARATOR != 0 || button.state & style::DISABLED != 0 {
            return None;
        }
        let rect = self.rects[index];
        let arrow = Self::split(button) && point.0 >= rect.right() - ARROW;
        Some((index, if arrow { Part::Arrow } else { Part::Button }))
    }

    pub fn paint(&self, p: &mut Painter<'_>, disabled: bool) {
        let image = self.image();
        for (index, (button, rect)) in self.buttons.iter().zip(&self.rects).enumerate() {
            let rect = *rect;
            if button.style & style::SEPARATOR != 0 {
                let x = rect.x + rect.width / 2 - 1;
                p.fill(
                    Rect::new(x, rect.y + 2, 1, rect.height - 4),
                    C::ButtonShadow,
                );
                p.fill(
                    Rect::new(x + 1, rect.y + 2, 1, rect.height - 4),
                    C::ButtonHilight,
                );
                continue;
            }
            let inactive = disabled || button.state & style::DISABLED != 0;
            let hot = !inactive && self.hot.is_some_and(|(hot, _)| hot == index);
            let pressed = !inactive
                && self.pressed.is_some_and(|(at, part)| {
                    at == index && (part == Part::Arrow || self.hot == Some((index, Part::Button)))
                });
            let checked = button.state & style::CHECKED != 0;
            let main = if Self::split(button) {
                Rect::new(rect.x, rect.y, rect.width - ARROW, rect.height)
            } else {
                rect
            };
            let pressed_main =
                pressed && self.pressed.is_some_and(|(_, part)| part == Part::Button);
            if checked && !hot {
                // 选中而未悬停：按钮面与高光的抖动底，外加单层凹陷边（comctl32 TBSTATE_CHECKED）。
                p.checker(main.inset(1), C::ButtonHilight, C::ButtonFace);
            }
            if pressed_main || checked {
                p.edge(main, Edge::SunkenOuter);
            } else if hot {
                p.edge(main, Edge::RaisedInner);
            }
            if Self::split(button) {
                let arrow = Rect::new(main.right(), rect.y, ARROW, rect.height);
                let pressed_arrow =
                    pressed && self.pressed.is_some_and(|(_, part)| part == Part::Arrow);
                if pressed_arrow {
                    p.edge(arrow, Edge::SunkenOuter);
                } else if hot {
                    p.edge(arrow, Edge::RaisedInner);
                }
                draw_arrow(p, arrow, inactive, pressed_arrow);
            }
            let offset = i32::from(pressed_main);
            let strip = if inactive {
                &button.strips[2]
            } else if hot {
                &button.strips[1]
            } else {
                &button.strips[0]
            };
            let image_x = main.x + (PAD_X + 1) / 2 + offset;
            let image_y = rect.y + (rect.height - image) / 2 + offset;
            if let Some(strip) = strip.as_ref().or(button.strips[0].as_ref()) {
                let frame = frame_width(strip);
                strip.paint_region(p, image_x, image_y, button.frame * frame, frame);
            }
            let mut right = main.right();
            if button.style & style::WHOLE_DROPDOWN != 0 {
                let arrow = Rect::new(main.right() - ARROW, rect.y, ARROW, rect.height);
                draw_arrow(p, arrow, inactive, pressed_main);
                right = arrow.x;
            }
            if button.style & style::SHOW_TEXT != 0 && !button.text.is_empty() {
                let x = image_x + self.image_width(button) + TEXT_GAP;
                let ink = if inactive {
                    Ink::Disabled
                } else {
                    Ink::Color(C::ButtonText)
                };
                p.with_role(classic_ui::FontRole::Icon, |p| {
                    p.label_fit(
                        x,
                        rect.y + (rect.height - p.line_height()) / 2 + offset,
                        (right - x).max(0),
                        &button.text,
                        ink,
                    );
                });
            }
        }
    }
}

fn draw_arrow(p: &mut Painter<'_>, area: Rect, disabled: bool, pressed: bool) {
    let offset = i32::from(pressed);
    let x = area.x + (area.width - 5) / 2 + offset;
    let y = area.y + (area.height - 3) / 2 + offset;
    if disabled {
        p.glyph(x + 1, y + 1, &DOWN, C::ButtonHilight);
        p.glyph(x, y, &DOWN, C::ButtonShadow);
    } else {
        p.glyph(x, y, &DOWN, C::ButtonText);
    }
}

impl Tree {
    /// 工具栏的悬停、按下与激活。返回 (已消费, 需要重绘)；未消费的事件继续交给其他控件。
    pub fn toolbar_input(
        &mut self,
        window: u64,
        input: &EventKind,
        out: &mut Vec<abi::Event>,
    ) -> (bool, bool) {
        let pointer = self.pointer;
        let ids: Vec<u64> = self
            .nodes
            .iter()
            .filter(|(id, node)| node.kind == widget::TOOLBAR && self.active(**id))
            .map(|(id, _)| *id)
            .collect();
        let mut consumed = false;
        let mut repaint = false;
        for id in ids {
            let node = self.nodes.get_mut(&id).expect("toolbar node");
            let inside = Toolbar::contains(node.clip, pointer);
            let model = node.toolbar.as_mut().expect("toolbar model");
            match input {
                EventKind::PointerEnter { .. } | EventKind::PointerMove { .. } => {
                    let hot = if inside { model.hit(pointer) } else { None };
                    if model.hot.map(|(index, _)| index) != hot.map(|(index, _)| index) {
                        // 换到另一个按钮：已显示的提示收起，新按钮重新计时。
                        repaint |= model.tip.is_some();
                        model.tip = hot.map(|(index, _)| (index, Instant::now() + TIP_DELAY));
                    }
                    repaint |= model.hot != hot;
                    model.hot = hot;
                }
                EventKind::PointerLeave | EventKind::Focus(false) => {
                    repaint |=
                        model.hot.is_some() || model.pressed.is_some() || model.tip.is_some();
                    model.hot = None;
                    model.pressed = None;
                    model.tip = None;
                }
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } if inside => {
                    consumed = true;
                    let Some((index, part)) = model.hit(pointer) else {
                        continue;
                    };
                    repaint = true;
                    model.pressed = Some((index, part));
                    model.tip = None;
                    let button = &model.buttons[index];
                    if part == Part::Arrow || button.style & style::WHOLE_DROPDOWN != 0 {
                        // comctl32 在按下时发出 TBN_DROPDOWN；菜单关闭前按钮保持按下外观。
                        let rect = model.rects[index];
                        out.push(abi::Event {
                            window,
                            node: id,
                            kind: event::DROPDOWN,
                            value: button.id,
                            x: f64::from(rect.x),
                            y: f64::from(rect.y),
                            width: rect.width,
                            height: rect.height,
                            ..Default::default()
                        });
                    }
                }
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } => {
                    let Some((index, part)) = model.pressed.take() else {
                        continue;
                    };
                    consumed = true;
                    repaint = true;
                    let button = &mut model.buttons[index];
                    if part == Part::Button
                        && button.style & style::WHOLE_DROPDOWN == 0
                        && model.hot == Some((index, Part::Button))
                    {
                        if button.style & style::CHECK != 0 {
                            button.state ^= style::CHECKED;
                        }
                        out.push(abi::Event {
                            window,
                            node: id,
                            kind: event::ACTIVATE,
                            value: button.id,
                            ..Default::default()
                        });
                    }
                }
                _ => {}
            }
        }
        (consumed, repaint)
    }
}

impl Node {
    /// 下拉菜单结束后撤销按钮的按下外观。
    pub fn release_toolbar(&mut self) {
        if let Some(model) = self.toolbar.as_mut() {
            model.pressed = None;
        }
    }
}
