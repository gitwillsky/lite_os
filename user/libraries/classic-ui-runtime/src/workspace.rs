//! Windows MDI 用同一工作区裁剪、命中和绘制。子项顺序即叠放顺序，末项位于最前。
use super::widgets::{Node, Tree, invalid};
use canvas::Rect;
use classic_theme::SystemColor;
use classic_ui::{
    CAPTION_HEIGHT, CaptionButton, CursorShape, EventKind, FrameKind, Painter,
    trailing_caption_button,
};
use classic_ui_abi::{self as abi, event, widget};
use std::io;

const CLOSE: [(CaptionButton, bool); 1] = [(CaptionButton::Close, true)];
const MIN_WIDTH: i32 = 280;
const MIN_HEIGHT: i32 = 180;

pub(super) fn initial_width(layout: abi::Layout) -> i32 {
    if layout.width < 0 { 640 } else { layout.width }
}

pub(super) fn initial_height(layout: abi::Layout) -> i32 {
    if layout.height < 0 {
        390
    } else {
        layout.height
    }
}

#[derive(Clone, Copy)]
enum Part {
    Move,
    Resize,
    Close,
}

/// 一次鼠标抓取。保留按下时的相对位置以按绝对位移计算，避免累积取整和边界误差。
#[derive(Clone, Copy)]
pub(super) struct Gesture {
    frame: u64,
    part: Part,
    pointer: (i32, i32),
    initial: Rect,
}

impl Gesture {
    pub(super) fn frame(self) -> u64 {
        self.frame
    }
}

fn caption(rect: Rect) -> Rect {
    let inner = rect.inset(FrameKind::Resizable.border());
    Rect::new(
        inner.x,
        inner.y,
        inner.width,
        CAPTION_HEIGHT.min(inner.height),
    )
}

fn contains(rect: Rect, point: (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&point.0) && (rect.y..rect.bottom()).contains(&point.1)
}

fn close_rect(rect: Rect) -> Rect {
    trailing_caption_button(caption(rect))
}

fn part(rect: Rect, pointer: (i32, i32)) -> Option<Part> {
    if !contains(rect, pointer) {
        return None;
    }
    if contains(close_rect(rect), pointer) {
        Some(Part::Close)
    } else if contains(caption(rect), pointer) {
        Some(Part::Move)
    } else if pointer.0 >= rect.right() - 12 && pointer.1 >= rect.bottom() - 12 {
        Some(Part::Resize)
    } else {
        None
    }
}

fn fit(rect: Rect, bounds: Rect) -> Rect {
    let width = rect.width.clamp(MIN_WIDTH.min(bounds.width), bounds.width);
    let height = rect
        .height
        .clamp(MIN_HEIGHT.min(bounds.height), bounds.height);
    Rect::new(
        rect.x.clamp(0, bounds.width - width),
        rect.y.clamp(0, bounds.height - height),
        width,
        height,
    )
}

impl Tree {
    pub(super) fn front_frame(&self, workspace: &Node) -> Option<u64> {
        workspace.children.iter().rev().copied().find(|id| {
            let frame = &self.nodes[id];
            frame.visible() && frame.enabled()
        })
    }

    pub(super) fn place_frame(&mut self, id: u64, bounds: Rect, index: usize) -> Rect {
        let preferred = self.nodes[&id]
            .child_frame
            .as_deref()
            .copied()
            .expect("child frame");
        let rect = if preferred.x < 0 && !bounds.is_empty() {
            let offset = (index % 8) as i32 * 24;
            let position = Rect::new(offset, offset, preferred.width, preferred.height);
            *self
                .nodes
                .get_mut(&id)
                .expect("child frame")
                .child_frame
                .as_deref_mut()
                .expect("frame placement") = position;
            position
        } else {
            preferred
        };
        let rect = fit(rect, bounds);
        Rect::new(
            bounds.x + rect.x,
            bounds.y + rect.y,
            rect.width,
            rect.height,
        )
    }

    pub(super) fn raise_frame(&mut self, workspace: u64, id: u64, focus: bool) -> io::Result<bool> {
        let parent = self
            .nodes
            .get(&workspace)
            .ok_or_else(|| invalid("unknown workspace"))?;
        let frame = self
            .nodes
            .get(&id)
            .filter(|frame| frame.parent == workspace && frame.visible() && frame.enabled());
        if parent.kind != widget::WORKSPACE || frame.is_none() {
            return Err(invalid("not an available workspace child"));
        }
        let previous = self.front_frame(parent);
        let changed = previous != Some(id);
        if changed {
            self.nodes.raise_child_frame(workspace, id);
        }
        if focus
            && !self
                .focus
                .is_some_and(|target| self.containing_frame(target) == Some(id))
        {
            let mut order = Vec::new();
            self.focus_order(id, &mut order);
            self.focus = order.into_iter().next();
            if let Some(target) = self.focus {
                self.request_reveal(target);
            }
        } else if changed
            && self
                .focus
                .is_some_and(|target| self.containing_frame(target) == previous)
        {
            self.focus = None;
        }
        Ok(changed)
    }

    pub(super) fn containing_frame(&self, mut id: u64) -> Option<u64> {
        while let Some(node) = self.nodes.get(&id) {
            if node.kind == widget::CHILD_FRAME {
                return Some(id);
            }
            id = node.parent;
        }
        None
    }

    pub(super) fn workspace_cursor(&self) -> Option<CursorShape> {
        if let Some(gesture) = self.workspace_gesture {
            return Some(match gesture.part {
                Part::Resize => CursorShape::NwseResize,
                _ => CursorShape::Default,
            });
        }
        let id = self.hit().and_then(|id| self.containing_frame(id))?;
        matches!(part(self.nodes[&id].rect, self.pointer), Some(Part::Resize))
            .then_some(CursorShape::NwseResize)
    }

    pub(super) fn workspace_input(
        &mut self,
        window: u64,
        input: &EventKind,
        painter: &Painter<'_>,
        out: &mut Vec<abi::Event>,
    ) -> (bool, bool) {
        if let Some(gesture) = self.workspace_gesture {
            let id = gesture.frame;
            if !self.active(id) || matches!(input, EventKind::Configure(_)) {
                self.workspace_gesture = None;
                return (false, true);
            }
            match input {
                EventKind::PointerEnter { .. } | EventKind::PointerMove { .. } => {
                    return (true, self.drag_frame(gesture, painter));
                }
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } => {
                    self.workspace_gesture = None;
                    if matches!(gesture.part, Part::Close)
                        && self.hit() == Some(id)
                        && contains(close_rect(self.nodes[&id].rect), self.pointer)
                    {
                        out.push(abi::Event {
                            window,
                            node: id,
                            kind: event::ACTIVATE,
                            ..Default::default()
                        });
                    }
                    return (true, true);
                }
                EventKind::PointerLeave | EventKind::Focus(false) => {
                    self.workspace_gesture = None;
                    return (false, true);
                }
                _ => return (true, false),
            }
        }
        if let EventKind::Key(key) = input
            && key.pressed
            && key.modifiers.control
            && matches!(key.code, 62 | 64)
            && let Some(id) = self.focus.and_then(|focus| self.containing_frame(focus))
        {
            if key.code == 62 {
                out.push(abi::Event {
                    window,
                    node: id,
                    kind: event::ACTIVATE,
                    ..Default::default()
                });
                return (true, false);
            }
            let parent = self.nodes[&id].parent;
            let mut frames = self.nodes[&parent]
                .children
                .iter()
                .copied()
                .filter(|frame| {
                    let frame = &self.nodes[frame];
                    frame.visible() && frame.enabled()
                });
            let next = if key.modifiers.shift {
                frames.rev().nth(1)
            } else {
                frames.next()
            };
            let changed = next.is_some_and(|next| {
                self.raise_frame(parent, next, true)
                    .expect("valid workspace child")
            });
            return (true, changed);
        }
        if !matches!(
            input,
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            }
        ) {
            return (false, false);
        }
        let Some(id) = self.hit().and_then(|id| self.containing_frame(id)) else {
            return (false, false);
        };
        let frame = &self.nodes[&id];
        let parent = frame.parent;
        let bounds = self.nodes[&parent].rect.inset(self.nodes[&parent].inset());
        let rect = frame.rect;
        let part = part(rect, self.pointer);
        let changed = self
            .raise_frame(parent, id, part.is_some())
            .expect("hit workspace child");
        if let Some(part) = part {
            self.armed = None;
            self.workspace_gesture = Some(Gesture {
                frame: id,
                part,
                pointer: self.pointer,
                initial: Rect::new(
                    rect.x - bounds.x,
                    rect.y - bounds.y,
                    rect.width,
                    rect.height,
                ),
            });
            return (true, true);
        }
        (false, changed)
    }

    fn drag_frame(&mut self, gesture: Gesture, painter: &Painter<'_>) -> bool {
        let (dx, dy) = (
            self.pointer.0.saturating_sub(gesture.pointer.0),
            self.pointer.1.saturating_sub(gesture.pointer.1),
        );
        let rect = gesture.initial;
        let (resized, moved) = match gesture.part {
            Part::Close => return true,
            Part::Move => (
                false,
                Rect::new(
                    rect.x.saturating_add(dx),
                    rect.y.saturating_add(dy),
                    rect.width,
                    rect.height,
                ),
            ),
            Part::Resize => (
                true,
                Rect::new(
                    rect.x,
                    rect.y,
                    rect.width.saturating_add(dx),
                    rect.height.saturating_add(dy),
                ),
            ),
        };
        let frame = &self.nodes[&gesture.frame];
        let workspace = &self.nodes[&frame.parent];
        let bounds = workspace.rect.inset(workspace.inset());
        let clip = workspace.clip.intersect(bounds);
        let next = if resized {
            let width = (bounds.width - moved.x).max(0);
            let height = (bounds.height - moved.y).max(0);
            Rect::new(
                moved.x,
                moved.y,
                moved.width.clamp(MIN_WIDTH.min(width), width),
                moved.height.clamp(MIN_HEIGHT.min(height), height),
            )
        } else {
            fit(moved, bounds)
        };
        if frame.child_frame.as_deref() == Some(&next) {
            return false;
        }
        *self
            .nodes
            .get_mut(&gesture.frame)
            .expect("drag frame")
            .child_frame
            .as_deref_mut()
            .expect("frame placement") = next;
        let rect = fit(next, bounds);
        let rect = Rect::new(
            bounds.x + rect.x,
            bounds.y + rect.y,
            rect.width,
            rect.height,
        );
        self.place_child(gesture.frame, rect, clip, painter);
        true
    }

    pub(super) fn caption_state(&self, id: u64, node: &Node, focused: bool) -> (bool, bool) {
        let active = focused && self.front_frame(&self.nodes[&node.parent]) == Some(id);
        let pressed = self.workspace_gesture.is_some_and(|gesture| {
            gesture.frame == id
                && matches!(gesture.part, Part::Close)
                && contains(close_rect(node.rect), self.pointer)
        });
        (active, pressed)
    }

    pub(super) fn paint_frame(
        &self,
        id: u64,
        node: &Node,
        painter: &mut Painter<'_>,
        focused: bool,
    ) {
        let (active, pressed) = self.caption_state(id, node, focused);
        painter.fill(node.rect, SystemColor::ButtonFace);
        painter.window_frame(node.rect, FrameKind::Resizable);
        painter.caption_bar(
            caption(node.rect),
            node.text(),
            None,
            active,
            &CLOSE,
            pressed.then_some(CaptionButton::Close),
        );
    }
}
