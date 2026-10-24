//! 唯一的 DRM master：两块 XRGB8888 dumb buffer 的双缓冲与 page flip。

use std::io;

use linux_uapi::drm::{DrmDevice, DumbBuffer, Topology};

struct Target {
    buffer: DumbBuffer,
    framebuffer: u32,
}

/// 单一 connector/CRTC 的显示输出。
///
/// 画面总是先画进 `back`；`show_first` 把它交给 CRTC 并交换前后缓冲。
pub struct Scanout {
    device: DrmDevice,
    topology: Topology,
    targets: [Target; 2],
    back: usize,
    /// 在途 flip 的序号；同一时刻至多一个 flip，缺少这条约束会让第二次 `PAGE_FLIP` 被内核以 `EBUSY` 拒绝。
    pending: Option<u64>,
    sequence: u64,
}

impl Scanout {
    /// 打开 `path`，成为 DRM master，按 connector 的首选模式建立双缓冲并分配 framebuffer。
    ///
    /// # Errors
    ///
    /// 设备不存在、已有另一个 master、没有可用 connector，或 dumb buffer/framebuffer 创建失败。
    pub fn open(path: &str) -> io::Result<Self> {
        let device = DrmDevice::open(path)?;
        device.set_master()?;
        let topology = device.query_topology()?;
        let (width, height) = (
            u32::from(topology.mode.width()),
            u32::from(topology.mode.height()),
        );
        let mut targets = Vec::with_capacity(2);
        for _ in 0..2 {
            let buffer = device.create_dumb(width, height)?;
            let framebuffer = device.add_framebuffer(&buffer, 24)?;
            targets.push(Target {
                buffer,
                framebuffer,
            });
        }
        let targets: [Target; 2] = targets
            .try_into()
            .map_err(|_| io::Error::other("two targets"))?;
        Ok(Self {
            device,
            topology,
            targets,
            back: 0,
            pending: None,
            sequence: 0,
        })
    }

    pub fn width(&self) -> usize {
        self.targets[0].buffer.width()
    }

    pub fn height(&self) -> usize {
        self.targets[0].buffer.height()
    }

    /// 当前可绘制的后缓冲。
    pub fn back(&mut self) -> &mut DumbBuffer {
        &mut self.targets[self.back].buffer
    }

    /// 首次把后缓冲设为 CRTC 的扫描对象（modeset），并交换前后缓冲。
    ///
    /// # Errors
    ///
    /// 内核拒绝 modeset。
    pub fn show_first(&mut self) -> io::Result<()> {
        self.device
            .set_crtc(&self.topology, self.targets[self.back].framebuffer)?;
        self.back ^= 1;
        Ok(())
    }

    /// 是否有 flip 尚未完成。
    pub fn flip_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// 异步把后缓冲交给 CRTC；完成由 DRM fd 变为可读通知，随后调用 [`Scanout::complete_flip`]。
    ///
    /// # Errors
    ///
    /// 已有 flip 在途，或内核拒绝。
    pub fn request_flip(&mut self) -> io::Result<()> {
        if self.pending.is_some() {
            return Err(io::Error::other("a page flip is already pending"));
        }
        self.sequence += 1;
        self.device.page_flip(
            &self.topology,
            self.targets[self.back].framebuffer,
            self.sequence,
        )?;
        self.pending = Some(self.sequence);
        Ok(())
    }

    /// 读出在途 flip 的完成事件并交换前后缓冲。
    ///
    /// # Errors
    ///
    /// 没有在途 flip、事件读取失败或序号不符。
    pub fn complete_flip(&mut self) -> io::Result<()> {
        let expected = self
            .pending
            .ok_or_else(|| io::Error::other("no page flip is pending"))?;
        let event = self.device.read_flip_event()?;
        if event.user_data != expected {
            return Err(io::Error::other(
                "flip completion for an unexpected sequence",
            ));
        }
        self.pending = None;
        self.back ^= 1;
        Ok(())
    }

    pub fn device(&self) -> &DrmDevice {
        &self.device
    }
}

impl Scanout {
    /// 显示或更新硬件光标：`position` 是热点在屏幕上的位置。
    ///
    /// # Errors
    ///
    /// 缓冲不是 64 × 64，或光标队列失败。
    pub fn show_cursor(
        &self,
        image: &DumbBuffer,
        position: (i32, i32),
        hotspot: (u32, u32),
    ) -> io::Result<()> {
        self.device
            .update_cursor(&self.topology, Some(image), position, hotspot)
    }

    /// 只移动硬件光标，不改变图像。
    pub fn move_cursor(&self, position: (i32, i32)) -> io::Result<()> {
        self.device.move_cursor(&self.topology, position)
    }
}
