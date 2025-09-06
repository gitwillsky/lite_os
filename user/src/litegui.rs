use crate::protocol::*;
use crate::{mmap_flags, munmap, shm_close, shm_create, shm_map, uds_connect, write};

pub struct LiteGuiConnection {
    fd: usize,
}

impl LiteGuiConnection {
    pub fn connect(path: &str) -> Option<Self> {
        let fd = uds_connect(path);
        if fd < 0 {
            return None;
        }
        Some(Self { fd: fd as usize })
    }
    pub fn fd(&self) -> usize {
        self.fd
    }
}

pub struct ShmBuffer {
    pub handle: isize,
    pub va: isize,
    pub size: usize,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

impl ShmBuffer {
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let stride = (width as usize) * 4;
        let size = stride * (height as usize);
        let handle = shm_create(size);
        if handle <= 0 {
            return None;
        }
        let va = shm_map(
            handle as usize,
            mmap_flags::PROT_READ | mmap_flags::PROT_WRITE,
        );
        if va <= 0 {
            let _ = shm_close(handle as usize);
            return None;
        }
        Some(Self {
            handle,
            va,
            size,
            width,
            height,
            stride,
        })
    }
    pub fn ptr_mut(&self) -> *mut u8 {
        self.va as *mut u8
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        let _ = munmap(self.va as usize, self.size);
        let _ = shm_close(self.handle as usize);
    }
}

pub fn submit_buffer(conn: &LiteGuiConnection, buf: &ShmBuffer, dx: i32, dy: i32) {
    let payload = build_payload_buffer_commit(
        buf.handle as u32,
        buf.width,
        buf.height,
        buf.stride as u32,
        dx,
        dy,
    );
    let frame = encode_frame(MSG_BUFFER_COMMIT, &payload);
    let _ = write(conn.fd, &frame);
}
