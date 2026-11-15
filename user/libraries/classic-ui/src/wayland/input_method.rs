//! 授权输入法进程的 Wayland 客户端。词库、选词和模式不属于本模块。

use super::{Configuration, Display, EventKind, Role, Window, WindowId};
use std::{io, os::fd::AsFd};
use wayland_wire::{
    protocol::{
        AnyEvent, Interface, wl_compositor, wl_keyboard, zwp_input_method_keyboard_grab_v2 as grab,
        zwp_input_method_manager_v2 as methods, zwp_input_method_v2 as method,
        zwp_virtual_keyboard_manager_v1 as keyboards, zwp_virtual_keyboard_v1 as keyboard,
    },
    wire::{ObjectId, Writer},
};

/// 输入法客户端看到的有效焦点和原始按键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMethodEvent {
    /// 活动编辑目标获得或失去输入法路由。
    Active(bool),
    /// 抓取到的按键；time 和 key 可原样交还虚拟键盘。
    Key { time: u32, key: u32, pressed: bool },
    /// XKB 修饰位。
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

pub(super) struct InputMethod {
    pub method: ObjectId,
    keyboard: ObjectId,
    pub notify: WindowId,
    serial: u32,
    pending_active: Option<bool>,
}

impl Display {
    /// 为已委派的输入法连接绑定活动上下文、键盘抓取和虚拟键盘。
    ///
    /// `notify` 接收输入法事件。普通应用连接返回 PermissionDenied；编码和 I/O 错误原样返回。
    pub fn start_input_method(&mut self, notify: WindowId) -> io::Result<()> {
        let Some((methods, keyboards)) = self.input_method_managers else {
            return Err(io::ErrorKind::PermissionDenied.into());
        };
        if !self.windows.contains_key(&notify) || self.input_method.is_some() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let transport = &mut self.transport;
        let method = transport.allocate(Interface::ZwpInputMethodV2)?;
        let grab = transport.allocate(Interface::ZwpInputMethodKeyboardGrabV2)?;
        let keyboard = transport.allocate(Interface::ZwpVirtualKeyboardV1)?;
        methods::Request::GetInputMethod {
            seat: self.seat,
            input_method: method,
        }
        .encode(methods, &mut transport.writer)
        .map_err(io::Error::other)?;
        keyboards::Request::CreateVirtualKeyboard {
            seat: self.seat,
            id: keyboard,
        }
        .encode(keyboards, &mut transport.writer)
        .map_err(io::Error::other)?;
        method::Request::GrabKeyboard { keyboard: grab }
            .encode(method, &mut transport.writer)
            .map_err(io::Error::other)?;
        self.input_method = Some(InputMethod {
            method,
            keyboard,
            notify,
            serial: 0,
            pending_active: None,
        });
        transport.flush()
    }

    /// 交还原输入法事件中的未消费按键或修饰位；错误表示未绑定协议或发送失败。
    pub fn forward_input_method_event(&mut self, event: &InputMethodEvent) -> io::Result<()> {
        let state = self
            .input_method
            .as_ref()
            .ok_or(io::ErrorKind::NotConnected)?;
        let request = match *event {
            InputMethodEvent::Key { time, key, pressed } => keyboard::Request::Key {
                time,
                key,
                state: u32::from(pressed),
            },
            InputMethodEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            } => keyboard::Request::Modifiers {
                mods_depressed: depressed,
                mods_latched: latched,
                mods_locked: locked,
                group,
            },
            InputMethodEvent::Active(_) => return Ok(()),
        };
        request
            .encode(state.keyboard, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 按最后一次输入法 done 提交预编辑和上屏文本；空预编辑清除旧预编辑。
    /// 未绑定、文字超长或传输失败时返回错误。
    pub fn commit_input_method(
        &mut self,
        preedit: &str,
        cursor: usize,
        commit: Option<&str>,
    ) -> io::Result<()> {
        if preedit.len() > 4000
            || !preedit.is_char_boundary(cursor)
            || commit.is_some_and(|text| text.len() > 4000)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let state = self
            .input_method
            .as_ref()
            .ok_or(io::ErrorKind::NotConnected)?;
        let writer = &mut self.transport.writer;
        if let Some(text) = commit {
            method::Request::CommitString { text: text.into() }
                .encode(state.method, writer)
                .map_err(io::Error::other)?;
        }
        method::Request::SetPreeditString {
            text: preedit.into(),
            cursor_begin: cursor as i32,
            cursor_end: cursor as i32,
        }
        .encode(state.method, writer)
        .map_err(io::Error::other)?;
        method::Request::Commit {
            serial: state.serial,
        }
        .encode(state.method, writer)
        .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 新建输入法候选窗；合成器从当前编辑目标取得锚点。用常规 paint_frame 提交像素。
    /// 非法尺寸、缺少权限或协议传输错误原样返回。
    pub fn create_input_popup(&mut self, size: (i32, i32)) -> io::Result<WindowId> {
        let method = self
            .input_method
            .as_ref()
            .ok_or(io::ErrorKind::NotConnected)?
            .method;
        let configuration = Configuration {
            width: size.0,
            height: size.1,
            scale: self.scale,
            active: false,
            maximized: false,
            fullscreen: false,
            resizing: false,
            suspended: false,
        };
        configuration.pixel_size()?;
        let transport = &mut self.transport;
        let surface = transport.allocate(Interface::WlSurface)?;
        let object = transport.allocate(Interface::ZwpInputPopupSurfaceV2)?;
        wl_compositor::Request::CreateSurface { id: surface }
            .encode(self.compositor, &mut transport.writer)
            .map_err(io::Error::other)?;
        method::Request::GetInputPopupSurface {
            id: object,
            surface,
        }
        .encode(method, &mut transport.writer)
        .map_err(io::Error::other)?;
        let id = WindowId(surface);
        self.windows.insert(
            id,
            Window {
                role: Role::InputPopup { object },
                configuration,
                configured: true,
                mapped: false,
                frame: None,
                history: Default::default(),
                viewport: None,
            },
        );
        transport.flush()?;
        Ok(id)
    }

    pub(super) fn handle_input_method(&mut self, event: AnyEvent) -> io::Result<()> {
        let Some(state) = &mut self.input_method else {
            return Ok(());
        };
        let notify = state.notify;
        let value = match event {
            AnyEvent::ZwpInputMethodV2(method::Event::Unavailable) => {
                return Err(io::Error::other("seat already has an input method"));
            }
            AnyEvent::ZwpInputMethodV2(method::Event::Activate) => {
                state.pending_active = Some(true);
                None
            }
            AnyEvent::ZwpInputMethodV2(method::Event::Deactivate) => {
                state.pending_active = Some(false);
                None
            }
            AnyEvent::ZwpInputMethodV2(method::Event::Done) => {
                state.serial = state.serial.wrapping_add(1);
                state.pending_active.take().map(InputMethodEvent::Active)
            }
            AnyEvent::ZwpInputMethodKeyboardGrabV2(grab::Event::Keymap { format, fd, size }) => {
                let request = keyboard::Request::Keymap {
                    format: format as u32,
                    fd,
                    size,
                };
                let mut writer = Writer::new();
                request
                    .encode(state.keyboard, &mut writer)
                    .map_err(io::Error::other)?;
                if let keyboard::Request::Keymap { fd, .. } = &request {
                    self.transport.send_fd(writer.bytes(), fd.as_fd())?;
                }
                None
            }
            AnyEvent::ZwpInputMethodKeyboardGrabV2(grab::Event::Key {
                time, key, state, ..
            }) => Some(InputMethodEvent::Key {
                time,
                key,
                pressed: state == wl_keyboard::KeyState::Pressed,
            }),
            AnyEvent::ZwpInputMethodKeyboardGrabV2(grab::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            }) => Some(InputMethodEvent::Modifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            }),
            _ => None,
        };
        if let Some(value) = value {
            self.emit(Some(notify), EventKind::InputMethod(value));
        }
        Ok(())
    }
}
