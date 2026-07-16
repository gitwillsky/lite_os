use core::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub struct Libseat {
    _private: [u8; 0],
}

#[repr(C)]
pub struct LibseatListener {
    pub enable_seat: unsafe extern "C" fn(*mut Libseat, *mut c_void),
    pub disable_seat: unsafe extern "C" fn(*mut Libseat, *mut c_void),
}

unsafe extern "C" {
    pub fn close(fd: c_int) -> c_int;
    pub fn calloc(count: usize, size: usize) -> *mut c_void;
    pub fn free(pointer: *mut c_void);
    pub fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
    pub fn libseat_open_seat(listener: *const LibseatListener, data: *mut c_void) -> *mut Libseat;
    pub fn libseat_close_seat(seat: *mut Libseat) -> c_int;
    pub fn libseat_disable_seat(seat: *mut Libseat) -> c_int;
    pub fn libseat_open_device(seat: *mut Libseat, path: *const c_char, fd: *mut c_int) -> c_int;
    pub fn libseat_close_device(seat: *mut Libseat, device_id: c_int) -> c_int;
    pub fn libseat_get_fd(seat: *mut Libseat) -> c_int;
    pub fn libseat_dispatch(seat: *mut Libseat, timeout: c_int) -> c_int;
    pub fn libseat_set_log_level(level: c_int);
}

pub const fn c_str(bytes: &'static [u8]) -> *const c_char {
    bytes.as_ptr().cast()
}
