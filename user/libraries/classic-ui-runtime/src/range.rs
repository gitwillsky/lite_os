//! 数值控件的唯一值 owner 为 Scroll.position；范围配置不复制当前值。
use super::widgets::{Node, Tree, invalid};
use classic_ui::{EventKind, Trackbar};
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
        if !matches!(
            self.kind,
            widget::SLIDER | widget::SCROLLBAR | widget::UPDOWN
        ) {
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
            page: if matches!(self.kind, widget::SLIDER | widget::UPDOWN) {
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
        if !matches!(
            self.kind,
            widget::SLIDER | widget::SCROLLBAR | widget::UPDOWN
        ) {
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
                    node.position.press_slider(
                        Trackbar::new(node.rect, node.range.vertical),
                        node.range.limit(),
                        (f64::from(pointer.0), f64::from(pointer.1)),
                    );
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
                    node.position.drag_slider(
                        Trackbar::new(node.rect, node.range.vertical),
                        node.range.limit(),
                        (f64::from(pointer.0), f64::from(pointer.1)),
                    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use canvas::Rect;

    fn slider(vertical: bool) -> (Tree, u64) {
        let mut tree = Tree::new();
        let id = tree
            .insert(
                0,
                abi::Widget {
                    kind: widget::SLIDER,
                    state: 0,
                    group: 0,
                    layout: Default::default(),
                    text: abi::Text {
                        data: std::ptr::null(),
                        len: 0,
                    },
                },
                String::new(),
            )
            .unwrap();
        let node = tree.nodes.get_mut(&id).unwrap();
        node.rect = Rect::new(
            10,
            20,
            if vertical { 30 } else { 111 },
            if vertical { 111 } else { 30 },
        );
        node.clip = node.rect;
        node.set_range(abi::Range {
            minimum: 0,
            maximum: 100,
            value: 50,
            page: 1,
            step: 1,
            vertical: u32::from(vertical),
        })
        .unwrap();
        (tree, id)
    }

    fn press(tree: &mut Tree, point: (i32, i32)) {
        tree.pointer = point;
        assert!(tree.range_input(
            1,
            &EventKind::Button {
                code: 0x110,
                pressed: true,
                serial: 1
            },
            &mut Vec::new()
        ));
    }

    #[test]
    fn slider_reaches_the_drawn_track_endpoint() {
        for vertical in [false, true] {
            let (mut tree, id) = slider(vertical);
            // 实际绘制终点为 rect.end - 6；输入不能另减 10，使最后一个像素只能得到 99。
            press(&mut tree, if vertical { (20, 125) } else { (115, 30) });
            assert_eq!(tree.nodes[&id].range_value().unwrap().value, 100);
        }
    }

    #[test]
    fn grabbing_the_thumb_edge_preserves_its_position_and_capture_ends_on_release() {
        for vertical in [false, true] {
            let (mut tree, id) = slider(vertical);
            let point = if vertical { (20, 79) } else { (69, 30) };
            press(&mut tree, point);
            assert_eq!(tree.nodes[&id].range_value().unwrap().value, 50);
            let moved = if vertical { (20., 99.) } else { (89., 30.) };
            tree.range_input(
                1,
                &EventKind::PointerMove {
                    x: moved.0,
                    y: moved.1,
                },
                &mut Vec::new(),
            );
            assert_eq!(tree.nodes[&id].range_value().unwrap().value, 70);
            tree.range_input(
                1,
                &EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    serial: 2,
                },
                &mut Vec::new(),
            );
            tree.range_input(1, &EventKind::PointerMove { x: 5., y: 5. }, &mut Vec::new());
            assert_eq!(tree.nodes[&id].range_value().unwrap().value, 70);
        }
    }
}
