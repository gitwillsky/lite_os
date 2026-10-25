//! wl_shm 缓冲池：Display 独占内存与 busy 状态，release 之前绝不重写。

use super::{Display, WindowId};
use linux_uapi::shared_memory::MemFd;
use std::{io, os::fd::AsFd};
use wayland_wire::{
    protocol::{Interface, wl_buffer, wl_shm, wl_shm_pool, wl_surface},
    wire::{ObjectId, Writer},
};

/// 三个在途缓冲覆盖生产者/消费者交错，同时限制慢消费者占用的内存。
const WINDOW_BUFFER_LIMIT: usize = 3;

pub(super) struct Buffer {
    pub window: WindowId,
    pub size: (i32, i32),
    pub busy: bool,
    memory: MemFd,
}

impl Display {
    pub(super) fn buffer_available(&self, window: WindowId) -> bool {
        let mut count = 0;
        for buffer in self
            .buffers
            .values()
            .filter(|buffer| buffer.window == window)
        {
            if !buffer.busy {
                return true;
            }
            count += 1;
        }
        count < WINDOW_BUFFER_LIMIT
    }

    /// 提交完整预乘 ARGB 帧；尺寸来自当前 configure，缓冲复用必须等待 release。
    ///
    /// # Errors
    /// 在途 callback/缓冲池满返回 WouldBlock；配置、尺寸、分配、编码与 I/O 失败保留错误。
    /// 连接错误后调用方应销毁 Display，不能继续使用部分发送的请求。
    pub fn present(&mut self, window: WindowId, pixels: &[u32]) -> io::Result<()> {
        let state = self
            .windows
            .get(&window)
            .filter(|state| state.configured)
            .ok_or_else(|| io::Error::other("window has not been configured"))?;
        if state.frame.is_some() || !self.buffer_available(window) {
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
        // 1. 淘汰不匹配尺寸的空闲缓冲；仍 busy 的旧尺寸保持 owner，直到 release。
        let retired: Vec<_> = self
            .buffers
            .iter()
            .filter_map(|(id, buffer)| {
                (buffer.window == window && !buffer.busy && buffer.size != (width, height))
                    .then_some(*id)
            })
            .collect();
        for id in retired {
            self.buffers.remove(&id);
            wl_buffer::Request::Destroy
                .encode(id, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        }
        let reusable = self
            .buffers
            .iter()
            .find_map(|(id, buffer)| (buffer.window == window && !buffer.busy).then_some(*id));
        let id = match reusable {
            Some(id) => id,
            None => self.allocate_buffer(window, (width, height), stride, bytes)?,
        };
        // 2. 每次覆盖完整内容，不依赖某个 back buffer 的历史。
        self.buffers
            .get_mut(&id)
            .expect("allocated buffer")
            .memory
            .write_words(pixels)?;
        let callback = self.transport.allocate(Interface::WlCallback)?;
        let writer = &mut self.transport.writer;
        wl_surface::Request::SetBufferScale {
            scale: configuration.scale,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Attach {
            buffer: Some(id),
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
        // 3. flush 前冻结 busy，错误后关闭连接，不能重写可能已交付的内存。
        self.buffers.get_mut(&id).expect("allocated buffer").busy = true;
        self.windows
            .get_mut(&window)
            .expect("validated window")
            .frame = Some(callback);
        self.transport.flush()?;
        self.windows
            .get_mut(&window)
            .expect("committed window")
            .mapped = true;
        let children: Vec<_> = self
            .pending_parents
            .iter()
            .filter_map(|(child, parent)| (*parent == window).then_some(*child))
            .collect();
        for child in children {
            self.set_parent(child, window)?;
        }
        Ok(())
    }

    fn allocate_buffer(
        &mut self,
        window: WindowId,
        size: (i32, i32),
        stride: i32,
        bytes: i32,
    ) -> io::Result<ObjectId> {
        let memory = MemFd::create("classic-ui", bytes as usize)?;
        let pool = self.transport.allocate(Interface::WlShmPool)?;
        let id = self.transport.allocate(Interface::WlBuffer)?;
        let mut request = Writer::new();
        request.begin(self.shm, 0);
        request.object(Some(pool));
        request.int(bytes);
        request.end().map_err(io::Error::other)?;
        self.transport.send_fd(request.bytes(), memory.as_fd())?;
        wl_shm_pool::Request::CreateBuffer {
            id,
            offset: 0,
            width: size.0,
            height: size.1,
            stride,
            format: wl_shm::Format::Argb8888,
        }
        .encode(pool, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        wl_shm_pool::Request::Destroy
            .encode(pool, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.buffers.insert(
            id,
            Buffer {
                window,
                size,
                busy: false,
                memory,
            },
        );
        Ok(id)
    }

    pub(super) fn release_buffer(&mut self, object: ObjectId) -> io::Result<()> {
        let Some(buffer) = self.buffers.get_mut(&object) else {
            return Ok(());
        };
        let window = buffer.window;
        if self.windows.contains_key(&window) {
            buffer.busy = false;
            self.events.push_back(super::Event {
                window,
                kind: super::EventKind::BufferAvailable,
            });
        } else {
            self.buffers.remove(&object);
            wl_buffer::Request::Destroy
                .encode(object, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        }
        Ok(())
    }

    pub(super) fn retire_idle_buffers(&mut self, window: WindowId) -> io::Result<()> {
        let ids: Vec<_> = self
            .buffers
            .iter()
            .filter_map(|(id, buffer)| (buffer.window == window && !buffer.busy).then_some(*id))
            .collect();
        for id in ids {
            self.buffers.remove(&id);
            wl_buffer::Request::Destroy
                .encode(id, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        }
        Ok(())
    }
}
