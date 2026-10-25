//! 输入焦点策略：指针焦点跟随光标下的窗口，键盘焦点跟随活动窗口；把输入事件投递给对应客户端。

use keyboard::{Modifiers, xkb};
use wayland_wire::wire::{Fixed, ObjectId};

use crate::{
    desktop::{Mode, Stack},
    wayland::{ClientId, Server, ToplevelState},
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

/// 指针下的 surface：所属客户端、surface，以及它左上角的物理坐标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub target: Target,
    pub origin: (i32, i32),
}

/// 指针在窗口内的局部坐标，单位是客户端的逻辑像素：物理像素偏移除以输出 scale。
///
/// 窗口内容总是被放大到输出 scale，所以一个客户端逻辑像素恰好占 `scale` 个物理像素。
pub fn local_position(origin: (i32, i32), pointer: (i32, i32), scale: i32) -> (Fixed, Fixed) {
    let scale = f64::from(scale.max(1));
    (
        Fixed::from_f64(f64::from(pointer.0 - origin.0) / scale),
        Fixed::from_f64(f64::from(pointer.1 - origin.1) / scale),
    )
}

/// 合成器快捷键的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    None,
    /// Alt+F4：请求关闭活动窗口。
    CloseActive,
    /// Alt+Tab / Alt+Shift+Tab：在切换面板里选下一个 / 上一个窗口；松开 Alt 才激活。
    CycleWindows {
        reverse: bool,
    },
    /// Alt+Esc：把活动窗口放到最底层。
    LowerActive,
    /// Alt+Space：打开活动窗口的系统菜单。
    WindowMenu,
}

const KEY_ESC: u32 = 1;
const KEY_TAB: u32 = 15;
const KEY_SPACE: u32 = 57;
const KEY_F4: u32 = 62;

/// 窗口当前应该收到的 `configure`：`(逻辑宽, 逻辑高, 状态)`。
///
/// 普通窗口的尺寸由客户端决定（0×0），只有明确要求时才带目标尺寸；最大化与全屏窗口带实际尺寸，
/// 否则客户端在失去激活时会把自己缩回默认大小。
///
/// # Parameters
///
/// - `size`: 要求客户端采用的目标物理尺寸（缩放抓取、取消最大化时的还原尺寸）；`None` 按窗口模式决定。
/// - `resizing`: 用户是否正在拖动边框缩放。
pub fn window_configure(
    stack: &Stack,
    target: Target,
    scale: i32,
    size: Option<(i32, i32)>,
    resizing: bool,
) -> (i32, i32, ToplevelState) {
    let placement = stack.placement(target.0, target.1);
    let mode = placement.map_or(Mode::Normal, |placed| placed.mode);
    let state = ToplevelState {
        activated: stack.active() == Some(target),
        maximized: mode == Mode::Maximized,
        fullscreen: mode == Mode::Fullscreen,
        resizing,
    };
    let physical = match (size, mode, placement) {
        (Some(size), ..) => size,
        (None, Mode::Normal, _) | (None, _, None) => return (0, 0, state),
        (None, _, Some(placed)) => (placed.width, placed.height),
    };
    let scale = scale.max(1);
    (physical.0 / scale, physical.1 / scale, state)
}

#[derive(Default)]
pub struct Seat {
    serial: u32,
    pointer_focus: Option<Target>,
    keyboard_focus: Option<Target>,
    keys: KeyTracker,
    /// 最近一次按下且尚未释放的按钮的序号；窗口的 move/resize 请求必须带它，否则视为过期请求。
    button_serial: Option<u32>,
    /// 合成器正在抓取指针（交互式移动/缩放）：客户端看不到指针事件。
    grabbed: bool,
    /// 当前指针焦点收到的 `enter` 序号；`set_cursor` 必须带它才有效。
    pointer_enter_serial: u32,
}

impl Seat {
    /// 仅供硬件输入路径查询 SAS；在 track 当前 Delete 之前读取已按住的 Ctrl/Alt。
    pub fn secure_attention(&self, code: u16, pressed: bool) -> bool {
        code == 111 && pressed && self.keys.modifiers().control && self.keys.modifiers().alt
    }

    /// 已有修饰键与硬件事件组成的桌面快捷键；只有会话装配能选择目标外壳。
    pub fn desktop_shortcut(&self, code: u16, pressed: bool) -> bool {
        let modifiers = self.keys.modifiers();
        pressed
            && ((code == 1 && modifiers.control && !modifiers.alt)
                || (code == 19 && modifiers.super_key))
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    pub fn pointer_focus(&self) -> Option<Target> {
        self.pointer_focus
    }

    pub fn pointer_enter_serial(&self) -> u32 {
        self.pointer_enter_serial
    }

    /// 当前仍按住的按钮的序号。
    pub fn button_serial(&self) -> Option<u32> {
        self.button_serial
    }

    /// 开始抓取：指针对当前焦点客户端就像离开了窗口。
    pub fn begin_grab(&mut self, server: &mut Server) {
        if let Some((client, surface)) = self.pointer_focus.take() {
            let serial = self.next_serial();
            server.with_client(client, |client| client.pointer_leave(serial, surface));
        }
        self.grabbed = true;
    }

    /// 结束抓取；下一次 `update_pointer` 会重新判定并向窗口发 enter。
    pub fn end_grab(&mut self) {
        self.grabbed = false;
    }

    /// 指针移动或窗口布局变化后重新判定指针焦点，并向新焦点发送 enter/motion。
    ///
    /// # Parameters
    ///
    /// - `moved`: 指针位置是否真的变了；只有焦点不变且位置变了才发 `motion`。
    pub fn update_pointer(
        &mut self,
        server: &mut Server,
        hit: Option<Hit>,
        pointer: (i32, i32),
        scale: i32,
        time: u32,
        moved: bool,
    ) {
        if self.grabbed {
            return;
        }
        let target = hit.map(|hit| hit.target);
        if target != self.pointer_focus {
            if let Some((client, surface)) = self.pointer_focus {
                let serial = self.next_serial();
                server.with_client(client, |client| client.pointer_leave(serial, surface));
            }
            if let Some(Hit {
                target: (client, surface),
                origin,
            }) = hit
            {
                let serial = self.next_serial();
                self.pointer_enter_serial = serial;
                let (x, y) = local_position(origin, pointer, scale);
                server.with_client(client, |client| client.pointer_enter(serial, surface, x, y));
            }
            self.pointer_focus = target;
        } else if let (
            true,
            Some(Hit {
                target: (client, _),
                origin,
            }),
        ) = (moved, hit)
        {
            let (x, y) = local_position(origin, pointer, scale);
            server.with_client(client, |client| client.pointer_motion(time, x, y));
        }
    }

    /// 滚轮发给当前指针焦点的客户端；没有焦点时丢弃。
    pub fn wheel(&mut self, server: &mut Server, vertical: i32, horizontal: i32, time: u32) {
        if let Some((client, _)) = self.pointer_focus {
            server.with_client(client, |client| {
                client.pointer_axis(time, vertical, horizontal)
            });
        }
    }

    pub fn button(&mut self, server: &mut Server, code: u16, pressed: bool, time: u32) {
        if let Some((client, _)) = self.pointer_focus {
            let serial = self.next_serial();
            self.button_serial = pressed.then_some(serial);
            server.with_client(client, |client| {
                client.pointer_button(serial, time, u32::from(code), pressed)
            });
        }
    }

    /// 键盘焦点跟随活动窗口：旧窗口 leave 并标记未激活，新窗口标记已激活并 enter。
    ///
    /// `exclusive` 是持有独占键盘交互的外壳 surface（登录屏）；它存在时优先于任何窗口。
    pub fn sync_keyboard(
        &mut self,
        server: &mut Server,
        stack: &Stack,
        scale: i32,
        exclusive: Option<Target>,
    ) {
        let active = exclusive.or_else(|| stack.active());
        if active == self.keyboard_focus {
            return;
        }
        if let Some((client, surface)) = self.keyboard_focus {
            let serial = self.next_serial();
            // 窗口可能刚被最小化或移除；还存在时通知它失去激活，并保留它的最大化等状态。
            let (width, height, state) =
                window_configure(stack, (client, surface), scale, None, false);
            server.with_client(client, |client| {
                client.keyboard_leave(serial, surface);
                client.configure(surface, width, height, state);
            });
        }
        if let Some((client, surface)) = active {
            let serial = self.next_serial();
            let (depressed, locked) = xkb::modifier_masks(self.keys.modifiers());
            let pressed = self.keys.pressed().to_vec();
            let (width, height, state) =
                window_configure(stack, (client, surface), scale, None, false);
            server.with_client(client, |client| {
                client.configure(surface, width, height, state);
                client.keyboard_enter(serial, surface, &pressed);
                client.keyboard_modifiers(serial, depressed, locked);
            });
        }
        self.keyboard_focus = active;
    }

    /// 键盘焦点所在的客户端。
    pub fn keyboard_client(&self) -> Option<ClientId> {
        self.keyboard_focus.map(|(client, _)| client)
    }

    /// 记录一次由合成器模态界面（窗口菜单、键盘移动）消费的按键：只更新按键状态，不转发给任何客户端，
    /// 按下与对应的释放都被吞掉；缺少它，客户端会收到没有按下的释放事件。
    pub fn swallow_key(&mut self, code: u16, pressed: bool) {
        let code = u32::from(code);
        self.keys.track(code, pressed);
        if pressed {
            self.keys.consume(code);
        } else {
            self.keys.take_consumed(code);
        }
    }

    /// 处理一次按键：更新状态、识别合成器快捷键，其余转发给键盘焦点。
    /// `shortcuts=false` 用于外壳独占键盘；否则 Alt+F4 等会被吞掉，登录屏收不到完整按键。
    pub fn key(
        &mut self,
        server: &mut Server,
        code: u16,
        pressed: bool,
        time: u32,
        shortcuts: bool,
    ) -> Shortcut {
        let code = u32::from(code);
        let modifiers_changed = self.keys.track(code, pressed);
        if shortcuts && pressed && self.keys.modifiers().alt {
            let shortcut = match code {
                KEY_F4 => Shortcut::CloseActive,
                KEY_TAB => Shortcut::CycleWindows {
                    reverse: self.keys.modifiers().shift,
                },
                KEY_ESC => Shortcut::LowerActive,
                KEY_SPACE => Shortcut::WindowMenu,
                _ => Shortcut::None,
            };
            if shortcut != Shortcut::None {
                self.keys.consume(code);
                return shortcut;
            }
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
