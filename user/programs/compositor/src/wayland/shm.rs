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

/// 读取 buffer 内容失败：文件截断或无法分配快照。
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Truncated,
    NoMemory,
}
use ReadError::Truncated;

impl BufferSpec {
    /// 把整个 buffer 复制成预乘 `0xAARRGGBB` 像素；`Xrgb8888` 强制不透明。
    ///
    /// # Errors
    ///
    /// 文件内容短于声明（客户端截断了共享内存），或无法分配快照。
    pub fn read_pixels(&self) -> Result<Vec<u32>, ReadError> {
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(self.width * self.height)
            .map_err(|_| ReadError::NoMemory)?;
        // 固定预算内批量 pread；大 stride 退化为单行，不分配整个客户端 pool。
        // 最后一批只读到最后一行的有效像素，不能要求客户端额外提供尾部 padding。
        let row_bytes = self.width * 4;
        let rows_per_read = (64 * 1024 / self.stride).max(1).min(self.height);
        let length = (rows_per_read - 1) * self.stride + row_bytes;
        let mut batch = Vec::new();
        batch
            .try_reserve_exact(length)
            .map_err(|_| ReadError::NoMemory)?;
        batch.resize(length, 0);
        for y in (0..self.height).step_by(rows_per_read) {
            let count = rows_per_read.min(self.height - y);
            let len = (count - 1) * self.stride + row_bytes;
            let at = (self.offset + y * self.stride) as u64;
            self.pool
                .file
                .read_exact_at(&mut batch[..len], at)
                .map_err(|_| Truncated)?;
            for row in batch[..len].chunks(self.stride) {
                pixels.extend(row[..row_bytes].as_chunks::<4>().0.iter().map(|chunk| {
                    let value = u32::from_le_bytes(*chunk);
                    if self.format == Format::Xrgb8888 {
                        value | 0xFF00_0000
                    } else {
                        value
                    }
                }));
            }
        }
        Ok(pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_uapi::shared_memory::MemFd;
    use std::{io::Write, os::fd::AsFd};

    #[test]
    fn batched_reads_preserve_rows_padding_offset_and_truncation_errors() {
        for (width, height, stride, offset) in
            [(3, 4, 20, 16), (300, 200, 1232, 36), (3, 2, 70000, 0)]
        {
            let len = offset + (height - 1) * stride + width * 4;
            let mut bytes = vec![0xa5; len];
            let expected: Vec<u32> = (0..width * height).map(|i| 0x12300000 | i as u32).collect();
            for y in 0..height {
                for x in 0..width {
                    let start = offset + y * stride + x * 4;
                    bytes[start..start + 4].copy_from_slice(&expected[y * width + x].to_le_bytes());
                }
            }
            let memory = MemFd::create("shm-read", len).unwrap();
            File::from(memory.as_fd().try_clone_to_owned().unwrap())
                .write_all(&bytes)
                .unwrap();
            let mut spec = BufferSpec {
                pool: Pool::new(memory.as_fd().try_clone_to_owned().unwrap(), len),
                offset,
                width,
                height,
                stride,
                format: Format::Argb8888,
            };
            assert_eq!(spec.read_pixels().unwrap(), expected);
            spec.format = Format::Xrgb8888;
            assert_eq!(
                spec.read_pixels().unwrap(),
                expected.iter().map(|v| v | 0xff000000).collect::<Vec<_>>()
            );
            let short = MemFd::create("shm-short", len - 1).unwrap();
            spec.pool = Pool::new(short.as_fd().try_clone_to_owned().unwrap(), len);
            assert_eq!(spec.read_pixels(), Err(Truncated));
        }
    }
}
