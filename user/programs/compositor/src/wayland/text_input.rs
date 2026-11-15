//! 每连接的 Wayland 文本输入状态与输入法的待提交请求。

use super::client::{Client, Data, Fail};
use canvas::Rect;
use std::{fs::File, os::unix::fs::FileExt};
use wayland_wire::{
    protocol::{
        Interface, wl_keyboard, zwp_input_method_keyboard_grab_v2 as grab,
        zwp_input_method_manager_v2 as methods, zwp_input_method_v2 as method,
        zwp_text_input_manager_v3 as manager, zwp_text_input_v3 as text,
        zwp_virtual_keyboard_manager_v1 as keyboards, zwp_virtual_keyboard_v1 as keyboard,
    },
    wire::ObjectId,
};

#[derive(Clone)]
pub(super) struct InputState {
    pub enabled: bool,
    pub rect: Option<Rect>,
    pub surrounding: Option<(String, u32, u32)>,
    pub hint: u32,
    pub purpose: text::ContentPurpose,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            enabled: false,
            rect: None,
            surrounding: None,
            hint: 0,
            purpose: text::ContentPurpose::Normal,
        }
    }
}

impl InputState {
    pub fn sensitive(&self) -> bool {
        use text::ContentPurpose::{Password, Pin};
        matches!(self.purpose, Password | Pin)
            || self.hint & (text::content_hint::HIDDENTEXT | text::content_hint::SENSITIVEDATA) != 0
    }
}

pub(super) struct TextInput {
    pub entered: Option<ObjectId>,
    pub current: InputState,
    pending: InputState,
    reset: bool,
    pub epoch: u32,
    pub serial: u32,
    pub cause: text::ChangeCause,
    pending_cause: text::ChangeCause,
}

impl Default for TextInput {
    fn default() -> Self {
        Self {
            entered: None,
            current: InputState::default(),
            pending: InputState::default(),
            reset: false,
            epoch: 0,
            serial: 0,
            cause: text::ChangeCause::InputMethod,
            pending_cause: text::ChangeCause::InputMethod,
        }
    }
}

impl TextInput {
    pub fn focus(&mut self, surface: Option<ObjectId>) {
        self.entered = surface;
        self.current = InputState::default();
        self.pending = InputState::default();
        self.cause = text::ChangeCause::InputMethod;
        self.pending_cause = self.cause;
        self.reset = false;
    }
}

#[derive(Default)]
pub(super) struct Edit {
    pub preedit: Option<(String, i32, i32)>,
    pub commit: Option<String>,
    pub delete: (u32, u32),
}

#[derive(Default)]
pub(super) struct Method {
    pending: Edit,
    pub done: u32,
    pub grab: Option<ObjectId>,
    pub popup: Option<(ObjectId, ObjectId)>,
    pub unavailable: bool,
}

pub(super) enum Action {
    Commit {
        object: ObjectId,
        serial: u32,
        edit: Edit,
    },
    Key {
        time: u32,
        key: u32,
        pressed: bool,
    },
    Modifiers {
        depressed: u32,
        locked: u32,
    },
}

impl Client {
    fn check_seat(&self, seat: ObjectId) -> Result<(), Fail> {
        if self
            .objects
            .get(&seat.get())
            .is_some_and(|entry| entry.interface == Interface::WlSeat)
        {
            Ok(())
        } else {
            Err(Fail::invalid_object("expected wl_seat"))
        }
    }

    pub(super) fn text_manager(&mut self, request: manager::Request) -> Result<(), Fail> {
        if let manager::Request::GetTextInput { id, seat } = request {
            self.check_seat(seat)?;
            self.create(
                id,
                Interface::ZwpTextInputV3,
                1,
                Data::TextInput(Box::default()),
            )?;
        }
        Ok(())
    }

    pub(super) fn text_input(
        &mut self,
        object: ObjectId,
        request: text::Request,
    ) -> Result<(), Fail> {
        let Some(Data::TextInput(input)) = self.objects.get_mut(&object.get()).map(|e| &mut e.data)
        else {
            return Err(Fail::implementation(object));
        };
        use text::Request as R;
        match request {
            R::Destroy => {}
            R::Enable | R::Disable => {
                input.pending = InputState {
                    enabled: matches!(request, R::Enable),
                    ..InputState::default()
                };
                input.reset = true;
            }
            R::SetSurroundingText {
                text,
                cursor,
                anchor,
            } => {
                let valid = |index: i32| {
                    usize::try_from(index).is_ok_and(|index| text.is_char_boundary(index))
                };
                if text.len() > 4000 || !valid(cursor) || !valid(anchor) {
                    return Err(Fail::invalid_method(object, "invalid surrounding text"));
                }
                input.pending.surrounding = Some((text, cursor as u32, anchor as u32));
            }
            R::SetTextChangeCause { cause } => input.pending_cause = cause,
            R::SetContentType { hint, purpose } => {
                input.pending.hint = hint;
                input.pending.purpose = purpose;
            }
            R::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => {
                input.pending.rect =
                    (width >= 0 && height >= 0).then_some(Rect::new(x, y, width, height));
            }
            R::Commit => {
                input.serial = input.serial.wrapping_add(1);
                if input.entered.is_none() {
                    input.focus(None);
                    return Ok(());
                }
                if input.pending.sensitive() {
                    input.pending.surrounding = None;
                }
                input.current = input.pending.clone();
                input.cause =
                    std::mem::replace(&mut input.pending_cause, text::ChangeCause::InputMethod);
                if std::mem::take(&mut input.reset) {
                    input.epoch = input.epoch.wrapping_add(1);
                    if input.current.enabled {
                        for (id, entry) in &mut self.objects {
                            if *id != object.get()
                                && let Data::TextInput(other) = &mut entry.data
                            {
                                other.current.enabled = false;
                                other.pending.enabled = false;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn method_manager(&mut self, request: methods::Request) -> Result<(), Fail> {
        let methods::Request::GetInputMethod { input_method, seat } = request else {
            return Ok(());
        };
        self.check_seat(seat)?;
        // 仅唯一获委派的连接能绑定 manager，同连接上的第二个方法对象立即成为 inert。
        let unavailable = self
            .objects
            .values()
            .any(|entry| matches!(&entry.data, Data::InputMethod(m) if !m.unavailable));
        self.create(
            input_method,
            Interface::ZwpInputMethodV2,
            1,
            Data::InputMethod(Box::new(Method {
                unavailable,
                ..Method::default()
            })),
        )?;
        if unavailable {
            method::Event::Unavailable.encode(input_method, &mut self.writer)?;
        }
        Ok(())
    }

    pub(super) fn method_mut(&mut self, object: ObjectId) -> Result<&mut Method, Fail> {
        match self
            .objects
            .get_mut(&object.get())
            .map(|entry| &mut entry.data)
        {
            Some(Data::InputMethod(state)) => Ok(state),
            _ => Err(Fail::implementation(object)),
        }
    }

    pub(super) fn method(
        &mut self,
        object: ObjectId,
        request: method::Request,
    ) -> Result<(), Fail> {
        use method::Request as R;
        let state = self.method_mut(object)?;
        if state.unavailable {
            return Ok(());
        }
        match request {
            R::Destroy => {
                let grab = state.grab.take();
                let popup = state.popup.take();
                if let Some(grab) = grab {
                    self.release_id(grab);
                }
                if let Some((popup, _)) = popup {
                    self.release_id(popup);
                }
            }
            R::SetPreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                let valid = |index: i32| {
                    index == -1
                        || usize::try_from(index).is_ok_and(|index| text.is_char_boundary(index))
                };
                if text.len() > 4000 || !valid(cursor_begin) || !valid(cursor_end) {
                    return Err(Fail::invalid_method(object, "invalid preedit"));
                }
                state.pending.preedit = Some((text, cursor_begin, cursor_end));
            }
            R::CommitString { text } => {
                if text.len() > 4000 {
                    return Err(Fail::invalid_method(object, "commit exceeds 4000 bytes"));
                }
                state.pending.commit = Some(text);
            }
            R::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.pending.delete = (before_length, after_length);
            }
            R::Commit { serial } => {
                let edit = std::mem::take(&mut state.pending);
                self.ime_actions.push(Action::Commit {
                    object,
                    serial,
                    edit,
                });
            }
            R::GrabKeyboard { keyboard } => {
                if state.grab.is_some() {
                    return Err(Fail::invalid_method(
                        object,
                        "input method already has a keyboard grab",
                    ));
                }
                self.create(
                    keyboard,
                    Interface::ZwpInputMethodKeyboardGrabV2,
                    1,
                    Data::MethodGrab(object),
                )?;
                self.method_mut(object)?.grab = Some(keyboard);
                let (fd, size) = super::seat::keymap(keyboard)?;
                let event = grab::Event::Keymap {
                    format: wl_keyboard::KeymapFormat::XkbV1,
                    fd,
                    size,
                };
                let offset = self.writer.bytes().len();
                event.encode(keyboard, &mut self.writer)?;
                self.writer.consume(0, 1);
                if let grab::Event::Keymap { fd, .. } = event {
                    self.outgoing_fds.push_back((offset, fd));
                }
                grab::Event::RepeatInfo {
                    rate: super::seat::REPEAT_RATE,
                    delay: super::seat::REPEAT_DELAY_MS,
                }
                .encode(keyboard, &mut self.writer)?;
            }
            R::GetInputPopupSurface { id, surface } => {
                if state.popup.is_some() {
                    return Err(Fail::invalid_method(
                        object,
                        "input method already has a popup",
                    ));
                }
                if self.surface_mut(surface)?.has_role() {
                    return Err(Fail::new(
                        object,
                        method::Error::Role as u32,
                        "wl_surface already has a role",
                    ));
                }
                self.create(
                    id,
                    Interface::ZwpInputPopupSurfaceV2,
                    1,
                    Data::MethodPopup(object),
                )?;
                self.surface_mut(surface)?.input_popup = true;
                self.method_mut(object)?.popup = Some((id, surface));
            }
        }
        Ok(())
    }

    pub(super) fn remove_method_child(&mut self, object: ObjectId) {
        match self.objects[&object.get()].data {
            Data::MethodGrab(parent) => {
                if let Ok(state) = self.method_mut(parent) {
                    state.grab = None;
                }
            }
            Data::MethodPopup(parent) => {
                if let Ok(state) = self.method_mut(parent) {
                    state.popup = None;
                }
                self.commits += 1;
            }
            _ => {}
        }
    }

    pub(super) fn virtual_keyboard_manager(
        &mut self,
        request: keyboards::Request,
    ) -> Result<(), Fail> {
        let keyboards::Request::CreateVirtualKeyboard { seat, id } = request;
        self.check_seat(seat)?;
        if self
            .objects
            .values()
            .any(|entry| matches!(entry.data, Data::VirtualKeyboard(_)))
        {
            return Err(Fail::invalid_method(
                id,
                "input method already has a virtual keyboard",
            ));
        }
        self.create(
            id,
            Interface::ZwpVirtualKeyboardV1,
            1,
            Data::VirtualKeyboard(false),
        )
    }

    pub(super) fn virtual_keyboard(
        &mut self,
        object: ObjectId,
        request: keyboard::Request,
    ) -> Result<(), Fail> {
        let Some(Data::VirtualKeyboard(mapped)) =
            self.objects.get_mut(&object.get()).map(|e| &mut e.data)
        else {
            return Err(Fail::implementation(object));
        };
        use keyboard::Request as R;
        match request {
            R::Destroy => {}
            R::Keymap { format, fd, size } => {
                let mut expected = ::keyboard::xkb::keymap_text().into_bytes();
                expected.push(0);
                let file = File::from(fd);
                let mut bytes = vec![0; expected.len()];
                if format != wl_keyboard::KeymapFormat::XkbV1 as u32
                    || size as usize != expected.len()
                    || !file
                        .metadata()
                        .is_ok_and(|meta| meta.is_file() && meta.len() >= u64::from(size))
                    || file.read_exact_at(&mut bytes, 0).is_err()
                    || bytes != expected
                {
                    return Err(Fail::invalid_method(
                        object,
                        "virtual keyboard must use the seat keymap",
                    ));
                }
                *mapped = true;
            }
            _ if !*mapped => {
                return Err(Fail::new(
                    object,
                    keyboard::Error::NoKeymap as u32,
                    "virtual keymap is missing",
                ));
            }
            R::Key { time, key, state } => {
                if key <= 767 && state <= 1 {
                    self.ime_actions.push(Action::Key {
                        time,
                        key,
                        pressed: state == 1,
                    });
                }
            }
            R::Modifiers {
                mods_depressed,
                mods_locked,
                ..
            } => {
                self.ime_actions.push(Action::Modifiers {
                    depressed: mods_depressed,
                    locked: mods_locked,
                });
            }
        }
        Ok(())
    }
}
