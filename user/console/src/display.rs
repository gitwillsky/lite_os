//! 文本控制台对显示器的占用：DRM master + 一块 dumb buffer 作为 scanout。
//!
//! 持有关系由 seatd 的 `Acquire`/`Release` 驱动：构造即占用，`Drop` 即交还。内核在 DRM fd 关闭时
//! 禁用 scanout 并清除 master（`kernel/src/drm/device.rs` 的 `DrmFile::drop`），所以这里不需要
//! 显式 `drop_master`；缺少 `Drop` 的 fd 关闭，下一个 session 会拿不到 master（`EBUSY`）。

use std::{io, thread, time::Duration};

use linux_uapi::drm::{Clip, DrmDevice, DumbBuffer, Topology};

use crate::frame::{Layout, Pixels};

/// 与 compositor 的 `take_master` 同样的重试预算：对端关闭 fd 后 master 才释放，可能有短暂窗口。
const MASTER_ATTEMPTS: u32 = 50;
const MASTER_RETRY: Duration = Duration::from_millis(100);
const EBUSY: i32 = 16;
/// XRGB8888 的有效位深。
const DEPTH: u32 = 24;
const BACKGROUND: u32 = 0x0010_1418;

pub struct Display {
    device: DrmDevice,
    framebuffer_id: u32,
    buffer: DumbBuffer,
    layout: Layout,
}

impl Pixels for DumbBuffer {
    fn row_mut(&mut self, y: usize) -> &mut [u32] {
        DumbBuffer::row_mut(self, y)
    }
}

impl Display {
    /// 取得 master 并把整屏刷成背景色；之后由 [`Display::buffer_mut`] 绘制、[`Display::flush`] 上传。
    pub fn open() -> io::Result<Self> {
        let device = DrmDevice::open("/dev/dri/card0")?;
        let topology = device.query_topology()?;
        take_master(&device)?;
        let width = usize::from(topology.mode.width());
        let height = usize::from(topology.mode.height());
        let layout = Layout::fit(width, height).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "display is smaller than one cell",
            )
        })?;
        let mut buffer = device.create_dumb(width as u32, height as u32)?;
        for y in 0..height {
            buffer.row_mut(y).fill(BACKGROUND);
        }
        let framebuffer_id = device.add_framebuffer(&buffer, DEPTH)?;
        set_scanout(&device, &topology, framebuffer_id)?;
        Ok(Self {
            device,
            framebuffer_id,
            buffer,
            layout,
        })
    }

    /// 平台是否发布了可用的 KMS 拓扑；`make run` 这类无 GPU 启动返回 false。
    pub fn available() -> bool {
        DrmDevice::open("/dev/dri/card0")
            .and_then(|device| device.query_topology())
            .is_ok()
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn buffer_mut(&mut self) -> &mut DumbBuffer {
        &mut self.buffer
    }

    /// 把 `[first, last]` 行字符所在的像素带上传给 host；空范围表示整屏。
    pub fn flush(&self, rows: Option<(usize, usize)>) -> io::Result<()> {
        let Some((first, last)) = rows else {
            return self.device.dirty(self.framebuffer_id, &[]);
        };
        let (x1, x2) = self.layout.column_span();
        let (y1, _) = self.layout.row_span(first);
        let (_, y2) = self.layout.row_span(last);
        self.device.dirty(
            self.framebuffer_id,
            &[Clip {
                x1: x1 as u16,
                y1: y1 as u16,
                x2: x2 as u16,
                y2: y2 as u16,
            }],
        )
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.device.remove_framebuffer(self.framebuffer_id);
    }
}

fn set_scanout(device: &DrmDevice, topology: &Topology, framebuffer_id: u32) -> io::Result<()> {
    device.set_crtc(topology, framebuffer_id)?;
    device.dirty(framebuffer_id, &[])
}

fn take_master(device: &DrmDevice) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match device.set_master() {
            Ok(()) => return Ok(()),
            Err(error) if error.raw_os_error() == Some(EBUSY) && attempts < MASTER_ATTEMPTS => {
                attempts += 1;
                thread::sleep(MASTER_RETRY);
            }
            Err(error) => return Err(error),
        }
    }
}
