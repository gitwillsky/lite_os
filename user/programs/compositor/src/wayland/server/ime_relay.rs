//! 一个 seat 的输入法 relay。焦点由 Seat 提供，应用字段与输入法分别拥有各自协议状态。

use super::{ClientId, Server};
use crate::wayland::{
    client::{Client, Data, Fail, Privilege},
    text_input::{Action, Edit},
};
use canvas::Rect;
use wayland_wire::{
    protocol::{
        wl_keyboard, zwp_input_method_keyboard_grab_v2 as grab, zwp_input_method_v2 as method,
        zwp_input_popup_surface_v2 as popup, zwp_text_input_v3 as text,
    },
    wire::ObjectId,
};

type Target = (ClientId, ObjectId);
type Focus = (Target, Option<u64>);

#[derive(Clone, Copy, PartialEq, Eq)]
struct Field {
    object: Target,
    surface: ObjectId,
    epoch: u32,
}

#[derive(Default)]
pub(super) struct Relay {
    focus: Option<Focus>,
    method: Option<Target>,
    field: Option<Field>,
    /// 这次激活发出的第一个 done。上一个字段的迟到事务不能投递给新字段。
    activated: u32,
    revision: Option<u32>,
    rectangle: Option<Rect>,
    popup_rectangle: Option<(ObjectId, Rect)>,
    /// 虚拟键的按下已交付；撤销 grab 时给仍有焦点的应用补齐释放。
    forwarded: Vec<u32>,
    modifiers: Option<(u32, u32)>,
    grab: Option<Grab>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Grab(ClientId, ObjectId, u32);

impl Server {
    /// 将当前键盘 surface 投影到 text-input，更新输入法的 active 状态。
    /// 返回活动字段、候选锚点或输入法变化（需要重绘）。
    pub fn sync_text_focus(
        &mut self,
        focus: Option<Focus>,
        masks: (u32, u32),
        mut serial: impl FnMut() -> u32,
    ) -> bool {
        let previous = self.ime.focus;
        self.ime.focus = focus;
        let previous_focus = previous.map(|(target, _)| target);
        let new_focus = focus.map(|(target, _)| target);
        // 同一 wl_surface 重新映射时仍是不同的编辑会话，必须清除旧的 enable 和抓取。
        let remapped = new_focus == previous_focus && focus != previous;
        let focus = new_focus;
        for connection in &mut self.connections {
            if !focus.is_some_and(|(id, _)| id == connection.id)
                && !previous_focus.is_some_and(|(id, _)| id == connection.id)
            {
                continue;
            }
            let surface = focus
                .filter(|(id, _)| {
                    *id == connection.id
                        && !connection.closing
                        && matches!(
                            connection.client.privilege,
                            Privilege::Application | Privilege::Shell
                        )
                })
                .map(|(_, surface)| surface);
            let client = &mut connection.client;
            for (&id, entry) in &mut client.objects {
                let Data::TextInput(input) = &mut entry.data else {
                    continue;
                };
                if input.entered == surface && !(remapped && surface.is_some()) {
                    continue;
                }
                let object = ObjectId::new(id).expect("registered object");
                if let Some(old) = input.entered
                    && (text::Event::Leave { surface: old })
                        .encode(object, &mut client.writer)
                        .is_err()
                {
                    client.dead = true;
                }
                input.focus(surface);
                if let Some(surface) = surface
                    && (text::Event::Enter { surface })
                        .encode(object, &mut client.writer)
                        .is_err()
                {
                    client.dead = true;
                }
            }
        }
        let provider = self
            .connections
            .iter()
            .filter(|c| !c.closing && c.client.privilege == Privilege::InputMethod)
            .find_map(|connection| {
                connection.client.objects.iter().find_map(|(&id, entry)| {
                    matches!(&entry.data, Data::InputMethod(m) if !m.unavailable)
                        .then(|| (connection.id, ObjectId::new(id).expect("registered object")))
                })
            });
        let text = focus.and_then(|(id, surface)| {
            let connection = self.connections.iter().find(|c| !c.closing && c.id == id)?;
            connection.client.objects.iter().find_map(|(&obj, entry)| {
                let Data::TextInput(input) = &entry.data else {
                    return None;
                };
                if input.entered != Some(surface)
                    || !input.current.enabled
                    || input.current.sensitive()
                {
                    return None;
                }
                Some((
                    Field {
                        object: (id, ObjectId::new(obj).expect("registered object")),
                        surface,
                        epoch: input.epoch,
                    },
                    input.serial,
                    input.current.rect,
                ))
            })
        });
        let field = text
            .filter(|_| provider.is_some())
            .map(|(field, _, _)| field);
        let changed =
            field != self.ime.field || provider != self.ime.method || focus != previous_focus;
        let old_rect = self.ime.rectangle;
        if changed {
            if let Some(old) = self.ime.field {
                if let Some((id, object)) = self.ime.method {
                    self.with_client(id, |client| {
                        let result = (|| {
                            method::Event::Deactivate.encode(object, &mut client.writer)?;
                            emit_done(client, object)?;
                            Ok::<_, Fail>(())
                        })();
                        client.dead |= result.is_err();
                    });
                }
                if previous_focus == focus {
                    self.finish_virtual_keys(old.object.0, masks, &mut serial);
                    if field != Some(old) || provider != self.ime.method {
                        self.with_client(old.object.0, |client| {
                            let Some(Data::TextInput(input)) = client
                                .objects
                                .get(&old.object.1.get())
                                .map(|entry| &entry.data)
                            else {
                                return;
                            };
                            let serial = input.serial;
                            let result = (|| {
                                text::Event::PreeditString {
                                    text: None,
                                    cursor_begin: 0,
                                    cursor_end: 0,
                                }
                                .encode(old.object.1, &mut client.writer)?;
                                text::Event::Done { serial }
                                    .encode(old.object.1, &mut client.writer)
                            })();
                            client.dead |= result.is_err();
                        });
                    }
                } else {
                    self.ime.forwarded.clear();
                    self.ime.modifiers = None;
                }
            }
            self.ime.method = provider;
            self.ime.field = field;
            self.ime.grab = None;
            self.ime.revision = None;
            self.ime.rectangle = None;
            self.ime.popup_rectangle = None;
        }
        if let (Some((id, object)), Some((_, revision, rect)), Some(field)) =
            (provider, text, field)
        {
            if self.ime.revision != Some(revision) {
                let input = self
                    .connections
                    .iter()
                    .find(|c| c.id == field.object.0)
                    .and_then(|c| c.client.objects.get(&field.object.1.get()));
                if let Some(Data::TextInput(input)) = input.map(|e| &e.data) {
                    let (current, cause) = (input.current.clone(), input.cause);
                    let mut done = 0;
                    self.with_client(id, |client| {
                        let result = (|| {
                            if changed {
                                method::Event::Activate.encode(object, &mut client.writer)?;
                            }
                            if let Some((text, cursor, anchor)) = current.surrounding {
                                method::Event::SurroundingText {
                                    text,
                                    cursor,
                                    anchor,
                                }
                                .encode(object, &mut client.writer)?;
                            }
                            method::Event::ContentType {
                                hint: current.hint,
                                purpose: current.purpose,
                            }
                            .encode(object, &mut client.writer)?;
                            method::Event::TextChangeCause { cause }
                                .encode(object, &mut client.writer)?;
                            done = emit_done(client, object)?;
                            Ok::<_, Fail>(())
                        })();
                        client.dead |= result.is_err();
                    });
                    self.ime.revision = Some(revision);
                    if changed {
                        self.ime.activated = done;
                    }
                }
            }
            self.ime.rectangle = rect;
            let popup_rect = self
                .connections
                .iter()
                .find(|c| c.id == id && !c.closing)
                .and_then(|c| match &c.client.objects.get(&object.get())?.data {
                    Data::InputMethod(state) => state.popup,
                    _ => None,
                })
                .zip(rect)
                .map(|((role, _), rect)| (role, rect));
            if popup_rect != self.ime.popup_rectangle {
                self.ime.popup_rectangle = popup_rect;
                if let Some((
                    role,
                    Rect {
                        x,
                        y,
                        width,
                        height,
                    },
                )) = popup_rect
                {
                    self.with_client(id, |client| {
                        client.dead |= popup::Event::TextInputRectangle {
                            x,
                            y,
                            width,
                            height,
                        }
                        .encode(role, &mut client.writer)
                        .is_err();
                    });
                }
            }
            let next_grab = self
                .connections
                .iter()
                .find(|c| c.id == id && !c.closing)
                .and_then(|c| match &c.client.objects.get(&object.get())?.data {
                    Data::InputMethod(method) => method.grab,
                    _ => None,
                })
                .map(|grab| Grab(id, grab, self.ime.activated));
            if self.ime.grab != next_grab {
                if self.ime.grab.is_some() {
                    self.finish_virtual_keys(field.object.0, masks, &mut serial);
                }
                self.ime.grab = next_grab;
                if let Some(Grab(client, grab, _)) = next_grab {
                    self.with_client(client, |client| send_modifiers(client, grab, 0, masks));
                }
            }
        }
        changed || old_rect != self.ime.rectangle
    }

    /// 在合成器快捷键处理后，物理键只交给抓取者或原键盘焦点。
    pub fn input_method_grab(&self) -> Option<Grab> {
        self.ime.grab
    }

    pub fn input_method_key(
        &mut self,
        channel: Grab,
        serial: u32,
        time: u32,
        code: u32,
        pressed: bool,
        modifiers: Option<(u32, u32)>,
    ) {
        let Grab(id, object, _) = channel;
        self.with_client(id, |client| {
            if let Some(masks) = modifiers.filter(|_| pressed) {
                send_modifiers(client, object, serial, masks);
            }
            client.dead |= grab::Event::Key {
                serial,
                time,
                key: code,
                state: key_state(pressed),
            }
            .encode(object, &mut client.writer)
            .is_err();
            if let Some(masks) = modifiers.filter(|_| !pressed) {
                send_modifiers(client, object, serial, masks);
            }
        });
    }

    /// 处理输入法提交和转发键。所有目标由已同步的 seat 决定。
    pub fn dispatch_input_method(
        &mut self,
        previous_grab: Option<Grab>,
        mut serial: impl FnMut() -> u32,
    ) {
        for index in 0..self.connections.len() {
            let source = self.connections[index].id;
            let actions = std::mem::take(&mut self.connections[index].client.ime_actions);
            let (Some(field), Some((provider, method))) = (self.ime.field, self.ime.method) else {
                continue;
            };
            if source != provider || self.connections[index].closing {
                continue;
            }
            let done = match self.connections[index].client.method_mut(method) {
                Ok(method) => method.done,
                Err(_) => continue,
            };
            let forward_keys = previous_grab.is_some() && previous_grab == self.ime.grab;
            for action in actions {
                match action {
                    Action::Commit {
                        object,
                        serial,
                        edit,
                    } if object == method
                        && done.wrapping_sub(serial) <= done.wrapping_sub(self.ime.activated) =>
                    {
                        let app_serial = self.ime.revision.unwrap_or(0);
                        self.with_client(field.object.0, |client| {
                            let allow_delete = client.objects.get(&field.object.1.get()).is_some_and(|entry| {
                                matches!(&entry.data, Data::TextInput(input) if input.current.surrounding.is_some())
                            });
                            let edit = if allow_delete { edit } else { Edit { delete: (0, 0), ..edit } };
                            client.dead |= send_edit(client, field.object.1, app_serial, edit).is_err();
                        });
                    }
                    Action::Key { time, key, pressed } if forward_keys => {
                        let index = self.ime.forwarded.iter().position(|held| *held == key);
                        match (pressed, index) {
                            (true, None) => self.ime.forwarded.push(key),
                            (false, Some(index)) => {
                                self.ime.forwarded.swap_remove(index);
                            }
                            _ => continue,
                        }
                        self.with_client(field.object.0, |client| {
                            client.keyboard_key(serial(), time, key, pressed)
                        });
                    }
                    Action::Modifiers { depressed, locked } if forward_keys => {
                        self.ime.modifiers = Some((depressed, locked));
                        self.with_client(field.object.0, |client| {
                            client.keyboard_modifiers(serial(), depressed, locked)
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    fn finish_virtual_keys(
        &mut self,
        client: ClientId,
        masks: (u32, u32),
        serial: &mut impl FnMut() -> u32,
    ) {
        let forwarded = std::mem::take(&mut self.ime.forwarded);
        let modifiers = self.ime.modifiers.take();
        self.with_client(client, |client| {
            for key in forwarded {
                client.keyboard_key(serial(), 0, key, false);
            }
            if modifiers.is_some() {
                client.keyboard_modifiers(serial(), masks.0, masks.1);
            }
        });
    }

    pub fn is_input_method(&self, id: ClientId) -> bool {
        self.connections
            .iter()
            .any(|c| c.id == id && c.client.privilege == Privilege::InputMethod)
    }

    /// 候选窗是普通共享缓冲，输入法持有像素；合成器只投影相对光标的位置。
    pub fn input_popup(&self) -> Option<(Target, Target, Rect, (i32, i32))> {
        let field = self.ime.field?;
        let (provider, method) = self.ime.method?;
        let client = &self
            .connections
            .iter()
            .find(|c| c.id == provider && !c.closing)?
            .client;
        let Data::InputMethod(state) = &client.objects.get(&method.get())?.data else {
            return None;
        };
        let (_, surface) = state.popup?;
        let Data::Surface(view) = &client.objects.get(&surface.get())?.data else {
            return None;
        };
        let size = view
            .current
            .content
            .as_ref()?
            .output_size(self.output.scale);
        Some((
            (provider, surface),
            (field.object.0, field.surface),
            self.ime.rectangle.unwrap_or(Rect::new(8, 32, 1, 16)),
            size,
        ))
    }
}

fn key_state(pressed: bool) -> wl_keyboard::KeyState {
    if pressed {
        wl_keyboard::KeyState::Pressed
    } else {
        wl_keyboard::KeyState::Released
    }
}

fn send_modifiers(
    client: &mut Client,
    object: ObjectId,
    serial: u32,
    (depressed, locked): (u32, u32),
) {
    client.dead |= grab::Event::Modifiers {
        serial,
        mods_depressed: depressed,
        mods_latched: 0,
        mods_locked: locked,
        group: 0,
    }
    .encode(object, &mut client.writer)
    .is_err();
}

fn emit_done(client: &mut Client, object: ObjectId) -> Result<u32, Fail> {
    let state = client.method_mut(object)?;
    state.done = state.done.wrapping_add(1);
    let count = state.done;
    method::Event::Done.encode(object, &mut client.writer)?;
    Ok(count)
}

fn send_edit(client: &mut Client, object: ObjectId, serial: u32, edit: Edit) -> Result<(), Fail> {
    if edit.delete != (0, 0) {
        text::Event::DeleteSurroundingText {
            before_length: edit.delete.0,
            after_length: edit.delete.1,
        }
        .encode(object, &mut client.writer)?;
    }
    if let Some(value) = edit.commit {
        text::Event::CommitString { text: Some(value) }.encode(object, &mut client.writer)?;
    }
    let (value, cursor_begin, cursor_end) = edit
        .preedit
        .map_or((None, 0, 0), |(s, a, b)| (Some(s), a, b));
    text::Event::PreeditString {
        text: value,
        cursor_begin,
        cursor_end,
    }
    .encode(object, &mut client.writer)?;
    text::Event::Done { serial }.encode(object, &mut client.writer)?;
    Ok(())
}
