//! 窗口栈：哪些窗口在屏幕上、各在什么位置、谁在最上。纯逻辑，不碰 DRM 或协议。

use wayland_wire::wire::ObjectId;

use crate::wayland::ClientId;

/// 窗口的显示模式；最小化与模式正交（最小化的最大化窗口恢复后仍是最大化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Maximized,
    Fullscreen,
}

/// `(x, y, 宽, 高)`，物理像素。
pub type Geometry = (i32, i32, i32, i32);

/// 一个已映射窗口的当前几何（物理像素）与状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub client: ClientId,
    pub surface: ObjectId,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub mode: Mode,
    pub minimized: bool,
    /// 进入最大化/全屏之前的几何；还原时用它，缺少它窗口无法回到原来的位置与大小。
    pub restore: Option<Geometry>,
    /// `set_parent` 指定的父窗口（同一连接的 `wl_surface`）。xdg-shell 规定子窗口始终在父窗口之上，
    /// 所以父窗口被提到最上层时它的子窗口要跟着提上去。
    pub parent: Option<ObjectId>,
}

/// 新窗口的初始位置策略（合成器自己的窗口管理策略；Wayland 客户端不能指定位置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Initial {
    /// 层叠：普通可调整大小的窗口。
    Cascade,
    /// 屏幕居中：固定尺寸的对话框类窗口（没有父窗口）。
    Center,
    /// 居中于父窗口（`set_parent`）；父窗口不存在时退回屏幕居中。
    CenterOn(ObjectId),
}

/// 窗口栈，`order` 末尾是最上层。
#[derive(Default)]
pub struct Stack {
    order: Vec<Placement>,
    /// 新窗口层叠放置的计数；每个新窗口比上一个右下偏移一个标题栏高度。
    cascaded: i32,
}

/// 层叠放置的步长，逻辑像素（标题栏高度）。
const CASCADE_STEP: i32 = 19;
/// 第一个窗口的左上角，逻辑像素。
const CASCADE_ORIGIN: i32 = 40;

impl Stack {
    /// 自底向上的窗口。
    pub fn windows(&self) -> &[Placement] {
        &self.order
    }

    /// 与当前已映射的窗口集合对账：消失的移除，新出现的按层叠规则放在最上层，尺寸变化的更新。
    ///
    /// # Parameters
    ///
    /// - `mapped`: `(连接, surface, 物理宽, 物理高, 初始位置策略)`；策略只对新出现的窗口生效，已映射的窗口不会被移动。
    /// - `screen`: 屏幕物理尺寸；窗口至少保留一个标题栏在屏内。
    /// - `scale`: 输出 scale，用来把逻辑层叠步长换成物理像素。
    ///
    /// # Returns
    ///
    /// 栈是否发生任何变化（窗口增、删或尺寸变）。
    pub fn sync(
        &mut self,
        mapped: &[(ClientId, ObjectId, i32, i32, Initial)],
        screen: (i32, i32),
        scale: i32,
    ) -> bool {
        let before = self.order.clone();
        self.order.retain(|placed| {
            mapped
                .iter()
                .any(|(client, surface, ..)| *client == placed.client && *surface == placed.surface)
        });
        for placed in &mut self.order {
            if let Some((_, _, width, height, initial)) =
                mapped.iter().find(|(client, surface, ..)| {
                    *client == placed.client && *surface == placed.surface
                })
            {
                (placed.width, placed.height) = (*width, *height);
                placed.parent = match initial {
                    Initial::CenterOn(parent) => Some(*parent),
                    _ => None,
                };
            }
        }
        for &(client, surface, width, height, initial) in mapped {
            if self
                .order
                .iter()
                .any(|placed| placed.client == client && placed.surface == surface)
            {
                continue;
            }
            let centered = match initial {
                Initial::Cascade => None,
                Initial::Center => Some((0, 0, screen.0, screen.1)),
                Initial::CenterOn(parent) => Some(
                    self.order
                        .iter()
                        .find(|placed| placed.client == client && placed.surface == parent)
                        .map_or((0, 0, screen.0, screen.1), |placed| {
                            (placed.x, placed.y, placed.width, placed.height)
                        }),
                ),
            };
            if let Some((x, y, w, h)) = centered {
                // 居中后夹进屏幕：窗口比参照区大或参照区靠边时不会跑出屏幕。
                let at = |origin: i32, extent: i32, size: i32, limit: i32| {
                    (origin + (extent - size) / 2).clamp(0, (limit - size).max(0))
                };
                self.order.push(Placement {
                    client,
                    surface,
                    x: at(x, w, width, screen.0),
                    y: at(y, h, height, screen.1),
                    width,
                    height,
                    mode: Mode::Normal,
                    minimized: false,
                    restore: None,
                    parent: match initial {
                        Initial::CenterOn(parent) => Some(parent),
                        _ => None,
                    },
                });
                continue;
            }
            let step = (CASCADE_ORIGIN + self.cascaded * CASCADE_STEP) * scale;
            // 层叠到屏幕右下角放不下时回到起点，窗口不会越来越偏出屏幕。
            let fits = step + width < screen.0 && step + height < screen.1;
            if !fits {
                self.cascaded = 0;
            }
            let offset = if fits { step } else { CASCADE_ORIGIN * scale };
            self.cascaded += 1;
            self.order.push(Placement {
                client,
                surface,
                x: offset,
                y: offset,
                width,
                height,
                mode: Mode::Normal,
                minimized: false,
                restore: None,
                parent: None,
            });
        }
        self.order != before
    }

    fn find(&mut self, client: ClientId, surface: ObjectId) -> Option<&mut Placement> {
        self.order
            .iter_mut()
            .find(|placed| placed.client == client && placed.surface == surface)
    }

    /// 活动窗口：栈中最上层的未最小化窗口。把活动状态放在栈序里，就不会出现"栈序与焦点不一致"的第二份状态。
    pub fn active(&self) -> Option<(ClientId, ObjectId)> {
        self.order
            .iter()
            .rev()
            .find(|placed| !placed.minimized)
            .map(|placed| (placed.client, placed.surface))
    }

    /// 激活窗口：恢复最小化、提到最上层；窗口不存在或本来就是活动窗口返回 `false`。
    pub fn raise(&mut self, client: ClientId, surface: ObjectId) -> bool {
        let Some(index) = self
            .order
            .iter()
            .position(|placed| placed.client == client && placed.surface == surface)
        else {
            return false;
        };
        let was_minimized = std::mem::replace(&mut self.order[index].minimized, false);
        if index + 1 == self.order.len() {
            return was_minimized;
        }
        let placed = self.order.remove(index);
        self.order.push(placed);
        // 子窗口（对话框）跟随父窗口留在它上面，保持原有的相对次序。
        let (mut children, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.order)
            .into_iter()
            .partition(|child| child.client == client && child.parent == Some(surface));
        self.order = rest;
        self.order.append(&mut children);
        true
    }

    /// 把窗口放到最底层（Alt+Esc）；窗口不存在或本来就在最底层返回 `false`。
    pub fn lower(&mut self, client: ClientId, surface: ObjectId) -> bool {
        let Some(index) = self
            .order
            .iter()
            .position(|placed| placed.client == client && placed.surface == surface)
        else {
            return false;
        };
        if index == 0 {
            return false;
        }
        let placed = self.order.remove(index);
        self.order.insert(0, placed);
        true
    }

    /// 最小化窗口：不再绘制、不参与命中与激活。
    pub fn minimize(&mut self, client: ClientId, surface: ObjectId) -> bool {
        match self.find(client, surface) {
            Some(placed) if !placed.minimized => {
                placed.minimized = true;
                true
            }
            _ => false,
        }
    }

    /// 切换窗口模式。
    ///
    /// 进入最大化时窗口铺满工作区 `work`（扣除任务栏等独占区），进入全屏时铺满整个输出 `full`；
    /// 两者都保存进入之前的几何，回到普通模式时恢复。
    ///
    /// # Returns
    ///
    /// 需要通知客户端的新物理尺寸；模式没有变化或窗口不存在返回 `None`。
    pub fn set_mode(
        &mut self,
        client: ClientId,
        surface: ObjectId,
        target: Mode,
        work: Geometry,
        full: Geometry,
    ) -> Option<(i32, i32)> {
        let placed = self.find(client, surface)?;
        if placed.mode == target {
            return None;
        }
        match target {
            Mode::Normal => {
                let (x, y, width, height) = placed.restore.take()?;
                (placed.x, placed.y, placed.width, placed.height) = (x, y, width, height);
            }
            Mode::Maximized | Mode::Fullscreen => {
                if placed.mode == Mode::Normal {
                    placed.restore = Some((placed.x, placed.y, placed.width, placed.height));
                }
                let area = if target == Mode::Maximized {
                    work
                } else {
                    full
                };
                (placed.x, placed.y, placed.width, placed.height) = area;
            }
        }
        placed.mode = target;
        Some((placed.width, placed.height))
    }

    /// 工作区变化（任务栏出现、消失、改变高度）后，让最大化与全屏窗口重新铺满各自的区域。
    ///
    /// # Returns
    ///
    /// 几何发生变化的窗口及其新的物理尺寸，调用者据此向客户端发 configure。
    pub fn refit(
        &mut self,
        work: Geometry,
        full: Geometry,
    ) -> Vec<(ClientId, ObjectId, (i32, i32))> {
        let mut changed = Vec::new();
        for placed in &mut self.order {
            let area = match placed.mode {
                Mode::Normal => continue,
                Mode::Maximized => work,
                Mode::Fullscreen => full,
            };
            if (placed.x, placed.y, placed.width, placed.height) != area {
                (placed.x, placed.y, placed.width, placed.height) = area;
                changed.push((placed.client, placed.surface, (area.2, area.3)));
            }
        }
        changed
    }

    /// 移动窗口左上角；最大化与全屏窗口不能移动。
    pub fn move_to(&mut self, client: ClientId, surface: ObjectId, x: i32, y: i32) -> bool {
        match self.find(client, surface) {
            Some(placed) if placed.mode == Mode::Normal && (placed.x, placed.y) != (x, y) => {
                (placed.x, placed.y) = (x, y);
                true
            }
            _ => false,
        }
    }

    pub fn placement(&self, client: ClientId, surface: ObjectId) -> Option<&Placement> {
        self.order
            .iter()
            .find(|placed| placed.client == client && placed.surface == surface)
    }
}

#[cfg(test)]
mod tests;
