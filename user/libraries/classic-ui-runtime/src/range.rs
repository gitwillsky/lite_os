//! 数值控件的唯一值 owner 为 Scroll.position；范围配置不复制当前值。
use super::widgets::{Node, Tree, invalid};
use classic_ui::EventKind;
use classic_ui_abi::{self as abi, event, widget};
use std::io;
pub struct RangeState {
    pub minimum: i32,
    pub maximum: i32,
    pub page: i32,
    pub step: i32,
    pub vertical: bool,
}
impl Default for RangeState {
    fn default() -> Self {
        Self {
            minimum: 0,
            maximum: 100,
            page: 1,
            step: 1,
            vertical: false,
        }
    }
}
impl RangeState {
    fn extent(&self) -> i32 {
        self.maximum - self.minimum + 1
    }
    fn limit(&self) -> i32 {
        (self.extent() - self.page).max(0)
    }
}
impl Node {
    pub fn set_range(&mut self, value: abi::Range) -> io::Result<()> {
        if !matches!(self.kind, widget::SLIDER | widget::SCROLLBAR) {
            return Err(invalid("not a range control"));
        }
        let span = i64::from(value.maximum) - i64::from(value.minimum);
        if !(0..i64::from(i32::MAX)).contains(&span)
            || value.page < 1
            || i64::from(value.page) > span + 1
            || value.step < 1
            || value.vertical > 1
        {
            return Err(invalid("invalid range"));
        }
        self.range = RangeState {
            minimum: value.minimum,
            maximum: value.maximum,
            page: if self.kind == widget::SLIDER {
                1
            } else {
                value.page
            },
            step: value.step,
            vertical: value.vertical != 0,
        };
        self.position.position = (i64::from(value.value) - i64::from(value.minimum))
            .clamp(0, i64::from(self.range.limit())) as i32;
        Ok(())
    }
    pub fn range_value(&self) -> io::Result<abi::Range> {
        if !matches!(self.kind, widget::SLIDER | widget::SCROLLBAR) {
            return Err(invalid("not a range control"));
        }
        Ok(abi::Range {
            minimum: self.range.minimum,
            maximum: self.range.maximum,
            value: self.range.minimum + self.position.position,
            page: self.range.page,
            step: self.range.step,
            vertical: u32::from(self.range.vertical),
        })
    }
    fn move_slider(&mut self, point: (i32, i32)) {
        let (offset, length) = if self.range.vertical {
            (point.1 - self.rect.y - 5, self.rect.height - 10)
        } else {
            (point.0 - self.rect.x - 5, self.rect.width - 10)
        };
        self.position.position = if length <= 0 {
            0
        } else {
            (i64::from(offset.clamp(0, length)) * i64::from(self.range.limit()) / i64::from(length))
                as i32
        };
    }
}
impl Tree {
    pub fn range_input(
        &mut self,
        window: u64,
        input: &EventKind,
        out: &mut Vec<abi::Event>,
    ) -> bool {
        let pointer = match input {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                (x.floor() as i32, y.floor() as i32)
            }
            _ => self.pointer,
        };
        let id = match input {
            EventKind::PointerEnter { .. }
            | EventKind::PointerMove { .. }
            | EventKind::Button { pressed: false, .. }
            | EventKind::PointerLeave
            | EventKind::Focus(false) => self.armed,
            EventKind::Key(_) => self.focus,
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            }
            | EventKind::Wheel { .. } => self.hit(),
            _ => None,
        };
        let Some(id) = id.filter(|id| {
            self.nodes.get(id).is_some_and(|node| {
                matches!(node.kind, widget::SLIDER | widget::SCROLLBAR) && node.focusable()
            }) && self.active(*id)
        }) else {
            return false;
        };
        let node = self.nodes.get_mut(&id).expect("range target");
        let before = node.position.position;
        match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                self.armed = Some(id);
                self.focus = Some(id);
                if node.kind == widget::SLIDER {
                    node.move_slider(pointer);
                } else {
                    node.position.press(
                        node.rect,
                        node.range.vertical,
                        (node.range.extent(), node.range.page, node.range.step),
                        pointer,
                    );
                }
            }
            EventKind::PointerEnter { .. } | EventKind::PointerMove { .. } => {
                self.pointer = pointer;
                if node.kind == widget::SLIDER {
                    node.move_slider(pointer);
                } else {
                    node.position.drag(
                        node.rect,
                        node.range.vertical,
                        (node.range.extent(), node.range.page),
                        pointer,
                    );
                }
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            }
            | EventKind::PointerLeave
            | EventKind::Focus(false) => {
                node.position.release();
                self.armed = None;
            }
            EventKind::Wheel { vertical, .. } => {
                let change = i64::from(*vertical) * i64::from(node.range.step);
                node.position.position =
                    (i64::from(before) + change).clamp(0, i64::from(node.range.limit())) as i32;
            }
            EventKind::Key(key) if key.pressed => {
                let change = match key.code {
                    103 | 105 => -i64::from(node.range.step),
                    108 | 106 => i64::from(node.range.step),
                    104 => -i64::from(node.range.page),
                    109 => i64::from(node.range.page),
                    102 => -i64::from(node.range.extent()),
                    107 => i64::from(node.range.extent()),
                    _ => return false,
                };
                node.position.position =
                    (i64::from(before) + change).clamp(0, i64::from(node.range.limit())) as i32;
            }
            _ => return false,
        }
        if before != node.position.position {
            out.push(abi::Event {
                window,
                node: id,
                kind: event::CHANGED,
                value: i64::from(node.range.minimum + node.position.position) as u64,
                ..Default::default()
            });
        }
        true
    }
}
