use super::{Display, Event, EventKind, Key, Repeat, Role, WindowId};
use keyboard::{Modifiers, character, xkb};
use std::{
    fs::File,
    io::{self, Read},
    time::{Duration, Instant},
};
use wayland_wire::{
    protocol::{
        AnyEvent, Interface, wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface,
        wp_cursor_shape_manager_v1, xdg_surface, xdg_toplevel, xdg_wm_base,
    },
    wire::ObjectId,
};

impl Display {
    pub(super) fn key(&self, code: u32, pressed: bool, repeat: bool) -> Key {
        let modifiers = self.modifiers;
        let text = if pressed && !modifiers.control && !modifiers.alt && !modifiers.super_key {
            u16::try_from(code)
                .ok()
                .and_then(|code| character(code, modifiers))
        } else {
            None
        };
        Key {
            code,
            pressed,
            repeat,
            modifiers,
            text,
        }
    }

    fn emit(&mut self, window: Option<WindowId>, kind: EventKind) {
        if let Some(window) = window.filter(|window| self.windows.contains_key(window)) {
            self.events.push_back(Event { window, kind });
        }
    }

    pub(super) fn dispatch(&mut self, object: ObjectId, event: AnyEvent) -> io::Result<()> {
        if self.foreign_event(object, &event)? || self.clipboard_event(object, &event)? {
            return Ok(());
        }
        match event {
            AnyEvent::WlSeat(wl_seat::Event::Capabilities { capabilities }) => {
                if capabilities & wl_seat::capability::POINTER != 0 && self.pointer_device.is_none()
                {
                    let pointer = self.transport.allocate(Interface::WlPointer)?;
                    wl_seat::Request::GetPointer { id: pointer }
                        .encode(self.seat, &mut self.transport.writer)
                        .map_err(io::Error::other)?;
                    self.pointer_device = Some(pointer);
                    if let Some(manager) = self.cursor_shape_manager {
                        let device = self.transport.allocate(Interface::WpCursorShapeDeviceV1)?;
                        wp_cursor_shape_manager_v1::Request::GetPointer {
                            cursor_shape_device: device,
                            pointer,
                        }
                        .encode(manager, &mut self.transport.writer)
                        .map_err(io::Error::other)?;
                        self.cursor_shape_device = Some(device);
                    }
                } else if capabilities & wl_seat::capability::POINTER == 0 {
                    let old = self.pointer.take();
                    self.emit(old, EventKind::PointerLeave);
                }
                if capabilities & wl_seat::capability::KEYBOARD != 0
                    && self.keyboard_device.is_none()
                {
                    let keyboard = self.transport.allocate(Interface::WlKeyboard)?;
                    wl_seat::Request::GetKeyboard { id: keyboard }
                        .encode(self.seat, &mut self.transport.writer)
                        .map_err(io::Error::other)?;
                    self.keyboard_device = Some(keyboard);
                } else if capabilities & wl_seat::capability::KEYBOARD == 0 {
                    let old = self.keyboard.take();
                    self.emit(old, EventKind::Focus(false));
                    self.repeat = None;
                    self.modifiers = Modifiers::default();
                }
            }
            // wl_output.mode 的 flags bit 0 = current；只记录当前模式，忽略其余可选模式。
            AnyEvent::WlOutput(wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            }) if flags & 1 != 0 && width > 0 && height > 0 => {
                self.output_mode = Some((width, height));
            }
            AnyEvent::WlOutput(wl_output::Event::Scale { factor }) if factor > 0 => {
                self.scale = factor;
                for (id, window) in &mut self.windows {
                    window.configuration.scale = factor;
                    if let Role::Toplevel { pending, .. } = &mut window.role { pending.scale = factor; }
                    if window.configured {
                        self.events.push_back(Event {
                            window: *id,
                            kind: EventKind::Configure(window.configuration),
                        });
                    }
                }
            }
            AnyEvent::WlSurface(wl_surface::Event::PreferredBufferScale { factor })
                if factor > 0 =>
            {
                if let Some(window) = self.windows.get_mut(&WindowId(object)) {
                    window.configuration.scale = factor;
                    if let Role::Toplevel { pending, .. } = &mut window.role { pending.scale = factor; }
                    if window.configured {
                        self.events.push_back(Event {
                            window: WindowId(object),
                            kind: EventKind::Configure(window.configuration),
                        });
                    }
                }
            }
            AnyEvent::XdgWmBase(xdg_wm_base::Event::Ping { serial }) => {
                xdg_wm_base::Request::Pong { serial }
                    .encode(object, &mut self.transport.writer)
                    .map_err(io::Error::other)?;
            }
            AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            }) => {
                if let Some(window) = self
                    .windows
                    .values_mut()
                    .find(|window| matches!(window.role, Role::Toplevel { toplevel, .. } if toplevel == object))
                {
                    let mut next = window.configuration;
                    if width > 0 {
                        next.width = width;
                    }
                    if height > 0 {
                        next.height = height;
                    }
                    let states: Vec<u32> = states
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|bytes| u32::from_ne_bytes(*bytes))
                        .collect();
                    next.maximized = states.contains(&1);
                    next.fullscreen = states.contains(&2);
                    next.resizing = states.contains(&3);
                    next.active = states.contains(&4);
                    next.pixel_size()?;
                    if let Role::Toplevel { pending, .. } = &mut window.role { *pending = next; }
                }
            }
            AnyEvent::XdgSurface(xdg_surface::Event::Configure { serial }) => {
                xdg_surface::Request::AckConfigure { serial }
                    .encode(object, &mut self.transport.writer)
                    .map_err(io::Error::other)?;
                if let Some((id, window)) = self
                    .windows
                    .iter_mut()
                    .find(|(_, window)| matches!(window.role, Role::Toplevel { xdg, .. } if xdg == object))
                {
                    if let Role::Toplevel { pending, .. } = window.role { window.configuration = pending; }
                    window.configured = true;
                    self.events.push_back(Event {
                        window: *id,
                        kind: EventKind::Configure(window.configuration),
                    });
                }
            }
            AnyEvent::XdgToplevel(xdg_toplevel::Event::Close) => {
                let window = self
                    .windows
                    .iter()
                    .find(|(_, window)| matches!(window.role, Role::Toplevel { toplevel, .. } if toplevel == object))
                    .map(|(id, _)| *id);
                self.emit(window, EventKind::Close);
            }
            AnyEvent::ZwlrLayerSurfaceV1(event) => self.layer_event(object, event)?,
            AnyEvent::WlBuffer(wl_buffer::Event::Release) => {
                self.release_buffer(object)?;
            }
            AnyEvent::WlCallback(wayland_wire::protocol::wl_callback::Event::Done {
                callback_data,
            }) => {
                if let Some((id, window)) = self
                    .windows
                    .iter_mut()
                    .find(|(_, window)| window.frame == Some(object))
                {
                    window.frame = None;
                    self.events.push_back(Event {
                        window: *id,
                        kind: EventKind::FrameReady {
                            time: callback_data,
                        },
                    });
                }
            }
            AnyEvent::WlPointer(wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            }) => {
                self.enter_serial = serial;
                self.shape = None;
                self.pointer = self
                    .windows
                    .contains_key(&WindowId(surface))
                    .then_some(WindowId(surface));
                self.emit(
                    self.pointer,
                    EventKind::PointerEnter {
                        x: surface_x.to_f64(),
                        y: surface_y.to_f64(),
                    },
                );
            }
            AnyEvent::WlPointer(wl_pointer::Event::Leave { .. }) => {
                let old = self.pointer.take();
                self.emit(old, EventKind::PointerLeave);
            }
            AnyEvent::WlPointer(wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            }) => {
                self.emit(
                    self.pointer,
                    EventKind::PointerMove {
                        x: surface_x.to_f64(),
                        y: surface_y.to_f64(),
                    },
                );
            }
            AnyEvent::WlPointer(wl_pointer::Event::Button {
                serial,
                button,
                state,
                ..
            }) => {
                self.clipboard.serial = serial;
                self.emit(
                    self.pointer,
                    EventKind::Button {
                        code: button,
                        pressed: state == wl_pointer::ButtonState::Pressed,
                        serial,
                    },
                );
            }
            AnyEvent::WlPointer(wl_pointer::Event::AxisDiscrete { axis, discrete }) => {
                self.scroll_discrete[axis as usize] += discrete;
            }
            AnyEvent::WlPointer(wl_pointer::Event::Axis { axis, value, .. }) => {
                self.scroll_continuous[axis as usize] += value.to_f64();
            }
            AnyEvent::WlPointer(wl_pointer::Event::Frame) => {
                // 有离散档位时以档位为准；只有连续值（触摸板）时每 10 逻辑像素折算一格。
                let notches = |axis: usize| {
                    if self.scroll_discrete[axis] != 0 {
                        self.scroll_discrete[axis]
                    } else {
                        (self.scroll_continuous[axis] / 10.0).round() as i32
                    }
                };
                let (vertical, horizontal) = (notches(0), notches(1));
                self.scroll_discrete = [0; 2];
                self.scroll_continuous = [0.0; 2];
                if vertical != 0 || horizontal != 0 {
                    self.emit(self.pointer, EventKind::Wheel { vertical, horizontal });
                }
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap { format, fd, size }) => {
                if format != wl_keyboard::KeymapFormat::XkbV1 || size > 1024 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "unsupported keyboard keymap",
                    ));
                }
                let mut bytes = Vec::new();
                File::from(fd)
                    .take(u64::from(size))
                    .read_to_end(&mut bytes)?;
                let mut expected = xkb::keymap_text().into_bytes();
                expected.push(0);
                if bytes != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "classic-ui requires the LiteOS US XKB keymap",
                    ));
                }
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { surface, .. }) => {
                self.keyboard = self
                    .windows
                    .contains_key(&WindowId(surface))
                    .then_some(WindowId(surface));
                self.repeat = None;
                self.emit(self.keyboard, EventKind::Focus(true));
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Leave { .. }) => {
                let old = self.keyboard.take();
                self.repeat = None;
                self.modifiers = Modifiers::default();
                self.emit(old, EventKind::Focus(false));
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            }) => {
                if group != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "multiple XKB layout groups are unsupported",
                    ));
                }
                self.modifiers =
                    xkb::modifiers_from_masks(mods_depressed, mods_latched, mods_locked);
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::RepeatInfo { rate, delay }) => {
                self.repeat_rate = rate.max(0);
                self.repeat_delay = delay.max(0);
                if self.repeat_rate == 0 {
                    self.repeat = None;
                }
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Key {
                serial, key, state, ..
            }) => {
                self.clipboard.serial = serial;
                let pressed = state == wl_keyboard::KeyState::Pressed;
                self.emit(self.keyboard, EventKind::Key(self.key(key, pressed, false)));
                if pressed && xkb::repeats(key) && self.repeat_rate > 0 {
                    self.repeat = self.keyboard.map(|window| Repeat {
                        window,
                        code: key,
                        next: Instant::now() + Duration::from_millis(self.repeat_delay as u64),
                    });
                } else if !pressed
                    && self
                        .repeat
                        .as_ref()
                        .is_some_and(|repeat| repeat.code == key)
                {
                    self.repeat = None;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Configuration, Window, transport::Transport};
    use super::*;
    use std::{
        collections::{HashMap, VecDeque},
        os::unix::net::UnixStream,
    };
    fn id(value: u32) -> ObjectId {
        ObjectId::new(value).unwrap()
    }
    fn display() -> (Display, UnixStream, WindowId) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let configuration = Configuration {
            width: 320,
            height: 200,
            scale: 2,
            active: false,
            maximized: false,
            fullscreen: false,
            resizing: false,
        };
        let window = WindowId(id(10));
        (
            Display {
                transport: Transport::new(stream).unwrap(),
                compositor: id(2),
                shm: id(3),
                wm_base: id(4),
                seat: id(5),
                scale: 2,
                output_mode: None,
                windows: HashMap::from([(
                    window,
                    Window {
                        role: Role::Toplevel {
                            xdg: id(11),
                            toplevel: id(12),
                            pending: configuration,
                        },
                        configuration,
                        configured: false,
                        mapped: false,
                        frame: None,
                        history: Default::default(),
                    },
                )]),
                buffers: HashMap::new(),
                events: VecDeque::new(),
                pointer: None,
                keyboard: None,
                modifiers: Modifiers::default(),
                scroll_discrete: [0; 2],
                scroll_continuous: [0.0; 2],
                repeat: None,
                repeat_rate: 30,
                repeat_delay: 500,
                pointer_device: None,
                keyboard_device: None,
                layer_shell: None,
                security_manager: None,
                dialog_manager: None,
                dialogs: HashMap::new(),
                pending_parents: HashMap::new(),
                poll_buffer: Default::default(),
                clipboard: Default::default(),
                cursor_shape_manager: None,
                cursor_shape_device: None,
                enter_serial: 0,
                shape: None,
                foreign: Default::default(),
                foreign_revision: 0,
            },
            peer,
            window,
        )
    }
    #[test]
    fn socket_release_precedes_an_overdue_synthetic_repeat() {
        use std::io::Write;
        use wayland_wire::wire::Writer;
        let (mut display, mut peer, window) = display();
        let keyboard = display.transport.allocate(Interface::WlKeyboard).unwrap();
        display.keyboard = Some(window);
        display.repeat = Some(Repeat {
            window,
            code: 28,
            next: Instant::now()
                .checked_sub(Duration::from_millis(1))
                .expect("test clock supports a previous millisecond"),
        });
        let mut writer = Writer::new();
        wl_keyboard::Event::Key {
            serial: 1,
            time: 1,
            key: 28,
            state: wl_keyboard::KeyState::Released,
        }
        .encode(keyboard, &mut writer)
        .unwrap();
        peer.write_all(writer.bytes()).unwrap();
        let event = display
            .next_event_timeout(Duration::from_millis(100))
            .unwrap()
            .unwrap();
        assert!(matches!(
            event.kind,
            EventKind::Key(Key {
                pressed: false,
                repeat: false,
                ..
            })
        ));
        assert!(display.repeat.is_none());
    }

    #[test]
    fn configure_is_atomic_at_xdg_surface_ack_and_zero_sizes_keep_the_previous_preference() {
        let (mut display, _peer, window) = display();
        display
            .dispatch(
                id(12),
                AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure {
                    width: 640,
                    height: 0,
                    states: 4u32.to_ne_bytes().to_vec(),
                }),
            )
            .unwrap();
        assert!(display.configuration(window).is_none());
        assert!(display.events.is_empty());
        display
            .dispatch(
                id(11),
                AnyEvent::XdgSurface(xdg_surface::Event::Configure { serial: 1 }),
            )
            .unwrap();
        let configuration = display.configuration(window).unwrap();
        assert_eq!(
            (
                configuration.width,
                configuration.height,
                configuration.pixel_size().unwrap()
            ),
            (640, 200, (1280, 400))
        );
        assert!(configuration.active);
        assert!(matches!(
            display.events.pop_front().unwrap().kind,
            EventKind::Configure(_)
        ));
        assert!(
            Configuration {
                width: i32::MAX,
                ..configuration
            }
            .pixel_size()
            .is_err()
        );
        assert!(display.present(window, &[0; 4]).is_err());
    }
    #[test]
    fn modifiers_are_authoritative_and_repeat_is_cancelled_on_release_and_focus_loss() {
        let (mut display, _peer, window) = display();
        display
            .dispatch(
                id(20),
                AnyEvent::WlKeyboard(wl_keyboard::Event::Enter {
                    serial: 1,
                    surface: window.0,
                    keys: vec![],
                }),
            )
            .unwrap();
        let (depressed, locked) = xkb::modifier_masks(Modifiers {
            shift: true,
            ..Modifiers::default()
        });
        display
            .dispatch(
                id(20),
                AnyEvent::WlKeyboard(wl_keyboard::Event::Modifiers {
                    serial: 2,
                    mods_depressed: depressed,
                    mods_latched: 0,
                    mods_locked: locked,
                    group: 0,
                }),
            )
            .unwrap();
        display
            .dispatch(
                id(20),
                AnyEvent::WlKeyboard(wl_keyboard::Event::Key {
                    serial: 3,
                    time: 0,
                    key: 30,
                    state: wl_keyboard::KeyState::Pressed,
                }),
            )
            .unwrap();
        assert_eq!(display.key(30, true, false).text, Some('A'));
        assert!(display.repeat.is_some());
        display
            .dispatch(
                id(20),
                AnyEvent::WlKeyboard(wl_keyboard::Event::Key {
                    serial: 4,
                    time: 0,
                    key: 30,
                    state: wl_keyboard::KeyState::Released,
                }),
            )
            .unwrap();
        assert!(display.repeat.is_none());
        display
            .dispatch(
                id(20),
                AnyEvent::WlKeyboard(wl_keyboard::Event::Key {
                    serial: 5,
                    time: 0,
                    key: 30,
                    state: wl_keyboard::KeyState::Pressed,
                }),
            )
            .unwrap();
        display
            .dispatch(
                id(20),
                AnyEvent::WlKeyboard(wl_keyboard::Event::Leave {
                    serial: 6,
                    surface: window.0,
                }),
            )
            .unwrap();
        assert!(display.repeat.is_none());
        assert_eq!(display.modifiers, Modifiers::default());
    }
    #[test]
    fn an_unsupported_keymap_is_rejected_before_character_translation() {
        use linux_uapi::shared_memory::MemFd;
        use std::{io::Write, os::fd::AsFd};
        let (mut display, _peer, _window) = display();
        let memory = MemFd::create("keymap-test", 4).unwrap();
        File::from(memory.as_fd().try_clone_to_owned().unwrap())
            .write_all(b"bad\0")
            .unwrap();
        let error = display
            .dispatch(
                id(20),
                AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap {
                    format: wl_keyboard::KeymapFormat::XkbV1,
                    fd: memory.as_fd().try_clone_to_owned().unwrap(),
                    size: 4,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
    #[test]
    fn layer_unmap_revokes_the_old_configuration_until_a_new_ack() {
        use crate::{Layer, LayerSettings};
        use wayland_wire::protocol::zwlr_layer_surface_v1 as layer;
        let (mut display, _peer, toplevel) = display();
        display.layer_shell = Some(id(30));
        let window = display
            .create_layer(
                "test-layer",
                LayerSettings {
                    layer: Layer::Overlay,
                    ..LayerSettings::default()
                },
            )
            .unwrap();
        assert!(display.configuration(window).is_none());
        let Role::Layer { object } = display.windows[&window].role else {
            panic!("layer role")
        };
        display
            .layer_event(
                object,
                layer::Event::Configure {
                    serial: 1,
                    width: 640,
                    height: 400,
                },
            )
            .unwrap();
        assert_eq!(
            display.configuration(window).unwrap().pixel_size().unwrap(),
            (1280, 800)
        );
        assert!(display.ready_to_draw(window));
        display.keyboard = Some(window);
        display.unmap_layer(window).unwrap();
        assert!(display.configuration(window).is_none());
        assert!(display.keyboard_focus().is_none());
        assert!(display.present(window, &[0]).is_err());
        display
            .configure_layer(window, LayerSettings::default())
            .unwrap();
        assert!(!display.ready_to_draw(window));
        display
            .layer_event(
                object,
                layer::Event::Configure {
                    serial: 2,
                    width: 640,
                    height: 400,
                },
            )
            .unwrap();
        assert!(display.ready_to_draw(window));
        assert!(
            display
                .request(window, super::super::WindowRequest::Minimize)
                .is_err()
        );
        assert!(
            display
                .configure_layer(toplevel, LayerSettings::default())
                .is_err()
        );
    }
}
