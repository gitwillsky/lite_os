use display_client::{Device, Seat};

use crate::{
    ffi::{self, InputAbsInfo, InputEvent},
    scene::{Damage, Scene, TerminalPointer},
};
use liteui_core::NodeId;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const EV_REL: u16 = 2;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_LEFT: u16 = 272;
const BTN_RIGHT: u16 = 273;
const BTN_MIDDLE: u16 = 274;
const REL_WHEEL: u16 = 8;

pub struct Input {
    keyboard: Device,
    pointer: Option<(Device, Pointer)>,
}

struct Pointer {
    x: InputAbsInfo,
    y: InputAbsInfo,
    left_down: bool,
    position_pending: bool,
    button_pending: bool,
    action_pending: Option<(u8, bool)>,
    dropped: bool,
}

pub struct Change {
    pub damage: Damage,
    pub quit: bool,
    pub event: Option<NodeId>,
    pub pointer: Option<TerminalPointer>,
    keys: [KeyEvent; 32],
    key_count: usize,
}

#[derive(Clone, Copy)]
pub struct KeyEvent {
    pub code: u16,
    pub value: i32,
}

const EMPTY_KEY: KeyEvent = KeyEvent { code: 0, value: 0 };

impl Change {
    fn empty() -> Self {
        Self {
            damage: Damage::EMPTY,
            quit: false,
            event: None,
            pointer: None,
            keys: [EMPTY_KEY; 32],
            key_count: 0,
        }
    }

    fn merge_scene(&mut self, update: (Damage, bool)) {
        self.damage.merge(update.0);
    }

    fn push_key(&mut self, event: KeyEvent) -> Result<(), ()> {
        let slot = self.keys.get_mut(self.key_count).ok_or(())?;
        *slot = event;
        self.key_count += 1;
        Ok(())
    }

    pub fn keys(&self) -> &[KeyEvent] {
        &self.keys[..self.key_count]
    }
}

impl Input {
    pub fn open(seat: &mut Seat) -> Result<Self, ()> {
        let keyboard = open_matching(seat, &[b"keyboard"])?.ok_or(())?;
        let pointer_device = match open_matching(seat, &[b"tablet", b"mouse"]) {
            Ok(device) => device,
            Err(()) => {
                seat.close_device(keyboard)?;
                return Err(());
            }
        };
        let pointer = match pointer_device {
            Some(device) => match Pointer::open(device.fd) {
                Some(pointer) => Some((device, pointer)),
                None => {
                    if seat.close_device(device).is_err() {
                        let _ = seat.close_device(keyboard);
                        return Err(());
                    }
                    None
                }
            },
            None => None,
        };
        Ok(Self { keyboard, pointer })
    }

    pub fn keyboard_fd(&self) -> i32 {
        self.keyboard.fd
    }

    pub fn pointer_fd(&self) -> i32 {
        self.pointer.as_ref().map_or(-1, |(device, _)| device.fd)
    }

    pub fn read_keyboard(&self, scene: &mut Scene) -> Result<Change, ()> {
        let mut events = empty_events();
        let count = read_events(self.keyboard.fd, &mut events)?;
        let mut change = Change::empty();
        for event in &events[..count] {
            if event.kind == EV_SYN && event.code == SYN_DROPPED && scene.terminal_focused() {
                change.push_key(KeyEvent {
                    code: u16::MAX,
                    value: 0,
                })?;
                continue;
            }
            if event.kind != EV_KEY {
                continue;
            }
            if scene.terminal_focused() {
                change.push_key(KeyEvent {
                    code: event.code,
                    value: event.value,
                })?;
                continue;
            }
            if event.value == 0 {
                continue;
            }
            let update = match event.code {
                1 | 16 => {
                    change.quit = true;
                    continue;
                }
                103 => (scene.move_window(0, -24)?, true),
                108 => (scene.move_window(0, 24)?, true),
                105 => (scene.move_window(-24, 0)?, true),
                106 => (scene.move_window(24, 0)?, true),
                57 => (scene.cycle_accent()?, false),
                _ => continue,
            };
            change.merge_scene(update);
        }
        Ok(change)
    }

    pub fn read_pointer(&mut self, scene: &mut Scene) -> Result<Change, ()> {
        let Some((device, pointer)) = self.pointer.as_mut() else {
            return Ok(Change::empty());
        };
        let mut events = empty_events();
        let count = read_events(device.fd, &mut events)?;
        let mut change = Change::empty();
        for event in &events[..count] {
            match (event.kind, event.code) {
                (EV_ABS, ABS_X) if !pointer.dropped => {
                    pointer.x.value = event.value;
                    pointer.position_pending = true;
                }
                (EV_ABS, ABS_Y) if !pointer.dropped => {
                    pointer.y.value = event.value;
                    pointer.position_pending = true;
                }
                (EV_KEY, BTN_LEFT) if !pointer.dropped => {
                    let left_down = event.value != 0;
                    pointer.button_pending |= left_down != pointer.left_down;
                    pointer.left_down = left_down;
                    pointer.action_pending = Some((0, left_down));
                }
                (EV_KEY, BTN_MIDDLE) if !pointer.dropped => {
                    pointer.action_pending = Some((1, event.value != 0));
                }
                (EV_KEY, BTN_RIGHT) if !pointer.dropped => {
                    pointer.action_pending = Some((2, event.value != 0));
                }
                (EV_REL, REL_WHEEL) if !pointer.dropped && event.value > 0 => {
                    pointer.action_pending = Some((64, true));
                }
                (EV_REL, REL_WHEEL) if !pointer.dropped && event.value < 0 => {
                    pointer.action_pending = Some((65, true));
                }
                (EV_SYN, SYN_DROPPED) => pointer.begin_recovery(),
                (EV_SYN, SYN_REPORT) => {
                    if pointer.dropped {
                        pointer.resynchronize(device.fd)?;
                    }
                    pointer.publish(scene, &mut change)?;
                }
                _ => {}
            }
        }
        Ok(change)
    }

    pub fn release(mut self, seat: &mut Seat) -> Result<(), ()> {
        let mut failed = false;
        if let Some((device, _)) = self.pointer.take() {
            failed |= seat.close_device(device).is_err();
        }
        failed |= seat.close_device(self.keyboard).is_err();
        (!failed).then_some(()).ok_or(())
    }
}

impl Pointer {
    fn open(fd: i32) -> Option<Self> {
        let mut pointer = Self {
            x: InputAbsInfo::default(),
            y: InputAbsInfo::default(),
            left_down: false,
            position_pending: false,
            button_pending: false,
            action_pending: None,
            dropped: false,
        };
        pointer.read_snapshot(fd).ok()?;
        (pointer.x.minimum < pointer.x.maximum && pointer.y.minimum < pointer.y.maximum)
            .then_some(pointer)
    }

    fn begin_recovery(&mut self) {
        // SYN_DROPPED 后的增量事件没有完整前态。忽略到下个 SYN_REPORT，再以
        // EVIOCG* 快照覆盖；若继续消费增量，按键释放或坐标会永久丢失。
        self.dropped = true;
        self.position_pending = false;
        self.button_pending = false;
        self.action_pending = None;
    }

    fn resynchronize(&mut self, fd: i32) -> Result<(), ()> {
        let old_left = self.left_down;
        self.read_snapshot(fd)?;
        self.position_pending = true;
        self.button_pending = old_left != self.left_down;
        self.dropped = false;
        Ok(())
    }

    fn read_snapshot(&mut self, fd: i32) -> Result<(), ()> {
        let mut keys = [0u8; 96];
        let failed = unsafe {
            ffi::ioctl(
                fd,
                ffi::EVIOCGABS_X,
                (&mut self.x as *mut InputAbsInfo).cast(),
            ) < 0
                || ffi::ioctl(
                    fd,
                    ffi::EVIOCGABS_Y,
                    (&mut self.y as *mut InputAbsInfo).cast(),
                ) < 0
                || ffi::ioctl(fd, ffi::EVIOCGKEY_96, keys.as_mut_ptr().cast()) < 0
        };
        if failed {
            return Err(());
        }
        self.left_down = keys[usize::from(BTN_LEFT) / 8] & (1 << (BTN_LEFT % 8)) != 0;
        Ok(())
    }

    fn publish(&mut self, scene: &mut Scene, change: &mut Change) -> Result<(), ()> {
        if self.position_pending {
            let (width, height) = scene.dimensions();
            change.merge_scene(scene.move_pointer(axis(self.x, width), axis(self.y, height))?);
            self.position_pending = false;
        }
        if self.button_pending {
            let update = scene.set_primary_button(self.left_down)?;
            change.damage.merge(update.0);
            change.event = update.2;
            self.button_pending = false;
        }
        if let Some((button, pressed)) = self.action_pending.take() {
            change.pointer = scene.terminal_pointer(button, pressed);
        }
        Ok(())
    }
}

fn read_events(fd: i32, events: &mut [InputEvent; 32]) -> Result<usize, ()> {
    let count = unsafe {
        ffi::read(
            fd,
            events.as_mut_ptr().cast(),
            core::mem::size_of_val(events),
        )
    };
    if count <= 0 || count as usize % core::mem::size_of::<InputEvent>() != 0 {
        return Err(());
    }
    Ok(count as usize / core::mem::size_of::<InputEvent>())
}

fn open_matching(seat: &mut Seat, needles: &[&[u8]]) -> Result<Option<Device>, ()> {
    for index in 0..16u32 {
        let mut path = [0u8; 32];
        let prefix = b"/dev/input/event";
        path[..prefix.len()].copy_from_slice(prefix);
        let length = prefix.len() + decimal(index, &mut path[prefix.len()..31]);
        path[length] = 0;
        let Ok(device) = seat.open_device(path.as_ptr().cast()) else {
            continue;
        };
        let mut name = [0u8; 128];
        let named =
            unsafe { ffi::ioctl(device.fd, ffi::EVIOCGNAME_128, name.as_mut_ptr().cast()) } >= 0;
        if named && needles.iter().any(|needle| contains(&name, needle)) {
            let mut grab = 1i32;
            unsafe { ffi::ioctl(device.fd, ffi::EVIOCGRAB, (&mut grab as *mut i32).cast()) };
            return Ok(Some(device));
        }
        seat.close_device(device)?;
    }
    Ok(None)
}

fn empty_events() -> [InputEvent; 32] {
    [InputEvent {
        seconds: 0,
        microseconds: 0,
        kind: 0,
        code: 0,
        value: 0,
    }; 32]
}

fn axis(value: InputAbsInfo, pixels: usize) -> usize {
    let span = i64::from(value.maximum) - i64::from(value.minimum) + 1;
    let offset = (i64::from(value.value) - i64::from(value.minimum)).clamp(0, span - 1);
    (offset as usize).saturating_mul(pixels) / span as usize
}

fn decimal(mut value: u32, output: &mut [u8]) -> usize {
    let mut reversed = [0u8; 10];
    let mut length = 0;
    loop {
        reversed[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..length {
        output[index] = reversed[length - index - 1];
    }
    length
}

fn contains(name: &[u8], needle: &[u8]) -> bool {
    let mut matched = 0;
    for byte in name.iter().copied().take_while(|byte| *byte != 0) {
        let value = byte.to_ascii_lowercase();
        matched = if value == needle[matched] {
            matched + 1
        } else {
            usize::from(value == needle[0])
        };
        if matched == needle.len() {
            return true;
        }
    }
    false
}
