//! Seat 输入事件路由；Session 仍独占焦点、抓取与安全注意状态，模块不另存镜像。
use super::{
    BUTTON_LEFT, BUTTON_RIGHT, ClientId, KeyboardInteractivity, Layer, PointerGrab, Session,
};
use crate::input::Event;
use std::io;
impl Session {
    pub(super) fn on_input(&mut self, event: Event, physical: bool) -> io::Result<()> {
        let time = self.now_ms();
        let scale = self.scale as i32;
        match event {
            Event::PointerMoved { x, y } => {
                self.scanout.move_cursor((x, y))?;
                if !std::mem::replace(&mut self.saw_pointer, true) {
                    println!("compositor: first pointer event {x},{y}");
                }
                if self.server.dragging() {
                    let hit = self.hit_test(&self.layer_boxes(), (x, y));
                    self.route_drag(hit, (x, y), time, true);
                    return Ok(());
                }
                if let Some(menu) = &mut self.window_menu {
                    self.dirty |= menu.hover_at((x, y));
                } else if self.grab.is_some() {
                    self.drag((x, y));
                } else {
                    let boxes = self.layer_boxes();
                    let grabbed = self.pointer_grab.and_then(|grab| {
                        self.locate_target(&boxes, grab.target)
                            .filter(|hit| hit.generation == grab.generation)
                    });
                    if grabbed.is_none() {
                        self.pointer_grab = None;
                    }
                    let hit = grabbed.or_else(|| self.hit_test(&boxes, (x, y)));
                    self.seat
                        .update_pointer(&mut self.server, hit, (x, y), scale, time, true);
                }
            }
            Event::Button {
                code,
                pressed,
                position,
            } => {
                let first = pressed && self.pressed_buttons.is_empty();
                // 在任何提前返回之前维护按钮集合，释放才不会漏掉而让抓取悬挂。
                if pressed {
                    self.pressed_buttons.insert(code);
                } else {
                    self.pressed_buttons.remove(&code);
                    if self.pressed_buttons.is_empty() {
                        self.pointer_grab = None;
                    }
                }
                if self.server.dragging() {
                    if !pressed && self.pressed_buttons.is_empty() {
                        self.server.drop_drag(time);
                        self.dirty = true;
                    }
                    return Ok(());
                }
                // 点击别处取消 Alt+Tab 面板；窗口仍留在原来的位置。
                if pressed && self.switcher.take().is_some() {
                    self.dirty = true;
                }
                if self.window_menu.is_some() {
                    if pressed && matches!(code, BUTTON_LEFT | BUTTON_RIGHT) {
                        self.menu_click(position)?;
                    }
                    return Ok(());
                }
                if self.grab.as_ref().is_some_and(|active| active.sticky) {
                    if code == BUTTON_LEFT && pressed {
                        self.end_grab()?;
                    }
                    return Ok(());
                }
                if code == BUTTON_LEFT && !pressed && self.grab.is_some() {
                    self.end_grab()?;
                } else if pressed && matches!(code, BUTTON_LEFT | BUTTON_RIGHT) {
                    // 点在弹出层之外：持有 grab 的弹出层结束（客户端随后销毁它们）。
                    let point = position;
                    if code == BUTTON_LEFT && self.popup_hit(point, self.stack.windows()).is_none()
                    {
                        let clients: Vec<ClientId> = self
                            .server
                            .popups()
                            .into_iter()
                            .map(|(client, _)| client)
                            .collect();
                        for client in clients {
                            self.server.dismiss_grabbed_popups(client);
                        }
                    }
                    // 单击应用窗口：激活并置顶。外壳 surface 不是窗口，点击它不改变活动窗口。
                    let point = position;
                    let boxes = self.layer_boxes();
                    let hit = self.hit_test(&boxes, point);
                    let root = hit.and_then(|hit| self.root_target(hit.target));
                    let layer = root.and_then(|root| {
                        boxes
                            .iter()
                            .find(|layer| (layer.client, layer.surface) == root)
                    });
                    if !self.secure_pending {
                        match layer {
                            Some(layer) if layer.keyboard == KeyboardInteractivity::OnDemand => {
                                self.keyboard_layer = Some((layer.client, layer.surface));
                            }
                            // 独占层临时接收键盘。关闭时应恢复此前点击选中的 OnDemand 表面。
                            Some(layer) if layer.keyboard == KeyboardInteractivity::Exclusive => {}
                            _ if code == BUTTON_LEFT => self.keyboard_layer = None,
                            _ => {}
                        }
                    }
                    if code == BUTTON_LEFT
                        && !self.secure_pending
                        && !layer.is_some_and(|layer| layer.layer >= Layer::Top)
                        && let Some(target) =
                            root.filter(|target| self.stack.placement(target.0, target.1).is_some())
                        && self.stack.raise(target.0, target.1)
                    {
                        self.dirty = true;
                    }
                }
                if !self.secure_pending {
                    let target = self.seat.pointer_focus();
                    let serial = self.seat.button(&mut self.server, code, pressed, time);
                    if first {
                        self.pointer_grab =
                            target.zip(serial).map(|(target, serial)| PointerGrab {
                                target,
                                generation: self
                                    .root_target(target)
                                    .and_then(|root| self.stack.placement(root.0, root.1))
                                    .map(|placed| placed.generation),
                                serial,
                                position,
                            });
                    }
                }
            }
            Event::Wheel {
                vertical,
                horizontal,
            } => {
                // 窗口菜单、拖动抓取期间滚轮不交给应用。
                if self.window_menu.is_none()
                    && self.grab.is_none()
                    && !self.secure_pending
                    && !self.server.dragging()
                {
                    self.seat
                        .wheel(&mut self.server, vertical, horizontal, time);
                }
            }
            Event::Key { code, pressed } => {
                if !std::mem::replace(&mut self.saw_key, true) {
                    println!("compositor: first key event");
                }
                // SAS 只识别硬件输入；登录背景始终映射，因此使用标准 wl_keyboard 焦点与按键事件。
                // 先撤销抓取，防止安全界面出现后旧窗口仍占有鼠标。
                if physical {
                    self.sas_modifiers
                        .apply(u32::from(code), i32::from(pressed));
                }
                if physical && super::Seat::secure_attention(self.sas_modifiers, code, pressed) {
                    self.server.cancel_drag();
                    self.close_menu();
                    if self.grab.is_some() {
                        self.end_grab()?;
                    }
                    self.secure_pending = self.logon_client.is_some();
                    self.keyboard_layer = self
                        .layer_boxes()
                        .iter()
                        .find(|layer| Some(layer.client) == self.logon_client)
                        .map(|layer| (layer.client, layer.surface));
                }
                if code == 1 && pressed && self.server.dragging() {
                    self.server.cancel_drag();
                    self.dirty = true;
                }
                if self.modal_key(code, pressed)? {
                    return Ok(());
                }
                // 键盘焦点必须先于这次按键同步：刚激活的窗口应该收到触发它之后的第一个键。
                let exclusive = self.exclusive_keyboard(&self.layer_boxes());
                if exclusive.is_none()
                    && !self.secure_pending
                    && self.seat.desktop_shortcut(code, pressed)
                {
                    self.keyboard_layer = self.desktop_keyboard(&self.layer_boxes());
                }
                self.dirty |= self.seat.sync_keyboard(
                    &mut self.server,
                    &self.stack,
                    scale,
                    if self.secure_pending {
                        self.keyboard_layer
                    } else {
                        exclusive.or(self.keyboard_layer)
                    },
                );
                // Alt 的 evdev 键码：左 56、右 100。
                if !pressed && matches!(code, 56 | 100) {
                    self.commit_switcher();
                }
                let shortcut = self.seat.key(
                    &mut self.server,
                    code,
                    pressed,
                    time,
                    exclusive.is_none()
                        && !self.secure_pending
                        && !(code == 62 && self.keyboard_layer.is_some()),
                );
                // 登录屏独占键盘时，窗口管理快捷键（Alt+Tab、Alt+F4…）不能越过它。
                if exclusive.is_none() {
                    self.on_shortcut(shortcut);
                }
            }
        }
        Ok(())
    }
}
