//! `wl_seat`：指针与键盘。客户端只在这里创建输入对象；事件由合成器的焦点策略主动发送。

use std::{fs::File, os::fd::AsFd};

use linux_uapi::shared_memory::MemFd;
use std::os::unix::fs::FileExt;
use wayland_wire::{
    protocol::{Interface, wl_keyboard, wl_pointer, wl_seat},
    wire::{Fixed, ObjectId},
};

use wayland_wire::protocol::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1};

use super::client::{Client, Data, Fail};

/// 客户端对指针光标的请求（`wl_pointer.set_cursor`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRequest {
    /// 必须是该客户端最近一次收到的 `wl_pointer.enter` 的序号，否则请求过期。
    pub serial: u32,
    /// 光标图像所在的 `wl_surface`；`None` 表示隐藏光标。
    pub surface: Option<ObjectId>,
    /// 热点，相对 surface 左上角，逻辑像素。
    pub hotspot: (i32, i32),
    /// `wp_cursor_shape_device_v1.set_shape` 指定的合成器自带光标名；此时没有 `surface`。
    pub named: Option<&'static str>,
}

/// 重复按键：每秒字符数与首次重复前的延迟毫秒。由客户端自己实现重复，合成器只宣告参数。
const REPEAT_RATE: i32 = 30;
const REPEAT_DELAY_MS: i32 = 500;

impl Client {
    pub(super) fn announce_seat(&mut self, seat: ObjectId, version: u32) -> Result<(), Fail> {
        wl_seat::Event::Capabilities {
            capabilities: wl_seat::capability::POINTER | wl_seat::capability::KEYBOARD,
        }
        .encode(seat, &mut self.writer)?;
        if version >= 2 {
            wl_seat::Event::Name {
                name: "seat0".into(),
            }
            .encode(seat, &mut self.writer)?;
        }
        Ok(())
    }

    pub(super) fn seat(
        &mut self,
        object: ObjectId,
        version: u32,
        request: wl_seat::Request,
    ) -> Result<(), Fail> {
        match request {
            wl_seat::Request::GetPointer { id } => {
                self.create(id, Interface::WlPointer, version, Data::None)
            }
            wl_seat::Request::GetKeyboard { id } => {
                self.create(id, Interface::WlKeyboard, version, Data::None)?;
                self.send_keymap(id, version)
            }
            wl_seat::Request::GetTouch { .. } => Err(Fail::new(
                object,
                wl_seat::Error::MissingCapability as u32,
                "this seat has no touch capability",
            )),
            wl_seat::Request::Release => Ok(()),
        }
    }

    /// 为新的 `wl_keyboard` 发送 keymap（独立 memfd，客户端可以随意映射）与重复参数。
    fn send_keymap(&mut self, keyboard: ObjectId, version: u32) -> Result<(), Fail> {
        let text = keyboard::xkb::keymap_text();
        // keymap 文本以 NUL 结尾，size 包含它（libxkbcommon 按 C 字符串读取）。
        let mut bytes = text.into_bytes();
        bytes.push(0);
        let memory = MemFd::create("liteos-keymap", bytes.len())
            .map_err(|_| Fail::implementation(keyboard))?;
        let fd = memory
            .as_fd()
            .try_clone_to_owned()
            .map_err(|_| Fail::implementation(keyboard))?;
        File::from(fd.try_clone().map_err(|_| Fail::implementation(keyboard))?)
            .write_all_at(&bytes, 0)
            .map_err(|_| Fail::implementation(keyboard))?;
        let offset = self.writer.bytes().len();
        let event = wl_keyboard::Event::Keymap {
            format: wl_keyboard::KeymapFormat::XkbV1,
            fd,
            size: bytes.len() as u32,
        };
        event.encode(keyboard, &mut self.writer)?;
        // 描述符不走 Writer 的借用列表，而由本连接持有直到发送完成。
        self.writer.consume(0, 1);
        let wl_keyboard::Event::Keymap { fd, .. } = event else {
            unreachable!("constructed as a keymap event above");
        };
        self.outgoing_fds.push_back((offset, fd));
        if version >= 4 {
            wl_keyboard::Event::RepeatInfo {
                rate: REPEAT_RATE,
                delay: REPEAT_DELAY_MS,
            }
            .encode(keyboard, &mut self.writer)?;
        }
        Ok(())
    }

    pub(super) fn pointer(
        &mut self,
        object: ObjectId,
        request: wl_pointer::Request,
    ) -> Result<(), Fail> {
        match request {
            wl_pointer::Request::SetCursor {
                serial,
                surface,
                hotspot_x,
                hotspot_y,
            } => {
                if let Some(surface) = surface {
                    let state = self.surface_mut(surface)?;
                    if state.xdg.is_some() || state.layer.is_some() {
                        return Err(Fail::new(
                            object,
                            wl_pointer::Error::Role as u32,
                            "cursor surface already has another role",
                        ));
                    }
                    state.cursor = true;
                }
                self.cursor_requests.push(CursorRequest {
                    serial,
                    surface,
                    hotspot: (hotspot_x, hotspot_y),
                    named: None,
                });
                Ok(())
            }
            wl_pointer::Request::Release => Ok(()),
        }
    }

    /// `wp_cursor_shape_manager_v1`：为 `wl_pointer` 创建形状设备；平板工具没有对应的全局对象，请求它是协议错误。
    pub(super) fn cursor_shape_manager(
        &mut self,
        object: ObjectId,
        version: u32,
        request: wp_cursor_shape_manager_v1::Request,
    ) -> Result<(), Fail> {
        use wp_cursor_shape_manager_v1::Request;
        match request {
            Request::Destroy => Ok(()),
            Request::GetPointer {
                cursor_shape_device,
                ..
            } => self.create(
                cursor_shape_device,
                Interface::WpCursorShapeDeviceV1,
                version,
                Data::None,
            ),
            Request::GetTabletToolV2 { .. } => Err(Fail::invalid_method(
                object,
                "this compositor has no tablet support".to_owned(),
            )),
        }
    }

    pub(super) fn cursor_shape_device(
        &mut self,
        request: wp_cursor_shape_device_v1::Request,
    ) -> Result<(), Fail> {
        if let wp_cursor_shape_device_v1::Request::SetShape { serial, shape } = request {
            self.cursor_requests.push(CursorRequest {
                serial,
                surface: None,
                hotspot: (0, 0),
                named: Some(crate::cursor::shape_name(shape)),
            });
        }
        Ok(())
    }

    /// 取走自上次以来客户端的光标请求。
    pub fn take_cursor_requests(&mut self) -> Vec<CursorRequest> {
        std::mem::take(&mut self.cursor_requests)
    }

    /// 光标 surface 当前提交的图像（预乘 ARGB）及其 buffer scale。
    pub fn cursor_image(&self, surface: ObjectId) -> Option<(&super::Content, i32)> {
        match self.objects.get(&surface.get()) {
            Some(super::client::Entry {
                data: Data::Surface(state),
                ..
            }) => Some((state.content.as_ref()?, state.scale)),
            _ => None,
        }
    }

    fn objects_of(&self, interface: Interface) -> Vec<(ObjectId, u32)> {
        self.objects
            .iter()
            .filter(|(_, entry)| entry.interface == interface)
            .filter_map(|(id, entry)| Some((ObjectId::new(*id)?, entry.version)))
            .collect()
    }

    /// 指针进入 `surface`，坐标是 surface 局部的逻辑像素。
    pub fn pointer_enter(&mut self, serial: u32, surface: ObjectId, x: Fixed, y: Fixed) {
        for (pointer, version) in self.objects_of(Interface::WlPointer) {
            let _ = wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x: x,
                surface_y: y,
            }
            .encode(pointer, &mut self.writer);
            self.pointer_frame(pointer, version);
        }
    }

    pub fn pointer_leave(&mut self, serial: u32, surface: ObjectId) {
        for (pointer, version) in self.objects_of(Interface::WlPointer) {
            let _ = wl_pointer::Event::Leave { serial, surface }.encode(pointer, &mut self.writer);
            self.pointer_frame(pointer, version);
        }
    }

    pub fn pointer_motion(&mut self, time: u32, x: Fixed, y: Fixed) {
        for (pointer, version) in self.objects_of(Interface::WlPointer) {
            let _ = wl_pointer::Event::Motion {
                time,
                surface_x: x,
                surface_y: y,
            }
            .encode(pointer, &mut self.writer);
            self.pointer_frame(pointer, version);
        }
    }

    /// `button` 是 Linux `BTN_*` 码。
    pub fn pointer_button(&mut self, serial: u32, time: u32, button: u32, pressed: bool) {
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        for (pointer, version) in self.objects_of(Interface::WlPointer) {
            let _ = wl_pointer::Event::Button {
                serial,
                time,
                button,
                state,
            }
            .encode(pointer, &mut self.writer);
            self.pointer_frame(pointer, version);
        }
    }

    /// 滚轮：`vertical`、`horizontal` 是档位数（正值向下、向右）。
    ///
    /// 版本 5 起先给来源（滚轮）与离散档位，再给连续值（每档 10 逻辑像素，与 libinput 一致），
    /// 全部以一个 `frame` 结束；更早的版本只有连续值。
    pub fn pointer_axis(&mut self, time: u32, vertical: i32, horizontal: i32) {
        for (pointer, version) in self.objects_of(Interface::WlPointer) {
            if version >= 5 {
                let _ = wl_pointer::Event::AxisSource {
                    axis_source: wl_pointer::AxisSource::Wheel,
                }
                .encode(pointer, &mut self.writer);
            }
            for (axis, notches) in [
                (wl_pointer::Axis::VerticalScroll, vertical),
                (wl_pointer::Axis::HorizontalScroll, horizontal),
            ] {
                if notches == 0 {
                    continue;
                }
                if version >= 5 {
                    let _ = wl_pointer::Event::AxisDiscrete {
                        axis,
                        discrete: notches,
                    }
                    .encode(pointer, &mut self.writer);
                }
                let _ = wl_pointer::Event::Axis {
                    time,
                    axis,
                    value: Fixed::from_int(notches * 10),
                }
                .encode(pointer, &mut self.writer);
            }
            self.pointer_frame(pointer, version);
        }
    }

    /// 版本 5 起，一组逻辑上同时发生的指针事件以 `frame` 结束。
    fn pointer_frame(&mut self, pointer: ObjectId, version: u32) {
        if version >= 5 {
            let _ = wl_pointer::Event::Frame.encode(pointer, &mut self.writer);
        }
    }

    /// 键盘获得焦点；`pressed` 是此刻按住的键（evdev 码）。
    pub fn keyboard_enter(&mut self, serial: u32, surface: ObjectId, pressed: &[u32]) {
        let keys: Vec<u8> = pressed.iter().flat_map(|key| key.to_ne_bytes()).collect();
        for (keyboard, _) in self.objects_of(Interface::WlKeyboard) {
            let _ = wl_keyboard::Event::Enter {
                serial,
                surface,
                keys: keys.clone(),
            }
            .encode(keyboard, &mut self.writer);
        }
    }

    pub fn keyboard_leave(&mut self, serial: u32, surface: ObjectId) {
        for (keyboard, _) in self.objects_of(Interface::WlKeyboard) {
            let _ =
                wl_keyboard::Event::Leave { serial, surface }.encode(keyboard, &mut self.writer);
        }
    }

    /// `key` 是 evdev 键码（不加 XKB 偏移）。
    pub fn keyboard_key(&mut self, serial: u32, time: u32, key: u32, pressed: bool) {
        let state = if pressed {
            wl_keyboard::KeyState::Pressed
        } else {
            wl_keyboard::KeyState::Released
        };
        for (keyboard, _) in self.objects_of(Interface::WlKeyboard) {
            let _ = wl_keyboard::Event::Key {
                serial,
                time,
                key,
                state,
            }
            .encode(keyboard, &mut self.writer);
        }
    }

    pub fn keyboard_modifiers(&mut self, serial: u32, depressed: u32, locked: u32) {
        for (keyboard, _) in self.objects_of(Interface::WlKeyboard) {
            let _ = wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed: depressed,
                mods_latched: 0,
                mods_locked: locked,
                group: 0,
            }
            .encode(keyboard, &mut self.writer);
        }
    }
}
