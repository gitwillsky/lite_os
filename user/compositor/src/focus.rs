//! 输入焦点策略：指针焦点跟随光标下的窗口，键盘焦点跟随活动窗口；把输入事件投递给对应客户端。

use keyboard::{Modifiers, xkb};
use wayland_wire::wire::{Fixed, ObjectId};

use crate::{
    desktop::{Placement, Stack},
    wayland::{ClientId, Server},
};

type Target = (ClientId, ObjectId);

/// 按键状态：修饰键、此刻按住的键，以及被合成器快捷键吞掉、释放时也不转发的键。
#[derive(Default)]
pub struct KeyTracker {
    modifiers: Modifiers,
    pressed: Vec<u32>,
    consumed: Vec<u32>,
}

impl KeyTracker {
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    pub fn pressed(&self) -> &[u32] {
        &self.pressed
    }

    /// 折入一次按键事件，返回修饰键状态是否变化。
    pub fn track(&mut self, code: u32, pressed: bool) -> bool {
        let before = xkb::modifier_masks(self.modifiers);
        self.modifiers.apply(code, i32::from(pressed));
        if pressed {
            if !self.pressed.contains(&code) {
                self.pressed.push(code);
            }
        } else {
            self.pressed.retain(|key| *key != code);
        }
        xkb::modifier_masks(self.modifiers) != before
    }

    /// 标记 `code` 已被合成器快捷键处理：它的释放事件不再转发。
    pub fn consume(&mut self, code: u32) {
        if !self.consumed.contains(&code) {
            self.consumed.push(code);
        }
    }

    /// 释放事件是否应被吞掉（并清除该标记）。
    pub fn take_consumed(&mut self, code: u32) -> bool {
        let before = self.consumed.len();
        self.consumed.retain(|key| *key != code);
        self.consumed.len() != before
    }
}

/// 指针在窗口内的局部坐标，单位是客户端的逻辑像素：物理像素偏移除以输出 scale。
///
/// 窗口内容总是被放大到输出 scale，所以一个客户端逻辑像素恰好占 `scale` 个物理像素。
pub fn local_position(placement: &Placement, pointer: (i32, i32), scale: i32) -> (Fixed, Fixed) {
    let scale = f64::from(scale.max(1));
    (
        Fixed::from_f64(f64::from(pointer.0 - placement.x) / scale),
        Fixed::from_f64(f64::from(pointer.1 - placement.y) / scale),
    )
}

/// 合成器快捷键的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    None,
    /// Alt+F4：请求关闭活动窗口。
    CloseActive,
}

const KEY_F4: u32 = 62;

#[derive(Default)]
pub struct Seat {
    serial: u32,
    pointer_focus: Option<Target>,
    keyboard_focus: Option<Target>,
    keys: KeyTracker,
}

impl Seat {
    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    /// 指针移动或窗口布局变化后重新判定指针焦点，并向新焦点发送 enter/motion。
    ///
    /// # Parameters
    ///
    /// - `moved`: 指针位置是否真的变了；只有焦点不变且位置变了才发 `motion`。
    pub fn update_pointer(
        &mut self,
        server: &mut Server,
        stack: &Stack,
        pointer: (i32, i32),
        scale: i32,
        time: u32,
        moved: bool,
    ) {
        let hit = stack.window_at(pointer.0, pointer.1).copied();
        let target = hit.map(|placement| (placement.client, placement.surface));
        if target != self.pointer_focus {
            if let Some((client, surface)) = self.pointer_focus {
                let serial = self.next_serial();
                server.with_client(client, |client| client.pointer_leave(serial, surface));
            }
            if let (Some((client, surface)), Some(placement)) = (target, hit) {
                let serial = self.next_serial();
                let (x, y) = local_position(&placement, pointer, scale);
                server.with_client(client, |client| client.pointer_enter(serial, surface, x, y));
            }
            self.pointer_focus = target;
        } else if let (true, Some((client, _)), Some(placement)) = (moved, target, hit) {
            let (x, y) = local_position(&placement, pointer, scale);
            server.with_client(client, |client| client.pointer_motion(time, x, y));
        }
    }

    pub fn button(&mut self, server: &mut Server, code: u16, pressed: bool, time: u32) {
        if let Some((client, _)) = self.pointer_focus {
            let serial = self.next_serial();
            server.with_client(client, |client| {
                client.pointer_button(serial, time, u32::from(code), pressed)
            });
        }
    }

    /// 键盘焦点跟随活动窗口：旧窗口 leave 并标记未激活，新窗口标记已激活并 enter。
    pub fn sync_keyboard(&mut self, server: &mut Server, stack: &Stack) {
        let active = stack.active();
        if active == self.keyboard_focus {
            return;
        }
        if let Some((client, surface)) = self.keyboard_focus {
            let serial = self.next_serial();
            server.with_client(client, |client| {
                client.keyboard_leave(serial, surface);
                client.configure(surface, 0, 0, false);
            });
        }
        if let Some((client, surface)) = active {
            let serial = self.next_serial();
            let (depressed, locked) = xkb::modifier_masks(self.keys.modifiers());
            let pressed = self.keys.pressed().to_vec();
            server.with_client(client, |client| {
                client.configure(surface, 0, 0, true);
                client.keyboard_enter(serial, surface, &pressed);
                client.keyboard_modifiers(serial, depressed, locked);
            });
        }
        self.keyboard_focus = active;
    }

    /// 处理一次按键：更新状态、识别合成器快捷键，其余转发给键盘焦点。
    pub fn key(&mut self, server: &mut Server, code: u16, pressed: bool, time: u32) -> Shortcut {
        let code = u32::from(code);
        let modifiers_changed = self.keys.track(code, pressed);
        if pressed && code == KEY_F4 && self.keys.modifiers().alt {
            self.keys.consume(code);
            return Shortcut::CloseActive;
        }
        if !pressed && self.keys.take_consumed(code) {
            return Shortcut::None;
        }
        if let Some((client, _)) = self.keyboard_focus {
            let (depressed, locked) = xkb::modifier_masks(self.keys.modifiers());
            let serial = self.next_serial();
            server.with_client(client, |client| {
                // 修饰键状态先于触发它的按键事件到达：客户端按这个顺序解释 keysym。
                if modifiers_changed && pressed {
                    client.keyboard_modifiers(serial, depressed, locked);
                }
                client.keyboard_key(serial, time, code, pressed);
                if modifiers_changed && !pressed {
                    client.keyboard_modifiers(serial, depressed, locked);
                }
            });
        }
        Shortcut::None
    }
}

#[cfg(test)]
mod tests;
