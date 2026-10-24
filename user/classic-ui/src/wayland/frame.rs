use super::{Display, WindowId};
use linux_uapi::shared_memory::MemFd;
use std::{
    fs::File,
    io::{self, Write},
    os::fd::AsFd,
};
use wayland_wire::{
    protocol::{Interface, wl_shm, wl_shm_pool, wl_surface},
    wire::Writer,
};

impl Display {
    /// 提交与当前配置匹配的预乘 ARGB 帧，像素按物理分辨率逐行排列。
    ///
    /// 每帧使用独立 memfd；提交后不再修改，收到 wl_buffer.release 才销毁客户端 buffer。
    /// 一次只允许一个未完成 frame callback，应用在 FramePresented 后提交下一帧。
    ///
    /// # Errors
    ///
    /// `WouldBlock` 表示上一帧未完成；未 configure、像素数/尺寸不符、超过 shm i32 extent、分配、编码或 I/O 失败。
    pub fn present(&mut self, window: WindowId, pixels: &[u32]) -> io::Result<()> {
        let state = self
            .windows
            .get(&window)
            .filter(|window| window.configured)
            .ok_or_else(|| io::Error::other("window has not been configured"))?;
        if state.frame.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let configuration = state.configuration;
        let (width, height) = configuration.pixel_size()?;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| io::Error::other("shm stride overflow"))?;
        let bytes = stride
            .checked_mul(height)
            .ok_or_else(|| io::Error::other("shm extent overflow"))?;
        if pixels.len() != bytes as usize / 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pixel count does not match window configuration",
            ));
        }
        let memory = MemFd::create("classic-ui", bytes as usize)?;
        let mut file = File::from(memory.as_fd().try_clone_to_owned()?);
        // 分块编码固定 scratch；避免再为整幅窗口分配第二个字节数组。
        let mut scratch = [0u8; 4096];
        for chunk in pixels.chunks(scratch.len() / 4) {
            for (destination, pixel) in scratch.as_chunks_mut::<4>().0.iter_mut().zip(chunk) {
                destination.copy_from_slice(&pixel.to_ne_bytes());
            }
            file.write_all(&scratch[..chunk.len() * 4])?;
        }
        let pool = self.transport.allocate(Interface::WlShmPool)?;
        let buffer = self.transport.allocate(Interface::WlBuffer)?;
        let callback = self.transport.allocate(Interface::WlCallback)?;
        let mut pool_request = Writer::new();
        pool_request.begin(self.shm, 0);
        pool_request.object(Some(pool));
        pool_request.int(bytes);
        pool_request.end().map_err(io::Error::other)?;
        self.transport
            .send_fd(pool_request.bytes(), memory.as_fd())?;
        let writer = &mut self.transport.writer;
        wl_shm_pool::Request::CreateBuffer {
            id: buffer,
            offset: 0,
            width,
            height,
            stride,
            format: wl_shm::Format::Argb8888,
        }
        .encode(pool, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::SetBufferScale {
            scale: configuration.scale,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Attach {
            buffer: Some(buffer),
            x: 0,
            y: 0,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Damage {
            x: 0,
            y: 0,
            width: configuration.width,
            height: configuration.height,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Frame { callback }
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        wl_surface::Request::Commit
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        wl_shm_pool::Request::Destroy
            .encode(pool, writer)
            .map_err(io::Error::other)?;
        self.windows
            .get_mut(&window)
            .expect("validated above")
            .frame = Some(callback);
        self.buffers.insert(buffer, memory);
        self.transport.flush()
    }
}
