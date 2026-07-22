//! evdev discovery and routing against compositor-presented scene state.

use std::{io, path::PathBuf};

use display_proto::PointerPhase;
use linux_uapi::input::{AbsoluteAxis, InputDevice, InputEvent};

use crate::session::Session;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_LEFT: u16 = 272;
const BTN_RIGHT: u16 = 273;
const BTN_MIDDLE: u16 = 274;
const EVENT_CAPACITY: usize = 64;

pub struct Input {
    keyboard: Option<InputDevice>,
    pointer: Option<InputDevice>,
    x_range: (i32, i32),
    y_range: (i32, i32),
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    pending_x: Option<i32>,
    pending_y: Option<i32>,
    pending_buttons: [(u32, u32, i32); 8],
    pending_button_count: usize,
    buttons: u32,
    modifiers: u32,
    serial: u64,
}

impl Input {
    pub fn open(width: i32, height: i32) -> Self {
        let keyboard = open_matching("keyboard");
        let pointer = open_matching("tablet").or_else(|| open_matching("mouse"));
        let x_range = pointer
            .as_ref()
            .and_then(|device| device.absolute_range(AbsoluteAxis::X).ok())
            .map_or((0, 0), |range| (range.minimum, range.maximum));
        let y_range = pointer
            .as_ref()
            .and_then(|device| device.absolute_range(AbsoluteAxis::Y).ok())
            .map_or((0, 0), |range| (range.minimum, range.maximum));
        Self {
            keyboard,
            pointer,
            x_range,
            y_range,
            width,
            height,
            x: width / 2,
            y: height / 2,
            pending_x: None,
            pending_y: None,
            pending_buttons: [(0, 0, 0); 8],
            pending_button_count: 0,
            buttons: 0,
            modifiers: 0,
            serial: 1,
        }
    }

    pub fn poll(&mut self, session: &mut Session) -> io::Result<bool> {
        let before = (self.x, self.y);
        self.poll_keyboard(session)?;
        self.poll_pointer(session)?;
        Ok(before != (self.x, self.y))
    }

    pub fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    pub fn cursor_revision(&self) -> u64 {
        1 << 63 | self.serial
    }

    fn poll_keyboard(&mut self, session: &Session) -> io::Result<()> {
        let Some(device) = self.keyboard.as_mut() else {
            return Ok(());
        };
        let mut events = [InputEvent::EMPTY; EVENT_CAPACITY];
        let count = read_events(device, &mut events)?;
        for event in &events[..count] {
            if event.kind() != EV_KEY {
                continue;
            }
            update_modifier(&mut self.modifiers, event.code(), event.value());
            session.route_key(u32::from(event.code()), event.value(), self.modifiers)?;
        }
        Ok(())
    }

    fn poll_pointer(&mut self, session: &mut Session) -> io::Result<()> {
        let Some(device) = self.pointer.as_mut() else {
            return Ok(());
        };
        let mut events = [InputEvent::EMPTY; EVENT_CAPACITY];
        let count = read_events(device, &mut events)?;
        for event in &events[..count] {
            match event.kind() {
                EV_ABS if event.code() == ABS_X => self.pending_x = Some(event.value()),
                EV_ABS if event.code() == ABS_Y => self.pending_y = Some(event.value()),
                EV_KEY => {
                    if let Some((button, bit)) = button(event.code())
                        && self.pending_button_count < self.pending_buttons.len()
                    {
                        self.pending_buttons[self.pending_button_count] =
                            (button, bit, event.value());
                        self.pending_button_count += 1;
                    }
                }
                EV_SYN if event.code() == SYN_REPORT => self.flush_pointer(session)?,
                _ => {}
            }
        }
        if self.pending_x.is_some() || self.pending_y.is_some() || self.pending_button_count != 0 {
            self.flush_pointer(session)?;
        }
        Ok(())
    }

    fn flush_pointer(&mut self, session: &mut Session) -> io::Result<()> {
        let old = (self.x, self.y);
        if let Some(raw) = self.pending_x.take() {
            self.x = map_absolute(raw, self.x_range, self.width);
        }
        if let Some(raw) = self.pending_y.take() {
            self.y = map_absolute(raw, self.y_range, self.height);
        }
        if old != (self.x, self.y) {
            session.route_pointer(
                self.x,
                self.y,
                PointerPhase::Motion,
                0,
                self.buttons,
                self.take_serial(),
            )?;
        }
        for index in 0..self.pending_button_count {
            let (button, bit, value) = self.pending_buttons[index];
            let phase = match value {
                1 => {
                    self.buttons |= bit;
                    PointerPhase::Down
                }
                0 => {
                    self.buttons &= !bit;
                    PointerPhase::Up
                }
                _ => continue,
            };
            session.route_pointer(
                self.x,
                self.y,
                phase,
                button,
                self.buttons,
                self.take_serial(),
            )?;
        }
        self.pending_button_count = 0;
        Ok(())
    }

    fn take_serial(&mut self) -> u64 {
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1).max(1);
        serial
    }
}

fn read_events(device: &mut InputDevice, events: &mut [InputEvent]) -> io::Result<usize> {
    let mut total = 0;
    while total < events.len() {
        match device.read_events(&mut events[total..]) {
            Ok(0) => break,
            Ok(count) => total += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
    }
    Ok(total)
}

fn open_matching(needle: &str) -> Option<InputDevice> {
    for index in 0..16u32 {
        let path = PathBuf::from(format!("/dev/input/event{index}"));
        let Ok(device) = InputDevice::open(&path) else {
            continue;
        };
        if device
            .name()
            .is_ok_and(|name| name.to_ascii_lowercase().contains(needle))
            && device.grab().is_ok()
        {
            return Some(device);
        }
    }
    None
}

fn button(code: u16) -> Option<(u32, u32)> {
    match code {
        BTN_LEFT => Some((u32::from(code), 1)),
        BTN_RIGHT => Some((u32::from(code), 2)),
        BTN_MIDDLE => Some((u32::from(code), 4)),
        _ => None,
    }
}

fn update_modifier(modifiers: &mut u32, code: u16, value: i32) {
    let bit = match code {
        42 | 54 => 1,
        29 | 97 => 2,
        56 | 100 => 4,
        125 | 126 => 8,
        _ => return,
    };
    if value == 0 {
        *modifiers &= !bit;
    } else {
        *modifiers |= bit;
    }
}

fn map_absolute(raw: i32, range: (i32, i32), extent: i32) -> i32 {
    let (minimum, maximum) = range;
    if maximum <= minimum || extent <= 0 {
        return 0;
    }
    let scaled = i64::from(raw - minimum) * i64::from(extent - 1) / i64::from(maximum - minimum);
    (scaled as i32).clamp(0, extent - 1)
}
