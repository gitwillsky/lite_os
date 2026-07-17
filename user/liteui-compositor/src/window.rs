use liteui_core::{NodeId, NodeRole, Primitive, PrimitiveInfo};

use crate::scene::{Damage, Rect};

const MAX_WINDOWS: usize = 8;
const RESIZE_EDGE: usize = 7;
const MINIMUM_WIDTH: usize = 320;
const MINIMUM_HEIGHT: usize = 240;
const TASKBAR_HEIGHT: usize = 42;

#[derive(Clone, Copy)]
struct Window {
    id: NodeId,
    base: Rect,
    geometry: Rect,
    restore: Rect,
    visible: bool,
    maximized: bool,
}

const EMPTY_WINDOW: Window = Window {
    id: NodeId::new(0, 0),
    base: Rect {
        x1: 0,
        y1: 0,
        x2: 0,
        y2: 0,
    },
    geometry: Rect {
        x1: 0,
        y1: 0,
        x2: 0,
        y2: 0,
    },
    restore: Rect {
        x1: 0,
        y1: 0,
        x2: 0,
        y2: 0,
    },
    visible: false,
    maximized: false,
};

#[derive(Clone, Copy)]
enum Interaction {
    Idle,
    Dragging {
        window: NodeId,
        offset_x: usize,
        offset_y: usize,
        preview: Rect,
    },
    Resizing {
        window: NodeId,
        initial: Rect,
        pointer_x: usize,
        pointer_y: usize,
        edges: u8,
        preview: Rect,
    },
    Action {
        node: NodeId,
    },
}

#[derive(Clone, Copy)]
pub struct WindowManager {
    windows: [Window; MAX_WINDOWS],
    count: usize,
    focused: Option<NodeId>,
    interaction: Interaction,
    width: usize,
    height: usize,
}

impl WindowManager {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            windows: [EMPTY_WINDOW; MAX_WINDOWS],
            count: 0,
            focused: None,
            interaction: Interaction::Idle,
            width,
            height,
        }
    }

    pub fn resized(self, width: usize, height: usize) -> Self {
        let mut next = Self {
            interaction: Interaction::Idle,
            width,
            height,
            ..self
        };
        for window in &mut next.windows[..next.count] {
            if window.maximized {
                window.geometry = Rect {
                    x1: 0,
                    y1: 0,
                    x2: width,
                    y2: height.saturating_sub(TASKBAR_HEIGHT),
                };
            } else {
                window.geometry = clamped(window.geometry, width, height);
                window.restore = clamped(window.restore, width, height);
            }
        }
        next
    }

    pub fn reconcile(&mut self, primitives: &[Primitive]) {
        let mut observed = [false; MAX_WINDOWS];
        for primitive in primitives {
            let info = primitive.info();
            if info.role != NodeRole::Window || info.window != Some(info.node) {
                continue;
            }
            let base = Rect::from_ui(info.bounds);
            if let Some(index) = self.index(info.node) {
                observed[index] = true;
                let old_base = self.windows[index].base;
                self.windows[index].base = base;
                if self.windows[index].geometry == old_base {
                    self.windows[index].geometry = base;
                    self.windows[index].restore = base;
                }
                continue;
            }
            if self.count == MAX_WINDOWS {
                continue;
            }
            self.windows[self.count] = Window {
                id: info.node,
                base,
                geometry: base,
                restore: base,
                visible: true,
                maximized: false,
            };
            observed[self.count] = true;
            self.focused = Some(info.node);
            self.count += 1;
        }
        let mut index = 0;
        while index < self.count {
            if observed[index] {
                index += 1;
                continue;
            }
            self.count -= 1;
            self.windows[index] = self.windows[self.count];
            observed[index] = observed[self.count];
            self.windows[self.count] = EMPTY_WINDOW;
        }
        if self.focused.is_some_and(|id| self.index(id).is_none()) {
            self.focused = self.count.checked_sub(1).map(|last| self.windows[last].id);
        }
    }

    pub fn project(&self, info: PrimitiveInfo) -> Option<Rect> {
        let source = Rect::from_ui(info.bounds);
        let Some(window) = info.window else {
            return Some(source);
        };
        let state = self.windows.get(self.index(window)?)?;
        state
            .visible
            .then(|| scaled(source, state.base, state.geometry))
    }

    pub fn z_count(&self) -> usize {
        self.count
    }

    pub fn window_at_z(&self, index: usize) -> Option<NodeId> {
        self.windows
            .get(index)
            .filter(|_| index < self.count)
            .map(|window| window.id)
    }

    /// @description 返回 move/resize transaction 尚未提交的唯一轮廓位置。
    /// @return 无交互时为 None；拖动或缩放时为 compositor-owned preview rectangle。
    pub(super) fn preview(&self) -> Option<Rect> {
        match self.interaction {
            Interaction::Dragging { preview, .. } | Interaction::Resizing { preview, .. } => {
                Some(preview)
            }
            Interaction::Idle | Interaction::Action { .. } => None,
        }
    }

    pub fn focused_contains(&self, role: NodeRole, primitives: &[Primitive]) -> bool {
        let Some(focused) = self.focused else {
            return false;
        };
        primitives.iter().any(|primitive| {
            let info = primitive.info();
            info.window == Some(focused) && info.role == role && self.project(info).is_some()
        })
    }

    pub fn move_focused(&mut self, dx: isize, dy: isize) -> Damage {
        let Some(index) = self.focused.and_then(|id| self.index(id)) else {
            return Damage::EMPTY;
        };
        if self.windows[index].maximized || !self.windows[index].visible {
            return Damage::EMPTY;
        }
        let old = self.windows[index].geometry;
        let width = old.x2.saturating_sub(old.x1);
        let height = old.y2.saturating_sub(old.y1);
        let x = shifted(old.x1, dx, self.width.saturating_sub(width));
        let y = shifted(
            old.y1,
            dy,
            self.height.saturating_sub(TASKBAR_HEIGHT + height),
        );
        self.windows[index].geometry = rectangle(x, y, width, height);
        Damage::pair(old, self.windows[index].geometry)
    }

    pub fn primary_button(
        &mut self,
        x: usize,
        y: usize,
        pressed: bool,
        primitives: &[Primitive],
    ) -> (Damage, bool, Option<NodeId>) {
        if !pressed {
            let previous = core::mem::replace(&mut self.interaction, Interaction::Idle);
            return match previous {
                Interaction::Dragging {
                    window, preview, ..
                }
                | Interaction::Resizing {
                    window, preview, ..
                } => self.commit_preview(window, preview),
                Interaction::Action { node } if self.action_at(x, y, primitives) == Some(node) => {
                    (Damage::EMPTY, false, Some(node))
                }
                _ => (Damage::EMPTY, false, None),
            };
        }
        let non_window_hit = primitives
            .iter()
            .rev()
            .filter_map(|primitive| {
                let info = primitive.info();
                if info.window.is_some() {
                    return None;
                }
                let bounds = self.project(info)?;
                contains(bounds, x, y).then_some(info)
            })
            .find(|info| info.role != NodeRole::Normal);
        let hit = non_window_hit.or_else(|| {
            self.windows[..self.count]
                .iter()
                .rev()
                .filter(|window| window.visible)
                .find_map(|window| {
                    primitives
                        .iter()
                        .rev()
                        .filter(|primitive| primitive.info().window == Some(window.id))
                        .filter_map(|primitive| {
                            let info = primitive.info();
                            let bounds = self.project(info)?;
                            contains(bounds, x, y).then_some(info)
                        })
                        .find(|info| info.role != NodeRole::Normal)
                })
        });
        let Some(info) = hit else {
            return (Damage::EMPTY, false, None);
        };
        if info.role == NodeRole::Restore && info.window.is_none() {
            let (damage, geometry) = self.restore_focused();
            return (damage, geometry, None);
        }
        let Some(window) = info.window else {
            return (Damage::EMPTY, false, None);
        };
        self.focused = Some(window);
        let Some(index) = self.index(window) else {
            return (Damage::EMPTY, false, None);
        };
        self.raise(index);
        let index = self.index(window).unwrap_or(index);
        let geometry = self.windows[index].geometry;
        match info.role {
            NodeRole::Close | NodeRole::Minimize => {
                self.windows[index].visible = false;
                (Damage::one(geometry), true, None)
            }
            NodeRole::Maximize => {
                let (damage, geometry) = self.toggle_maximize(index);
                (damage, geometry, None)
            }
            NodeRole::TitleBar if !self.windows[index].maximized => {
                self.interaction = Interaction::Dragging {
                    window,
                    offset_x: x.saturating_sub(geometry.x1),
                    offset_y: y.saturating_sub(geometry.y1),
                    preview: geometry,
                };
                (Damage::EMPTY, false, None)
            }
            NodeRole::Window => {
                let edges = resize_edges(geometry, x, y);
                if edges != 0 && !self.windows[index].maximized {
                    self.interaction = Interaction::Resizing {
                        window,
                        initial: geometry,
                        pointer_x: x,
                        pointer_y: y,
                        edges,
                        preview: geometry,
                    };
                }
                (Damage::EMPTY, false, None)
            }
            NodeRole::Action => {
                self.interaction = Interaction::Action { node: info.node };
                (Damage::EMPTY, false, None)
            }
            _ => (Damage::EMPTY, false, None),
        }
    }

    pub fn pointer_moved(&mut self, x: usize, y: usize) -> (Damage, bool) {
        let (window, previous, next, interaction) = match self.interaction {
            Interaction::Idle => return (Damage::EMPTY, false),
            Interaction::Action { .. } => return (Damage::EMPTY, false),
            Interaction::Dragging {
                window,
                offset_x,
                offset_y,
                preview,
            } => {
                let Some(index) = self.index(window) else {
                    self.interaction = Interaction::Idle;
                    return (Damage::EMPTY, false);
                };
                let current = self.windows[index].geometry;
                let width = current.x2 - current.x1;
                let height = current.y2 - current.y1;
                let left = x
                    .saturating_sub(offset_x)
                    .min(self.width.saturating_sub(width));
                let top = y
                    .saturating_sub(offset_y)
                    .min(self.height.saturating_sub(TASKBAR_HEIGHT + height));
                let next = rectangle(left, top, width, height);
                (
                    window,
                    preview,
                    next,
                    Interaction::Dragging {
                        window,
                        offset_x,
                        offset_y,
                        preview: next,
                    },
                )
            }
            Interaction::Resizing {
                window,
                initial,
                pointer_x,
                pointer_y,
                edges,
                preview,
            } => {
                let next = resized(
                    initial,
                    x,
                    y,
                    pointer_x,
                    pointer_y,
                    edges,
                    self.width,
                    self.height,
                );
                (
                    window,
                    preview,
                    next,
                    Interaction::Resizing {
                        window,
                        initial,
                        pointer_x,
                        pointer_y,
                        edges,
                        preview: next,
                    },
                )
            }
        };
        if self.index(window).is_none() {
            self.interaction = Interaction::Idle;
            return (Damage::EMPTY, false);
        }
        if previous == next {
            return (Damage::EMPTY, false);
        }
        self.interaction = interaction;
        let mut damage = outline_damage(previous);
        damage.merge(outline_damage(next));
        (damage, false)
    }

    fn commit_preview(&mut self, window: NodeId, preview: Rect) -> (Damage, bool, Option<NodeId>) {
        let Some(index) = self.index(window) else {
            return (Damage::EMPTY, false, None);
        };
        let previous = self.windows[index].geometry;
        self.windows[index].geometry = preview;
        self.windows[index].restore = preview;
        (Damage::pair(previous, preview), previous != preview, None)
    }

    fn restore_focused(&mut self) -> (Damage, bool) {
        let Some(index) = self.focused.and_then(|id| self.index(id)) else {
            return (Damage::EMPTY, false);
        };
        self.windows[index].visible = true;
        (Damage::one(self.windows[index].geometry), true)
    }

    fn toggle_maximize(&mut self, index: usize) -> (Damage, bool) {
        let old = self.windows[index].geometry;
        if self.windows[index].maximized {
            self.windows[index].geometry = self.windows[index].restore;
            self.windows[index].maximized = false;
        } else {
            self.windows[index].restore = old;
            self.windows[index].geometry = Rect {
                x1: 0,
                y1: 0,
                x2: self.width,
                y2: self.height.saturating_sub(TASKBAR_HEIGHT),
            };
            self.windows[index].maximized = true;
        }
        (Damage::pair(old, self.windows[index].geometry), true)
    }

    fn index(&self, id: NodeId) -> Option<usize> {
        self.windows[..self.count]
            .iter()
            .position(|window| window.id == id)
    }

    fn action_at(&self, x: usize, y: usize, primitives: &[Primitive]) -> Option<NodeId> {
        self.windows[..self.count]
            .iter()
            .rev()
            .filter(|window| window.visible)
            .find_map(|window| {
                primitives
                    .iter()
                    .rev()
                    .filter(|primitive| primitive.info().window == Some(window.id))
                    .find_map(|primitive| {
                        let info = primitive.info();
                        (info.role == NodeRole::Action
                            && self
                                .project(info)
                                .is_some_and(|bounds| contains(bounds, x, y)))
                        .then_some(info.node)
                    })
            })
    }

    fn raise(&mut self, index: usize) {
        if index >= self.count || index + 1 == self.count {
            return;
        }
        let selected = self.windows[index];
        self.windows.copy_within(index + 1..self.count, index);
        self.windows[self.count - 1] = selected;
    }
}

fn scaled(source: Rect, base: Rect, target: Rect) -> Rect {
    Rect {
        x1: scale(source.x1, base.x1, base.x2, target.x1, target.x2),
        y1: scale(source.y1, base.y1, base.y2, target.y1, target.y2),
        x2: scale(source.x2, base.x1, base.x2, target.x1, target.x2),
        y2: scale(source.y2, base.y1, base.y2, target.y1, target.y2),
    }
}

fn scale(
    value: usize,
    base_start: usize,
    base_end: usize,
    target_start: usize,
    target_end: usize,
) -> usize {
    let base_span = base_end.saturating_sub(base_start);
    let target_span = target_end.saturating_sub(target_start);
    if base_span == 0 {
        return target_start;
    }
    target_start
        .saturating_add(value.saturating_sub(base_start).saturating_mul(target_span) / base_span)
}

fn rectangle(x: usize, y: usize, width: usize, height: usize) -> Rect {
    Rect {
        x1: x,
        y1: y,
        x2: x.saturating_add(width),
        y2: y.saturating_add(height),
    }
}

fn outline_damage(rectangle: Rect) -> Damage {
    const WIDTH: usize = 2;
    let mut damage = Damage::EMPTY;
    damage.push(Rect {
        y2: rectangle.y1.saturating_add(WIDTH).min(rectangle.y2),
        ..rectangle
    });
    damage.push(Rect {
        y1: rectangle.y2.saturating_sub(WIDTH).max(rectangle.y1),
        ..rectangle
    });
    damage.push(Rect {
        x2: rectangle.x1.saturating_add(WIDTH).min(rectangle.x2),
        ..rectangle
    });
    damage.push(Rect {
        x1: rectangle.x2.saturating_sub(WIDTH).max(rectangle.x1),
        ..rectangle
    });
    damage
}

fn clamped(value: Rect, width: usize, height: usize) -> Rect {
    let available_height = height.saturating_sub(TASKBAR_HEIGHT);
    let window_width = value.x2.saturating_sub(value.x1).min(width);
    let window_height = value.y2.saturating_sub(value.y1).min(available_height);
    rectangle(
        value.x1.min(width.saturating_sub(window_width)),
        value.y1.min(available_height.saturating_sub(window_height)),
        window_width,
        window_height,
    )
}

fn contains(rectangle: Rect, x: usize, y: usize) -> bool {
    x >= rectangle.x1 && x < rectangle.x2 && y >= rectangle.y1 && y < rectangle.y2
}

fn resize_edges(rectangle: Rect, x: usize, y: usize) -> u8 {
    u8::from(x < rectangle.x1.saturating_add(RESIZE_EDGE))
        | u8::from(x.saturating_add(RESIZE_EDGE) >= rectangle.x2) << 1
        | u8::from(y < rectangle.y1.saturating_add(RESIZE_EDGE)) << 2
        | u8::from(y.saturating_add(RESIZE_EDGE) >= rectangle.y2) << 3
}

fn resized(
    initial: Rect,
    x: usize,
    y: usize,
    px: usize,
    py: usize,
    edges: u8,
    width: usize,
    height: usize,
) -> Rect {
    let dx = x as isize - px as isize;
    let dy = y as isize - py as isize;
    let mut next = initial;
    if edges & 1 != 0 {
        next.x1 = shifted(initial.x1, dx, initial.x2.saturating_sub(MINIMUM_WIDTH));
    }
    if edges & 2 != 0 {
        next.x2 = shifted(initial.x2, dx, width).max(next.x1.saturating_add(MINIMUM_WIDTH));
    }
    if edges & 4 != 0 {
        next.y1 = shifted(initial.y1, dy, initial.y2.saturating_sub(MINIMUM_HEIGHT));
    }
    if edges & 8 != 0 {
        next.y2 = shifted(initial.y2, dy, height.saturating_sub(TASKBAR_HEIGHT))
            .max(next.y1.saturating_add(MINIMUM_HEIGHT));
    }
    next.x2 = next.x2.min(width);
    next.y2 = next.y2.min(height.saturating_sub(TASKBAR_HEIGHT));
    next
}

fn shifted(value: usize, delta: isize, maximum: usize) -> usize {
    value.saturating_add_signed(delta).min(maximum)
}
