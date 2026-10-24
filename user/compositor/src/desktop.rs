//! 窗口栈：哪些窗口在屏幕上、各在什么位置、谁在最上。纯逻辑，不碰 DRM 或协议。

use wayland_wire::wire::ObjectId;

use crate::wayland::ClientId;

/// 一个已映射窗口的当前几何（物理像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub client: ClientId,
    pub surface: ObjectId,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
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
            });
        }
        self.order != before
    }

    /// 活动窗口：栈顶。把活动状态放在栈序里，就不会出现"栈序与焦点不一致"的第二份状态。
    pub fn active(&self) -> Option<(ClientId, ObjectId)> {
        self.order
            .last()
            .map(|placed| (placed.client, placed.surface))
    }

    /// 把窗口提到最上层（同时成为活动窗口）；窗口不存在或已在最上层返回 `false`。
    pub fn raise(&mut self, client: ClientId, surface: ObjectId) -> bool {
        let Some(index) = self
            .order
            .iter()
            .position(|placed| placed.client == client && placed.surface == surface)
        else {
            return false;
        };
        if index + 1 == self.order.len() {
            return false;
        }
        let placed = self.order.remove(index);
        self.order.push(placed);
        true
    }

    /// 最上层窗口里包含 `(x, y)` 的那个。
    pub fn window_at(&self, x: i32, y: i32) -> Option<&Placement> {
        self.order.iter().rev().find(|placed| {
            (placed.x..placed.x + placed.width).contains(&x)
                && (placed.y..placed.y + placed.height).contains(&y)
        })
    }
}

#[cfg(test)]
mod tests;
