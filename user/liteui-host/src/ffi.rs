use core::ffi::{c_char, c_int, c_void};

pub const O_RDONLY: c_int = 0;
pub const O_CLOEXEC: c_int = 0x80000;
pub const EINTR: c_int = 4;
pub const ENOENT: c_int = 2;
pub const O_WRONLY: c_int = 1;
pub const O_CREAT: c_int = 0x40;
pub const O_TRUNC: c_int = 0x200;
pub const O_NONBLOCK: c_int = 0x800;
pub const AF_UNIX: c_int = 1;
pub const SOCK_STREAM: c_int = 1;
pub const SOCK_CLOEXEC: c_int = O_CLOEXEC;
pub const F_SETFL: c_int = 4;
pub const POLLIN: i16 = 1;
pub const POLLOUT: i16 = 4;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 16;
pub const EAGAIN: c_int = 11;
pub const MSG_NOSIGNAL: c_int = 0x4000;

#[repr(C)]
pub struct SockaddrUn {
    pub family: u16,
    pub path: [u8; 108],
}

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub returned: i16,
}

#[repr(C)]
pub struct LiteJs {
    _private: [u8; 0],
}

pub type CommitCallback = unsafe extern "C" fn(*mut c_void, *const u8, usize, u32) -> c_int;

unsafe extern "C" {
    pub fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn getuid() -> u32;
    pub fn read(fd: c_int, output: *mut c_void, length: usize) -> isize;
    pub fn write(fd: c_int, input: *const c_void, length: usize) -> isize;
    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    pub fn connect(fd: c_int, address: *const SockaddrUn, length: u32) -> c_int;
    pub fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    pub fn send(fd: c_int, input: *const c_void, length: usize, flags: c_int) -> isize;
    pub fn poll(descriptors: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    pub fn malloc(size: usize) -> *mut c_void;
    pub fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
    pub fn free(pointer: *mut c_void);
    pub fn fsync(fd: c_int) -> c_int;
    pub fn rename(old_path: *const c_char, new_path: *const c_char) -> c_int;
    pub fn unlink(path: *const c_char) -> c_int;
    pub fn __errno_location() -> *mut c_int;
    pub fn _exit(status: c_int) -> !;

    pub fn litejs_create(
        heap_limit: usize,
        stack_limit: usize,
        opaque: *mut c_void,
        commit: CommitCallback,
    ) -> *mut LiteJs;
    pub fn litejs_destroy(engine: *mut LiteJs);
    pub fn litejs_compile_module(
        engine: *mut LiteJs,
        source: *const u8,
        source_length: usize,
        filename: *const c_char,
        compile_deadline_ms: u32,
        evaluate_deadline_ms: u32,
        bytecode: *mut *mut u8,
        bytecode_length: *mut usize,
        error: *mut u8,
        error_capacity: usize,
    ) -> c_int;
    pub fn litejs_eval_bytecode(
        engine: *mut LiteJs,
        bytecode: *const u8,
        bytecode_length: usize,
        deadline_ms: u32,
        error: *mut u8,
        error_capacity: usize,
    ) -> c_int;
    pub fn litejs_free_buffer(engine: *mut LiteJs, buffer: *mut u8);
    pub fn litejs_execute_jobs(
        engine: *mut LiteJs,
        budget: u32,
        deadline_ms: u32,
        error: *mut u8,
        error_capacity: usize,
    ) -> c_int;
    pub fn litejs_dispatch_click(
        engine: *mut LiteJs,
        node: u16,
        generation: u16,
        deadline_ms: u32,
        error: *mut u8,
        error_capacity: usize,
    ) -> c_int;
}

pub fn errno() -> c_int {
    // SAFETY: musl 返回当前线程有效的 TLS errno 地址。
    unsafe { *__errno_location() }
}

pub fn write_stderr(message: &[u8]) {
    let mut written = 0;
    while written < message.len() {
        let count = unsafe {
            write(
                2,
                message[written..].as_ptr().cast(),
                message.len() - written,
            )
        };
        if count > 0 {
            written += count as usize;
        } else if count < 0 && errno() == EINTR {
            continue;
        } else {
            return;
        }
    }
}
