use display_client::{Device, Seat};

use crate::{
    ffi::{self, InputAbsInfo, InputEvent},
    scene::{Rect, Scene},
};

pub struct Input {
    keyboard: Device,
    pointer: Option<(Device, Pointer)>,
}

struct Pointer {
    x: InputAbsInfo,
    y: InputAbsInfo,
}

pub struct Change {
    pub damage: Option<Rect>,
    pub quit: bool,
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
        let count = unsafe {
            ffi::read(
                self.keyboard.fd,
                events.as_mut_ptr().cast(),
                core::mem::size_of_val(&events),
            )
        };
        if count <= 0 || count as usize % core::mem::size_of::<InputEvent>() != 0 {
            return Err(());
        }
        let mut damage = None;
        let mut quit = false;
        for event in &events[..count as usize / core::mem::size_of::<InputEvent>()] {
            if event.kind != 1 || event.value == 0 {
                continue;
            }
            let next = match event.code {
                1 | 16 => {
                    quit = true;
                    None
                }
                103 => Some(scene.move_square(0, -24)),
                108 => Some(scene.move_square(0, 24)),
                105 => Some(scene.move_square(-24, 0)),
                106 => Some(scene.move_square(24, 0)),
                57 => Some(scene.cycle_color()),
                _ => None,
            };
            if let Some(next) = next {
                damage = Some(damage.map_or(next, |current: Rect| current.union(next)));
            }
        }
        Ok(Change { damage, quit })
    }

    pub fn read_pointer(&mut self, scene: &mut Scene) -> Result<Option<Rect>, ()> {
        let Some((device, pointer)) = self.pointer.as_mut() else {
            return Ok(None);
        };
        let mut events = empty_events();
        let count = unsafe {
            ffi::read(
                device.fd,
                events.as_mut_ptr().cast(),
                core::mem::size_of_val(&events),
            )
        };
        if count <= 0 || count as usize % core::mem::size_of::<InputEvent>() != 0 {
            return Err(());
        }
        let mut changed = false;
        let mut report = false;
        for event in &events[..count as usize / core::mem::size_of::<InputEvent>()] {
            match (event.kind, event.code) {
                (3, 0) => {
                    pointer.x.value = event.value;
                    changed = true;
                }
                (3, 1) => {
                    pointer.y.value = event.value;
                    changed = true;
                }
                (0, 3) => changed = false,
                (0, 0) => report = changed,
                _ => {}
            }
        }
        if !report {
            return Ok(None);
        }
        let (width, height) = scene.dimensions();
        Ok(Some(scene.move_pointer(
            axis(pointer.x, width),
            axis(pointer.y, height),
        )))
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
        let mut x = InputAbsInfo::default();
        let mut y = InputAbsInfo::default();
        if unsafe {
            ffi::ioctl(fd, ffi::EVIOCGABS_X, (&mut x as *mut InputAbsInfo).cast()) < 0
                || ffi::ioctl(fd, ffi::EVIOCGABS_Y, (&mut y as *mut InputAbsInfo).cast()) < 0
        } || x.minimum >= x.maximum
            || y.minimum >= y.maximum
        {
            return None;
        }
        Some(Self { x, y })
    }
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
