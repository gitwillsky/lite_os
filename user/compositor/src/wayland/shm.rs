//! `wl_shm`：用 `pread` 读取客户端共享内存，而不是 mmap。
//!
//! 客户端可以在合成器读取时截断文件；mmap 会因此 SIGBUS，`pread` 只会短读，可转成协议错误，也不需要 unsafe。

use std::{
    cell::Cell,
    fs::File,
    os::{fd::OwnedFd, unix::fs::FileExt},
    rc::Rc,
};

use wayland_wire::protocol::wl_shm::Format;

/// 一个 `wl_shm_pool`：客户端文件与当前声明的大小。
pub struct Pool {
    file: File,
    size: Cell<usize>,
}

impl Pool {
    pub fn new(fd: OwnedFd, size: usize) -> Rc<Self> {
        Rc::new(Self {
            file: File::from(fd),
            size: Cell::new(size),
        })
    }

    pub fn size(&self) -> usize {
        self.size.get()
    }

    pub fn grow(&self, size: usize) {
        self.size.set(size);
    }
}

/// 一个 `wl_buffer` 的几何：位于某个 pool 内。
pub struct BufferSpec {
    pub pool: Rc<Pool>,
    pub offset: usize,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: Format,
}

/// 读取 buffer 内容失败：文件比声明的短（被客户端截断）。
#[derive(Debug, PartialEq, Eq)]
pub struct Truncated;

impl BufferSpec {
    /// 把整个 buffer 复制成预乘 `0xAARRGGBB` 像素；`Xrgb8888` 强制不透明。
    ///
    /// # Errors
    ///
    /// 文件内容短于声明（客户端截断了共享内存）。
    pub fn read_pixels(&self) -> Result<Vec<u32>, Truncated> {
        let mut pixels = Vec::with_capacity(self.width * self.height);
        let mut row = vec![0u8; self.width * 4];
        for y in 0..self.height {
            let at = (self.offset + y * self.stride) as u64;
            self.pool
                .file
                .read_exact_at(&mut row, at)
                .map_err(|_| Truncated)?;
            pixels.extend(row.as_chunks::<4>().0.iter().map(|chunk| {
                let value = u32::from_le_bytes(*chunk);
                if self.format == Format::Xrgb8888 {
                    value | 0xFF00_0000
                } else {
                    value
                }
            }));
        }
        Ok(pixels)
    }
}
