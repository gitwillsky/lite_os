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

/// 读取 buffer 内容失败：文件截断、无法分配，或 GPU 后端无法建立/上传纹理。
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Truncated,
    NoMemory,
    Upload,
}
use ReadError::{NoMemory, Truncated};

/// 单次 `pread` 的字节预算；大 stride 退化为单行，不分配整个客户端 pool。
const BATCH: usize = 64 * 1024;

impl BufferSpec {
    /// 逐行读取 `rect` 内的像素，转换成预乘 `0xAARRGGBB` 后交给 `row(y, pixels)`；`Xrgb8888` 强制不透明。
    ///
    /// # Parameters
    ///
    /// - `rect`: buffer 像素坐标，调用者保证非空且在 buffer 之内。
    /// - `row`: 每行回调一次，`y` 为 buffer 行号，`pixels` 从 `rect.x` 开始、长 `rect.width`。
    ///
    /// # Errors
    ///
    /// 文件内容短于声明（客户端截断了共享内存），或无法分配读缓冲。
    pub fn read_rect(
        &self,
        rect: canvas::Rect,
        mut row: impl FnMut(usize, &[u32]),
    ) -> Result<(), ReadError> {
        let (x, top) = (rect.x as usize, rect.y as usize);
        let (width, height) = (rect.width as usize, rect.height as usize);
        let row_bytes = width * 4;
        // 1. 窄矩形（如单个按钮）逐行读，避免把行间无关字节一起读入；
        // 2. 宽矩形在固定预算内多行一批读，减少 pread 次数；最后一行只读到矩形右边缘，
        //    不能要求客户端在 buffer 尾部额外提供 padding。
        let rows_per_read = if row_bytes * 2 < self.stride {
            1
        } else {
            (BATCH / self.stride).max(1).min(height)
        };
        let length = (rows_per_read - 1) * self.stride + row_bytes;
        let mut batch = Vec::new();
        batch.try_reserve_exact(length).map_err(|_| NoMemory)?;
        batch.resize(length, 0);
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(width).map_err(|_| NoMemory)?;
        for start in (0..height).step_by(rows_per_read) {
            let count = rows_per_read.min(height - start);
            let len = (count - 1) * self.stride + row_bytes;
            let at = (self.offset + (top + start) * self.stride + x * 4) as u64;
            self.pool
                .file
                .read_exact_at(&mut batch[..len], at)
                .map_err(|_| Truncated)?;
            for (index, line) in batch[..len].chunks(self.stride).enumerate() {
                pixels.clear();
                pixels.extend(line[..row_bytes].as_chunks::<4>().0.iter().map(|chunk| {
                    let value = u32::from_le_bytes(*chunk);
                    if self.format == Format::Xrgb8888 {
                        value | 0xFF00_0000
                    } else {
                        value
                    }
                }));
                row(top + start + index, &pixels);
            }
        }
        Ok(())
    }

    /// 整幅 buffer 的像素矩形。
    pub fn bounds(&self) -> canvas::Rect {
        canvas::Rect::new(0, 0, self.width as i32, self.height as i32)
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
            let read = |spec: &BufferSpec, rect: canvas::Rect| {
                let mut pixels = Vec::new();
                spec.read_rect(rect, |y, row| {
                    assert_eq!(y, rect.y as usize + pixels.len() / rect.width as usize);
                    pixels.extend_from_slice(row);
                })
                .map(|()| pixels)
            };
            assert_eq!(read(&spec, spec.bounds()).unwrap(), expected);
            // 损伤子矩形：列偏移与行号都按 buffer 坐标取，不能读成矩形左上角的偏移。
            let inner = canvas::Rect::new(1, 1, width as i32 - 2, height as i32 - 1);
            let columns = 1..width - 1;
            assert_eq!(
                read(&spec, inner).unwrap(),
                (1..height)
                    .flat_map(|y| expected[y * width..][columns.clone()].iter().copied())
                    .collect::<Vec<_>>()
            );
            spec.format = Format::Xrgb8888;
            assert_eq!(
                read(&spec, spec.bounds()).unwrap(),
                expected.iter().map(|v| v | 0xff000000).collect::<Vec<_>>()
            );
            let short = MemFd::create("shm-short", len - 1).unwrap();
            spec.pool = Pool::new(short.as_fd().try_clone_to_owned().unwrap(), len);
            assert_eq!(read(&spec, spec.bounds()), Err(Truncated));
        }
    }
}
