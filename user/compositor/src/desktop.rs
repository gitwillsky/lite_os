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
    /// - `mapped`: `(连接, surface, 物理宽, 物理高)`。
    /// - `screen`: 屏幕物理尺寸；窗口至少保留一个标题栏在屏内。
    /// - `scale`: 输出 scale，用来把逻辑层叠步长换成物理像素。
    ///
    /// # Returns
    ///
    /// 栈是否发生任何变化（窗口增、删或尺寸变）。
    pub fn sync(
        &mut self,
        mapped: &[(ClientId, ObjectId, i32, i32)],
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
            if let Some((.., width, height)) = mapped.iter().find(|(client, surface, ..)| {
                *client == placed.client && *surface == placed.surface
            }) {
                (placed.width, placed.height) = (*width, *height);
            }
        }
        for &(client, surface, width, height) in mapped {
            if self
                .order
                .iter()
                .any(|placed| placed.client == client && placed.surface == surface)
            {
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

    /// Alt+Tab 的目标：最久没有被激活的窗口（栈最底层）。
    ///
    /// 只有一个窗口时，仅当它被最小化才有切换的意义（恢复它）；否则没有可切换的对象。
    pub fn cycle_target(&self) -> Option<(ClientId, ObjectId)> {
        let bottom = self.order.first()?;
        (self.order.len() >= 2 || bottom.minimized).then_some((bottom.client, bottom.surface))
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
    /// 进入最大化/全屏时保存当前几何并让窗口铺满 `screen`；回到普通模式时恢复保存的几何。
    ///
    /// # Returns
    ///
    /// 需要通知客户端的新物理尺寸；模式没有变化或窗口不存在返回 `None`。
    pub fn set_mode(
        &mut self,
        client: ClientId,
        surface: ObjectId,
        target: Mode,
        screen: (i32, i32),
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
                (placed.x, placed.y, placed.width, placed.height) = (0, 0, screen.0, screen.1);
            }
        }
        placed.mode = target;
        Some((placed.width, placed.height))
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

    /// 最上层可见窗口里包含 `(x, y)` 的那个。
    pub fn window_at(&self, x: i32, y: i32) -> Option<&Placement> {
        self.order.iter().rev().find(|placed| {
            !placed.minimized
                && (placed.x..placed.x + placed.width).contains(&x)
                && (placed.y..placed.y + placed.height).contains(&y)
        })
    }
}

#[cfg(test)]
mod tests;
