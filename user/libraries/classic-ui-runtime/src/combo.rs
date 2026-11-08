//! ComboBoxEx（CBS_DROPDOWN）：可编辑文字区、左侧当前项图标、边框内的下拉按钮与按层次缩进的下拉列表。
//! 文字与光标由 Node.edit 持有，项目层次复用 Outline（全部展开），图标复用 row_icons；本模块只持有弹出状态。
use super::widgets::{Node, Tree};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{EventKind, Ink, Painter, SCROLLBAR_WIDTH};
use classic_ui_abi::{self as abi, event, state, widget};

/// 下拉按钮宽度与滚动条一致（SM_CXVSCROLL）。
const BUTTON: i32 = SCROLLBAR_WIDTH;
/// 文字区左侧为当前项图标让出的宽度：16 像素图标加两侧间距。
const LEADING: i32 = 16 + 4;
/// 未设置展开高度时下拉列表最多可见行数；窗口内放不下时再按可用空间减少。
const VISIBLE: i32 = 16;
/// ComboBoxEx 每级缩进（CBEIF_INDENT 的单位）。
const INDENT: i32 = 10;
const DOWN: [&str; 4] = ["#######", ".#####.", "..###..", "...#..."];

#[derive(Default)]
pub struct Combo {
    pub open: bool,
    hover: Option<usize>,
    top: usize,
    pressed: bool,
    /// 打开时求出的列表矩形：列表画在本窗口表面内，只能在窗口范围里向下或向上展开；
    /// 缺少它时列表按固定行数伸出窗口底部被裁掉，末行与滚动条不可见。
    list: Rect,
    /// 应用声明的展开总高度（模板 cy），0 表示未声明；缺少它时所有组合框都按默认 16 行展开，
    /// 与模板规定的列表高度不符。
    pub dropped: i32,
}

impl Node {
    /// 项目带图标时（ComboBoxEx）文字前留出图标列；纯文字组合框（USER32 COMBOBOX）文字贴左。
    fn combo_leading(&self) -> i32 {
        if self.row_icons.is_empty() {
            0
        } else {
            LEADING
        }
    }

    /// 文字区左右边距；绘制与光标命中共用。
    pub fn combo_margins(&self) -> (i32, i32) {
        (self.combo_leading(), BUTTON)
    }

    /// 下拉列表项高度：纯文字项为字体行高（USER32 列表框项高 = tmHeight），带图标项取图标与文字的较大者再加 2。
    pub fn combo_item_height(&self, line: i32) -> i32 {
        if self.row_icons.is_empty() {
            line
        } else {
            line.max(16) + 2
        }
    }

    fn combo_button(&self) -> Rect {
        let rect = self.rect;
        Rect::new(
            rect.right() - 2 - BUTTON,
            rect.y + 2,
            BUTTON,
            (rect.height - 4).max(0),
        )
    }

    fn combo_count(&self) -> usize {
        self.row_count()
    }

    /// 在窗口 `area` 内为下拉列表选位置：1 像素黑框，默认紧贴控件下方；
    /// 下方放不下全部可见行且上方更宽裕时贴着控件上方展开（USER32 下拉在屏幕底部的行为）。
    fn place_combo_list(&self, area: Rect) -> Rect {
        let item = self.row_height.max(1);
        let declared = self.combo.as_ref().map_or(0, |combo| combo.dropped);
        let limit = if declared > 0 {
            ((declared - self.rect.height - 2) / item).max(1)
        } else {
            VISIBLE
        };
        let wanted = (self.combo_count() as i32).min(limit);
        let below = self.rect.bottom().max(area.y);
        let below = (area.bottom() - below - 2) / item;
        let above = (self.rect.y - area.y - 2) / item;
        let (rows, upward) = if below >= wanted || below >= above {
            (wanted.min(below).max(1), false)
        } else {
            (wanted.min(above).max(1), true)
        };
        let height = rows * item + 2;
        let y = if upward {
            self.rect.y - height
        } else {
            self.rect.bottom()
        };
        Rect::new(self.rect.x, y, self.rect.width, height)
    }

    pub(super) fn combo_list(&self) -> Rect {
        self.combo
            .as_ref()
            .map(|combo| combo.list)
            .unwrap_or_default()
    }

    /// 当前列表能同时显示的行数。
    fn combo_visible(&self) -> usize {
        ((self.combo_list().height - 2) / self.row_height.max(1)).max(1) as usize
    }

    fn combo_row_at(&self, point: (i32, i32)) -> Option<usize> {
        let list = self.combo_list();
        if !contains(list.inset(1), point) {
            return None;
        }
        let combo = self.combo.as_ref()?;
        let index = combo.top + ((point.1 - list.y - 1) / self.row_height.max(1)) as usize;
        (index < self.combo_count()).then_some(index)
    }

    /// 打开下拉列表：定位列表、高亮当前选中项并把它滚到可见范围。
    fn open_combo(&mut self, area: Rect) {
        let selected = self.selected.and_then(|value| {
            (0..self.row_count())
                .find(|index| self.row_at(*index).is_some_and(|row| row.0 == value))
        });
        let list = self.place_combo_list(area);
        let combo = self.combo.as_mut().expect("combo");
        combo.open = true;
        combo.list = list;
        combo.hover = selected;
        let visible = self.combo_visible();
        let maximum = self.combo_count().saturating_sub(visible);
        let combo = self.combo.as_mut().expect("combo");
        combo.top = selected
            .map_or(0, |index| index.saturating_sub(visible - 1))
            .min(maximum);
    }

    fn combo_depth(&self, index: usize) -> i32 {
        self.outline
            .as_ref()
            .map_or(0, |outline| outline.depth(index))
    }

    /// 选中第 `index` 项：文字替换为项目文字并全选，记录稳定标识。
    fn combo_choose(&mut self, index: usize) -> Option<u64> {
        let (id, text) = self.row_at(index)?.clone();
        let mut edit = classic_ui::Edit::new(&text);
        edit.select_position(0, false);
        edit.select_position(text.len(), true);
        self.edit = Some(edit);
        self.selected = Some(id);
        Some(id)
    }

    fn dropdown_list(&self) -> bool {
        self.state & state::DROPDOWN_LIST != 0
    }

    pub fn paint_combo(&self, p: &mut Painter<'_>, focused: bool, enabled: bool) {
        let Some(edit) = self.edit.as_ref() else {
            return;
        };
        if self.dropdown_list() {
            // 下拉列表：凹陷白框内显示选中项；有焦点时文字区为选择色并带焦点框（USER32 CBS_DROPDOWNLIST）。
            let inner = p.edge(self.rect, Edge::Sunken);
            p.fill(inner, if enabled { C::Window } else { C::ButtonFace });
            let leading = self.combo_leading();
            let field = Rect::new(
                inner.x + 1 + leading,
                inner.y + 1,
                (inner.width - BUTTON - 2 - leading).max(0),
                (inner.height - 2).max(0),
            );
            if focused && enabled {
                p.fill(field, C::Hilight);
                p.focus_rect(field);
            }
            let ink = if !enabled {
                Ink::Disabled
            } else if focused {
                Ink::Color(C::HilightText)
            } else {
                Ink::Color(C::WindowText)
            };
            p.text_fit(
                field.x + 2,
                field.y + (field.height - p.line_height()) / 2,
                (field.width - 4).max(0),
                edit.text(),
                ink,
                false,
            );
        } else if enabled {
            p.edit_with_margins(self.rect, edit, focused, false, self.combo_margins());
        } else {
            p.edit_enabled(self.rect, edit, false, false, false);
        }
        if let Some(icon) = self
            .selected
            .and_then(|id| self.row_icons.get(&id))
            .and_then(|icons| icons.0.as_ref())
        {
            icon.paint(
                p,
                self.rect.x + 4,
                self.rect.y + (self.rect.height - 16) / 2,
            );
        }
        let button = self.combo_button();
        let pressed = self
            .combo
            .as_ref()
            .is_some_and(|combo| combo.pressed || combo.open);
        let offset = i32::from(pressed);
        if pressed {
            p.outline(button, C::ButtonShadow);
            p.fill(button.inset(1), C::ButtonFace);
        } else {
            let face = p.edge(button, Edge::Raised);
            p.fill(face, C::ButtonFace);
        }
        p.glyph(
            button.x + (button.width - 7) / 2 + offset,
            button.y + (button.height - 4) / 2 + offset,
            &DOWN,
            if enabled { C::ButtonText } else { C::GrayText },
        );
    }

    /// 打开时的下拉列表；在整棵控件树之后绘制，覆盖下方内容。
    pub fn paint_combo_list(&self, p: &mut Painter<'_>) {
        let Some(combo) = self.combo.as_ref().filter(|combo| combo.open) else {
            return;
        };
        let list = self.combo_list();
        p.fill(list, C::Window);
        p.outline(list, C::WindowFrame);
        let count = self.combo_count();
        let visible = self.combo_visible();
        let scroll = count > visible;
        let rows = Rect::new(
            list.x + 1,
            list.y + 1,
            list.width - 2 - if scroll { SCROLLBAR_WIDTH } else { 0 },
            list.height - 2,
        );
        p.clipped(rows, |p| {
            for (slot, index) in (combo.top..count).take(visible).enumerate() {
                let Some((id, text)) = self.row_at(index) else {
                    continue;
                };
                let row = Rect::new(
                    rows.x,
                    rows.y + slot as i32 * self.row_height,
                    rows.width,
                    self.row_height,
                );
                let hot = combo.hover == Some(index);
                if hot {
                    p.fill(row, C::Hilight);
                }
                let x = row.x + 2 + self.combo_depth(index) * INDENT;
                if let Some(icon) = self.row_icons.get(id).and_then(|icons| icons.0.as_ref()) {
                    icon.paint(p, x, row.y + (row.height - 16) / 2);
                }
                let left = x + self.combo_leading();
                p.text_fit(
                    left,
                    row.y + (row.height - p.line_height()) / 2,
                    (row.right() - left - 2).max(0),
                    text,
                    Ink::Color(if hot { C::HilightText } else { C::WindowText }),
                    false,
                );
            }
        });
        if scroll {
            p.scrollbar(
                Rect::new(rows.right(), rows.y, SCROLLBAR_WIDTH, rows.height),
                true,
                (count as i32 - 1, visible as i32, combo.top as i32),
                None,
            );
        }
    }
}

fn contains(rect: Rect, point: (i32, i32)) -> bool {
    point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
}

impl Tree {
    /// 下拉列表可占用的窗口区域；控件可交互时必已布局，未布局只会让列表保底一行。
    fn combo_area(&self) -> Rect {
        self.layout_bounds
            .map(|(bounds, _)| bounds)
            .unwrap_or_default()
    }

    /// 组合框的下拉、选择与键盘；返回 (已消费, 需要重绘)。收起时的文字编辑交给普通编辑路径。
    pub fn combo_input(
        &mut self,
        window: u64,
        input: &EventKind,
        out: &mut Vec<abi::Event>,
    ) -> (bool, bool) {
        let open = self
            .nodes
            .iter()
            .find(|(_, node)| node.combo.as_ref().is_some_and(|combo| combo.open))
            .map(|(id, _)| *id);
        let pointer = self.pointer;
        match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                if let Some(id) = open {
                    let node = self.nodes.get_mut(&id).expect("open combo");
                    if let Some(index) = node.combo_row_at(pointer)
                        && let Some(value) = node.combo_choose(index)
                    {
                        out.push(abi::Event {
                            window,
                            node: id,
                            kind: event::SELECT,
                            value,
                            ..Default::default()
                        });
                    }
                    // 列表外的单击只关闭列表（被下拉列表捕获），不传给下方控件。
                    node.combo.as_mut().expect("combo").open = false;
                    return (true, true);
                }
                let target = self
                    .nodes
                    .iter()
                    .find(|(id, node)| {
                        node.kind == widget::COMBO
                            && self.active(**id)
                            && contains(
                                if node.dropdown_list() {
                                    node.rect
                                } else {
                                    node.combo_button()
                                },
                                pointer,
                            )
                    })
                    .map(|(id, _)| *id);
                let Some(id) = target else {
                    return (false, false);
                };
                self.focus = Some(id);
                let area = self.combo_area();
                let node = self.nodes.get_mut(&id).expect("combo");
                node.open_combo(area);
                node.combo.as_mut().expect("combo").pressed = true;
                (true, true)
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            } => {
                let mut repaint = false;
                for (_, node) in self
                    .nodes
                    .matching_mut(|node| node.combo.as_ref().is_some_and(|combo| combo.pressed))
                {
                    if let Some(combo) = node.combo.as_mut() {
                        repaint |= std::mem::replace(&mut combo.pressed, false);
                    }
                }
                (open.is_some(), repaint)
            }
            EventKind::PointerEnter { .. } | EventKind::PointerMove { .. } => {
                let Some(id) = open else {
                    return (false, false);
                };
                let node = self.nodes.get_mut(&id).expect("open combo");
                let hover = node.combo_row_at(pointer);
                let combo = node.combo.as_mut().expect("combo");
                if hover.is_some() && combo.hover != hover {
                    combo.hover = hover;
                    return (true, true);
                }
                (true, false)
            }
            EventKind::Wheel { vertical, .. } => {
                let Some(id) = open else {
                    return (false, false);
                };
                let node = self.nodes.get_mut(&id).expect("open combo");
                let maximum = node.combo_count().saturating_sub(node.combo_visible());
                let combo = node.combo.as_mut().expect("combo");
                combo.top =
                    (combo.top as i64 + i64::from(*vertical) * 3).clamp(0, maximum as i64) as usize;
                (true, true)
            }
            EventKind::Focus(false) => {
                let Some(id) = open else {
                    return (false, false);
                };
                self.nodes
                    .get_mut(&id)
                    .expect("combo")
                    .combo
                    .as_mut()
                    .expect("combo")
                    .open = false;
                (false, true)
            }
            EventKind::Key(key) if key.pressed => {
                let Some(id) = self.focus.filter(|id| self.nodes[id].kind == widget::COMBO) else {
                    return (false, false);
                };
                let area = self.combo_area();
                let node = self.nodes.get_mut(&id).expect("combo");
                let count = node.combo_count();
                let is_open = node.combo.as_ref().is_some_and(|combo| combo.open);
                let toggle = key.code == 62 || (key.modifiers.alt && matches!(key.code, 103 | 108));
                if toggle {
                    if is_open {
                        node.combo.as_mut().expect("combo").open = false;
                    } else if count > 0 {
                        node.open_combo(area);
                    }
                    return (true, true);
                }
                if !is_open && node.dropdown_list() {
                    // 收起的下拉列表：方向键、Home/End 直接换选项并通知应用。
                    let current = node.selected.and_then(|value| {
                        (0..count)
                            .find(|index| node.row_at(*index).is_some_and(|row| row.0 == value))
                    });
                    let index = match (key.code, current) {
                        (103 | 105, Some(index)) => index.saturating_sub(1),
                        (108 | 106, Some(index)) => (index + 1).min(count.saturating_sub(1)),
                        (102, _) | (103 | 105 | 108 | 106, None) => 0,
                        (107, _) => count.saturating_sub(1),
                        _ => return (false, false),
                    };
                    if count > 0
                        && Some(index) != current
                        && let Some(value) = node.combo_choose(index)
                    {
                        out.push(abi::Event {
                            window,
                            node: id,
                            kind: event::SELECT,
                            value,
                            ..Default::default()
                        });
                    }
                    return (true, true);
                }
                if !is_open {
                    if key.code == 28 && !key.modifiers.alt {
                        // 收起时 Enter 等同“转到”：交给应用解析地址。
                        out.push(abi::Event {
                            window,
                            node: id,
                            kind: event::ACTIVATE,
                            ..Default::default()
                        });
                        return (true, true);
                    }
                    return (false, false);
                }
                let hover = node.combo.as_ref().and_then(|combo| combo.hover);
                let visible = node.combo_visible();
                match key.code {
                    1 => node.combo.as_mut().expect("combo").open = false,
                    28 => {
                        if let Some(value) = hover.and_then(|index| node.combo_choose(index)) {
                            out.push(abi::Event {
                                window,
                                node: id,
                                kind: event::SELECT,
                                value,
                                ..Default::default()
                            });
                        }
                        node.combo.as_mut().expect("combo").open = false;
                    }
                    103 | 108 if count > 0 => {
                        let index = match (hover, key.code) {
                            (Some(index), 103) => index.saturating_sub(1),
                            (Some(index), _) => (index + 1).min(count - 1),
                            (None, _) => 0,
                        };
                        let combo = node.combo.as_mut().expect("combo");
                        combo.hover = Some(index);
                        if index < combo.top {
                            combo.top = index;
                        } else if index >= combo.top + visible {
                            combo.top = index + 1 - visible;
                        }
                    }
                    _ => {}
                }
                (true, true)
            }
            _ => (false, false),
        }
    }
}
