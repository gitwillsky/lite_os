use crate::{
    ffi::{self, InputAbsInfo, InputEvent},
    model::{Grid, Model},
};

use super::input::InputQueue;

pub(super) const MAX_POINTER_BYTES: usize = 6;

pub(super) struct Pointer {
    fd: i32,
    x: InputAbsInfo,
    y: InputAbsInfo,
    pending: Option<(u8, bool)>,
}

impl Pointer {
    pub(super) fn open(fd: i32) -> Option<Self> {
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
        Some(Self {
            fd,
            x,
            y,
            pending: None,
        })
    }

    pub(super) fn fd(&self) -> i32 {
        self.fd
    }

    pub(super) fn read(&mut self, input: &mut InputQueue, model: &Model) {
        let mut events = [InputEvent {
            seconds: 0,
            microseconds: 0,
            kind: 0,
            code: 0,
            value: 0,
        }; 32];
        let capacity = events.len().min(input.remaining() / MAX_POINTER_BYTES)
            * core::mem::size_of::<InputEvent>();
        if capacity == 0 {
            return;
        }
        let count = unsafe { ffi::read(self.fd, events.as_mut_ptr().cast(), capacity) };
        if count <= 0 {
            return;
        }
        for event in &events[..count as usize / core::mem::size_of::<InputEvent>()] {
            self.handle(input, model, event);
        }
    }

    fn handle(&mut self, input: &mut InputQueue, model: &Model, event: &InputEvent) {
        match (event.kind, event.code) {
            (0, 3) => self.pending = None,
            (0, 0) => self.report(input, model),
            (3, 0) => self.x.value = event.value,
            (3, 1) => self.y.value = event.value,
            (1, 272) => self.pending = Some((0, event.value != 0)),
            (1, 274) => self.pending = Some((1, event.value != 0)),
            (1, 273) => self.pending = Some((2, event.value != 0)),
            (2, 8) if event.value > 0 => self.pending = Some((64, true)),
            (2, 8) if event.value < 0 => self.pending = Some((65, true)),
            _ => {}
        }
    }

    fn report(&mut self, input: &mut InputQueue, model: &Model) {
        let Some((button, pressed)) = self.pending.take() else {
            return;
        };
        let mode = model.mouse_mode();
        if mode == 0 || mode == 1 && !pressed {
            return;
        }
        let button = if pressed { button } else { 3 };
        let column = axis_cell(self.x, model.columns()).min(222);
        let row = axis_cell(self.y, model.rows()).min(222);
        input.push(&[
            0x1b,
            b'[',
            b'M',
            32 + button,
            32 + column as u8 + 1,
            32 + row as u8 + 1,
        ]);
    }
}

fn axis_cell(axis: InputAbsInfo, cells: usize) -> usize {
    let span = i64::from(axis.maximum) - i64::from(axis.minimum) + 1;
    let offset = (i64::from(axis.value) - i64::from(axis.minimum)).clamp(0, span - 1);
    (offset as usize).saturating_mul(cells) / span as usize
}
