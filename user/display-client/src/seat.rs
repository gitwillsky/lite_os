use core::{
    ffi::{c_char, c_void},
    ptr,
};

use crate::ffi::{self, Libseat, LibseatListener};

struct SeatState {
    listener: LibseatListener,
    enabled: bool,
    changed: bool,
}

/// 固定 seatd backend 的唯一 client-side capability session。
pub struct Seat {
    raw: *mut Libseat,
    state: *mut SeatState,
    fd: i32,
}

/// broker device id 与 client fd 的成对所有权。
#[derive(Clone, Copy)]
pub struct Device {
    pub id: i32,
    pub fd: i32,
}

impl Seat {
    /// 打开固定 seatd backend，并使 callback userdata 在整个 libseat lifetime 内地址稳定。
    pub fn open() -> Result<Self, ()> {
        unsafe {
            ffi::setenv(ffi::c_str(b"LIBSEAT_BACKEND\0"), ffi::c_str(b"seatd\0"), 1);
            ffi::libseat_set_log_level(1);
        }
        let state =
            unsafe { ffi::calloc(1, core::mem::size_of::<SeatState>()).cast::<SeatState>() };
        if state.is_null() {
            return Err(());
        }
        unsafe {
            state.write(SeatState {
                listener: LibseatListener {
                    enable_seat: enable,
                    disable_seat: disable,
                },
                enabled: true,
                changed: false,
            });
        }
        let raw = unsafe { ffi::libseat_open_seat(&(*state).listener, state.cast()) };
        if raw.is_null() {
            unsafe { ffi::free(state.cast()) };
            return Err(());
        }
        let fd = unsafe { ffi::libseat_get_fd(raw) };
        if fd < 0 {
            unsafe {
                ffi::libseat_close_seat(raw);
                ffi::free(state.cast());
            }
            return Err(());
        }
        Ok(Self { raw, state, fd })
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    pub fn dispatch(&mut self) -> Result<(), ()> {
        (unsafe { ffi::libseat_dispatch(self.raw, 0) } >= 0)
            .then_some(())
            .ok_or(())
    }

    pub fn take_change(&mut self) -> Option<bool> {
        let state = unsafe { &mut *self.state };
        if !state.changed {
            return None;
        }
        state.changed = false;
        Some(state.enabled)
    }

    pub fn acknowledge_disable(&mut self) -> Result<(), ()> {
        if unsafe { (*self.state).enabled } || unsafe { ffi::libseat_disable_seat(self.raw) } < 0 {
            return Err(());
        }
        Ok(())
    }

    pub fn open_device(&mut self, path: *const c_char) -> Result<Device, ()> {
        let mut fd = -1;
        let id = unsafe { ffi::libseat_open_device(self.raw, path, &mut fd) };
        if id < 0 || fd < 0 {
            if fd >= 0 {
                unsafe { ffi::close(fd) };
            }
            return Err(());
        }
        Ok(Device { id, fd })
    }

    /// 先销毁 client fd，再通知 broker 释放其保留的同一 OFD。
    pub fn close_device(&mut self, device: Device) -> Result<(), ()> {
        unsafe { ffi::close(device.fd) };
        (unsafe { ffi::libseat_close_device(self.raw, device.id) } >= 0)
            .then_some(())
            .ok_or(())
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        unsafe {
            ffi::libseat_close_seat(self.raw);
            ffi::free(self.state.cast());
        }
        self.raw = ptr::null_mut();
    }
}

unsafe extern "C" fn enable(_seat: *mut Libseat, data: *mut c_void) {
    let state = unsafe { &mut *data.cast::<SeatState>() };
    state.enabled = true;
    state.changed = true;
}

unsafe extern "C" fn disable(_seat: *mut Libseat, data: *mut c_void) {
    let state = unsafe { &mut *data.cast::<SeatState>() };
    state.enabled = false;
    state.changed = true;
}
