//! `wl_shm` 池持有只读共享映射。按损伤直接复制到调用方拥有的像素存储。

use linux_uapi::shared_memory::{ReadOnlySharedMapping, is_immutable, is_shrink_sealed};
use std::{
    cell::{OnceCell, RefCell},
    fs::File,
    io,
    os::fd::{AsFd, OwnedFd},
    rc::Rc,
};
use wayland_wire::protocol::wl_shm::Format;

/// 一个 `wl_shm_pool` 的文件和映射，最后一个 buffer 释放时一同退休。
pub struct Pool {
    file: File,
    mapping: RefCell<ReadOnlySharedMapping>,
    // 封印不可撤销。仅在实际禁止缩小时免除每次读之前的 EOF 检查；SIGBUS 仍可能来自 backing 故障。
    shrink_sealed: bool,
    // 写封印与尺寸封印同时成立时，可跨 surface 复用同一像素快照。
    immutable: bool,
}

impl Pool {
    /// 建立整个声明区间的只读映射。文件过短、无法 mmap 或无法安装恢复处理时返回 I/O 错误。
    pub fn new(fd: OwnedFd, size: usize) -> io::Result<Rc<Self>> {
        let file = File::from(fd);
        let immutable = is_immutable(file.as_fd());
        let shrink_sealed = immutable || is_shrink_sealed(file.as_fd());
        let mapping = Self::map(&file, size)?;
        Ok(Rc::new(Self {
            file,
            mapping: RefCell::new(mapping),
            shrink_sealed,
            immutable,
        }))
    }

    fn map(file: &File, size: usize) -> io::Result<ReadOnlySharedMapping> {
        if file.metadata()?.len() < size as u64 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        ReadOnlySharedMapping::map(file.as_fd(), size)
    }

    pub(super) fn immutable(&self) -> bool {
        self.immutable
    }

    pub fn size(&self) -> usize {
        self.mapping.borrow().len()
    }

    /// 按客户端提供的更大 extent 重建视图。准备失败时保留原有映射。
    pub fn grow(&self, size: usize) -> io::Result<()> {
        assert!(size >= self.size(), "wl_shm pool cannot shrink");
        if size > self.size() {
            *self.mapping.borrow_mut() = Self::map(&self.file, size)?;
        }
        Ok(())
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
    // OWNER: 一个实际 wl_buffer 的不可变 SHM 快照；只在本连接线程填充，mutable SHM 永远不使用。
    // Content 的采样元数据按 surface 复制，像素/纹理由 Rc 共享；buffer 销毁后由活动 surface 接力保活。
    pub(super) snapshot: OnceCell<Rc<super::surface::Content>>,
}

/// 读取 buffer 内容失败：文件截断、无法分配，或 GPU 后端无法建立/上传纹理。
#[derive(Debug)]
pub enum ReadError {
    Truncated,
    NoMemory,
    Upload(std::io::Error),
}
use ReadError::Truncated;

impl From<std::io::Error> for ReadError {
    fn from(error: std::io::Error) -> Self {
        Self::Upload(error)
    }
}

impl BufferSpec {
    /// 将 `rect` 从池复制到调用方提供的像素存储。只写每行有效像素。
    ///
    /// XRGB 补上不透明 alpha。共享文件过短或访问触发 SIGBUS 时返回 Truncated，输出可能已部分更新。
    pub(super) fn copy_pixels(
        &self,
        rect: canvas::Rect,
        output: &mut [u32],
        stride: usize,
    ) -> Result<(), ReadError> {
        assert!(!rect.is_empty() && rect.intersect(self.bounds()) == rect);
        let (x, top) = (rect.x as usize, rect.y as usize);
        let (width, height) = (rect.width as usize, rect.height as usize);
        assert!(stride >= width && (height - 1) * stride + width <= output.len());
        let offset = self.offset + top * self.stride + x * 4;
        let end = offset + (height - 1) * self.stride + width * 4;
        let mapping = self.pool.mapping.borrow();
        assert!(end <= mapping.len(), "validated buffer is inside its pool");
        // mmap 对 EOF 同一页内的空尾只返回零。未封印的文件需要主动检查字节上限，
        // 检查后的并发截断或其他同步 backing fault 则由同一段 C 访问保护。
        if !self.pool.shrink_sealed
            && self.pool.file.metadata().map_err(|_| Truncated)?.len() < end as u64
        {
            return Err(Truncated);
        }
        let alpha = if self.format == Format::Xrgb8888 {
            0xff00_0000
        } else {
            0
        };
        mapping
            .copy_words(offset, self.stride, output, stride, [width, height], alpha)
            .map_err(|_| Truncated)
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
    fn mapped_reads_preserve_rows_padding_offset_and_truncation_errors() {
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
                pool: Pool::new(memory.as_fd().try_clone_to_owned().unwrap(), len).unwrap(),
                offset,
                width,
                height,
                stride,
                format: Format::Argb8888,
                snapshot: OnceCell::new(),
            };
            let read = |spec: &BufferSpec, rect: canvas::Rect| {
                let mut pixels = vec![0; (rect.width * rect.height) as usize];
                spec.copy_pixels(rect, &mut pixels, rect.width as usize)
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
            assert_eq!(
                Pool::new(short.as_fd().try_clone_to_owned().unwrap(), len)
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::UnexpectedEof,
            );
        }
    }
}
