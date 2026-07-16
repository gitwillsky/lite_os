use display_client::{Device, Seat};

use crate::{
    atlas::{Atlas, FontMetrics},
    display::Display,
    ffi,
    model::{Grid, Model},
};

use super::{evdev, input::open_keyboard, pointer::Pointer, session::set_window_size};

pub(super) struct Active {
    pub display: Display,
    drm: Device,
    keyboard: Option<Device>,
    pointer: Option<(Device, Pointer)>,
}

impl Active {
    pub(super) fn open(seat: &mut Seat) -> Result<Self, ()> {
        let drm = seat.open_device(ffi::c_str(b"/dev/dri/card0\0"))?;
        let display = match Display::open(drm.fd) {
            Ok(display) => display,
            Err(_) => {
                seat.close_device(drm)?;
                return Err(());
            }
        };
        let keyboard = match open_keyboard(seat) {
            Ok(device) => device,
            Err(()) => {
                drop(display);
                close_devices(seat, [None, None, Some(drm)])?;
                return Err(());
            }
        };
        let pointer_device = match evdev::open_matching(seat, &[b"tablet", b"mouse"]) {
            Ok(device) => device,
            Err(()) => {
                drop(display);
                close_devices(seat, [None, keyboard, Some(drm)])?;
                return Err(());
            }
        };
        let pointer = match pointer_device {
            Some(device) => match Pointer::open(device.fd) {
                Some(pointer) => Some((device, pointer)),
                None => {
                    if seat.close_device(device).is_err() {
                        drop(display);
                        close_devices(seat, [None, keyboard, Some(drm)])?;
                        return Err(());
                    }
                    None
                }
            },
            None => None,
        };
        Ok(Self {
            display,
            drm,
            keyboard,
            pointer,
        })
    }

    pub(super) fn keyboard_fd(&self) -> i32 {
        self.keyboard.map_or(-1, |device| device.fd)
    }

    pub(super) fn pointer(&self) -> Option<&Pointer> {
        self.pointer.as_ref().map(|(_, pointer)| pointer)
    }

    pub(super) fn pointer_mut(&mut self) -> Option<&mut Pointer> {
        self.pointer.as_mut().map(|(_, pointer)| pointer)
    }

    pub(super) fn release(mut self, seat: &mut Seat) -> Result<(), ()> {
        let pointer = self.pointer.take().map(|(device, pointer)| {
            drop(pointer);
            device
        });
        let keyboard = self.keyboard.take();
        let drm = self.drm;
        drop(self.display);
        // 1. renderer/input objects 已停止访问 fd；2. client fd 关闭；3. broker 释放保留 OFD。
        // 缺少此顺序会让 disable ACK 先于 framebuffer/GEM teardown 对外可见。
        close_devices(seat, [pointer, keyboard, Some(drm)])
    }

    pub(super) fn reacquire(
        seat: &mut Seat,
        atlas: &Atlas,
        model: &mut Model,
        metrics: FontMetrics,
        master: i32,
    ) -> Result<Self, ()> {
        let mut active = Self::open(seat)?;
        let result = (|| {
            let mode = active.display.query_mode().map_err(|_| ())?;
            let columns = usize::from(mode.hdisplay) / metrics.width();
            let rows = usize::from(mode.vdisplay) / metrics.height();
            if columns == 0 || rows == 0 {
                return Err(());
            }
            if columns == model.columns() && rows == model.rows() {
                let mut buffer = active
                    .display
                    .prepare(mode, model, atlas, metrics)
                    .map_err(|_| ())?;
                active.display.commit(&mut buffer).map_err(|_| ())?;
            } else {
                let candidate = model.prepare_resize(columns, rows).ok_or(())?;
                let mut buffer = active
                    .display
                    .prepare(mode, &candidate, atlas, metrics)
                    .map_err(|_| ())?;
                active.display.commit(&mut buffer).map_err(|_| ())?;
                model.commit_resize(candidate);
            }
            model.clear_all_dirty();
            set_window_size(master, columns, rows, mode.hdisplay, mode.vdisplay)
        })();
        if result.is_err() {
            let _ = active.release(seat);
            return Err(());
        }
        Ok(active)
    }
}

fn close_devices(seat: &mut Seat, devices: [Option<Device>; 3]) -> Result<(), ()> {
    let mut failed = false;
    for device in devices.into_iter().flatten() {
        failed |= seat.close_device(device).is_err();
    }
    (!failed).then_some(()).ok_or(())
}
