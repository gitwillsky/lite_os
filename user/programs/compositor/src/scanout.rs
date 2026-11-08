//! 唯一的 DRM master：选中的 CPU/GPU 后端独占一对扫描目标与 page flip。

use std::io;
mod kms;

use linux_uapi::drm::{Clip, DrmDevice, DumbBuffer, Topology, VirglResource};

struct Target {
    buffer: DumbBuffer,
    framebuffer: u32,
    device: DrmDevice,
}
impl Drop for Target {
    fn drop(&mut self) {
        // 构造失败与正常退出都先移除 KMS 引用，再由字段析构释放 GEM。
        let _ = self.device.remove_framebuffer(self.framebuffer);
    }
}
#[expect(
    clippy::large_enum_variant,
    reason = "Scanout 一生只持有一个后端；为单个对象节约 208 字节而新增堆分配和间接访问没有收益"
)]
enum Backend {
    Cpu([Target; 2]),
    Gpu(crate::compose::gpu::Gpu),
}
impl Backend {
    fn framebuffer(&self, back: usize) -> u32 {
        match self {
            Self::Cpu(targets) => targets[back].framebuffer,
            Self::Gpu(gpu) => gpu.framebuffer(back),
        }
    }
}

/// 单一 connector/CRTC 的显示输出。
///
/// 画面总是先画进 `back`；`show_first` 把它交给 CRTC 并交换前后缓冲。
pub struct Scanout {
    device: DrmDevice,
    topology: Topology,
    kms: kms::Output,
    backend: Backend,
    back: usize,
    /// 在途 flip 的序号；同一时刻至多一个 flip，缺少这条约束会让第二次 `PAGE_FLIP` 被内核以 `EBUSY` 拒绝。
    pending: Option<u64>,
    sequence: u64,
    // OWNER: 与同一对扫描目标同寿命，只存几何/损伤，不保留客户端像素。
    history: crate::compose::damage::History,
    // OWNER: 每个扫描目标是否已画过完整的启动画面静态位图。启动动画每帧只重画动画带与进度条，
    // 缺少它，第一次用到的后缓冲只会得到这两块而其余保持黑屏。
    boot_ready: [bool; 2],
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
        let kms = kms::Output::new(&device)?;
        let (width, height) = (
            u32::from(topology.mode.width()),
            u32::from(topology.mode.height()),
        );
        // 1. 先协商能力，再分配唯一后端。Unsupported 之外的错误必须向上传播。
        // 2. GPU 的启动画面通过一块临时 backing 写入同一对 host 扫描目标。
        let backend = match device.validate_virgl2() {
            Ok(()) => Backend::Gpu(crate::compose::gpu::Gpu::new(&device, width, height)?),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                let target = || -> io::Result<Target> {
                    let buffer = device.create_dumb(width, height)?;
                    let framebuffer = device.add_framebuffer(&buffer, 24)?;
                    Ok(Target {
                        buffer,
                        framebuffer,
                        device: device.clone(),
                    })
                };
                Backend::Cpu([target()?, target()?])
            }
            Err(error) => return Err(error),
        };
        println!(
            "compositor: composition backend={}",
            match &backend {
                Backend::Cpu(_) => "CPU",
                Backend::Gpu(_) => "VirGL2",
            }
        );
        Ok(Self {
            device,
            topology,
            kms,
            backend,
            back: 0,
            pending: None,
            sequence: 0,
            history: crate::compose::damage::History::default(),
            boot_ready: [false; 2],
        })
    }

    /// 当前 GPU context 的共享引用；CPU 后端不提供 dma-buf 导入能力。
    pub fn gpu_context(&self) -> Option<linux_uapi::drm::VirglContext> {
        match &self.backend {
            Backend::Gpu(gpu) => Some(gpu.context()),
            Backend::Cpu(_) => None,
        }
    }

    pub fn width(&self) -> usize {
        usize::from(self.topology.mode.width())
    }

    pub fn height(&self) -> usize {
        usize::from(self.topology.mode.height())
    }

    /// 由实际 KMS timing 得到预算周期；无时钟的 mode 不能申请截止保证。
    pub(super) fn frame_period_ns(&self) -> io::Result<u64> {
        self.topology
            .mode
            .frame_period_ns()
            .ok_or_else(|| io::ErrorKind::InvalidData.into())
    }

    /// Append a validated rectangle from the last completed front buffer as BGRX.
    pub(super) fn capture(
        &self,
        rect: [u32; 4],
        staging: &mut Option<VirglResource>,
        output: &mut Vec<u8>,
    ) -> io::Result<()> {
        let front = self.back ^ 1;
        match &self.backend {
            Backend::Gpu(gpu) => gpu.capture(front, rect, staging, output),
            Backend::Cpu(targets) => {
                let [x, y, width, height] = rect.map(|value| value as usize);
                for row in y..y + height {
                    for &pixel in &targets[front].buffer.row(row)[x..x + width] {
                        output.extend_from_slice(&pixel.to_le_bytes());
                    }
                }
                Ok(())
            }
        }
    }

    /// 在可写后缓冲绘制启动画面；flip 在途返回 WouldBlock，画面或设备错误直接传播。
    pub fn paint_boot(
        &mut self,
        scene: &crate::boot::Scene,
        frame: crate::boot::Frame,
    ) -> io::Result<()> {
        if self.pending.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.history.invalidate();
        let full = !self.boot_ready[self.back];
        let damage = match &mut self.backend {
            Backend::Cpu(targets) => {
                crate::boot::paint(&mut targets[self.back].buffer, scene, frame, full)
                    .map_err(io::Error::other)?
            }
            Backend::Gpu(gpu) => gpu.paint_boot(self.back, scene, frame, full)?,
        };
        self.boot_ready[self.back] = true;
        let damage: Vec<_> = damage
            .iter()
            .map(|rect| {
                canvas::Rect::new(
                    rect.x as i32,
                    rect.y as i32,
                    rect.width as i32,
                    rect.height as i32,
                )
            })
            .collect();
        self.kms.damage(&damage);
        Ok(())
    }

    /// 首次把后缓冲设为 CRTC 的扫描对象（modeset），并交换前后缓冲。
    ///
    /// # Errors
    ///
    /// 内核拒绝 modeset。
    pub fn show_first(&mut self) -> io::Result<()> {
        self.kms.configure(
            &self.device,
            &self.topology,
            self.backend.framebuffer(self.back),
        )?;
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
        self.kms.present(
            &self.device,
            self.backend.framebuffer(self.back),
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

    /// 按设备能力选择一次后端；CPU/GPU 消费同一标准 Wayland 内容、几何与叠加顺序。
    /// `overlay` 收到绘制面及其相对输出的物理原点。CPU 原点为零，GPU 使用叠加边界的左上角。
    /// 已有 flip 时拒绝覆盖 back；渲染/提交失败直接返回错误。
    pub fn compose(
        &mut self,
        theme: &classic_theme::Theme,
        windows: &[crate::compose::Drawable<'_>],
        overlay_bounds: Option<canvas::Rect>,
        overlay: impl FnMut(canvas::Surface<'_>, (i32, i32)),
    ) -> io::Result<()> {
        if self.pending.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let frame = self.history.prepare(
            self.back,
            canvas::Rect::new(0, 0, self.width() as i32, self.height() as i32),
            theme.color(classic_theme::SystemColor::Background),
            windows,
            overlay_bounds,
        );
        match &mut self.backend {
            Backend::Gpu(gpu) => {
                gpu.render(self.back, theme, windows, &frame, overlay_bounds, overlay)?
            }
            Backend::Cpu(targets) => {
                if !crate::compose::compose(
                    &mut targets[self.back].buffer,
                    theme,
                    windows,
                    &frame,
                    overlay,
                )? {
                    return Err(io::Error::other("invalid scanout drawing surface"));
                }
                // 先同步该后缓冲的全部修复区，再翻页。只传相对前台的 damage 会遗漏更旧目标的变化；
                // DIRTYFB 完成后 adapter 的单次同步凭据让随后 page flip 不再整幅 TRANSFER。
                sync_cpu_target(
                    &self.device,
                    targets[self.back].framebuffer,
                    frame.repair_rects(),
                )?;
            }
        }
        self.kms.damage(frame.output_rects());
        self.history.record(self.back, frame);
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

    /// 隐藏硬件光标；之后 [`Scanout::show_cursor`] 再次显示。
    pub fn hide_cursor(&self, position: (i32, i32)) -> io::Result<()> {
        self.device
            .update_cursor(&self.topology, None, position, (0, 0))
    }

    /// 只移动硬件光标，不改变图像。
    pub fn move_cursor(&self, position: (i32, i32)) -> io::Result<()> {
        self.device.move_cursor(&self.topology, position)
    }
}

/// 标准 DIRTYFB 的 32-clip 预算；片段超限只扩大同步矩形，不改变光栅范围或提交两套状态。
fn sync_cpu_target(
    device: &DrmDevice,
    framebuffer: u32,
    rectangles: &[canvas::Rect],
) -> io::Result<()> {
    const CLIPS: usize = 32;
    if rectangles.is_empty() {
        return Ok(());
    }
    let mut clips = [Clip {
        x1: 0,
        y1: 0,
        x2: 0,
        y2: 0,
    }; CLIPS];
    let convert = |rect: canvas::Rect| -> io::Result<Clip> {
        Ok(Clip {
            x1: u16::try_from(rect.x).map_err(io::Error::other)?,
            y1: u16::try_from(rect.y).map_err(io::Error::other)?,
            x2: u16::try_from(rect.right()).map_err(io::Error::other)?,
            y2: u16::try_from(rect.bottom()).map_err(io::Error::other)?,
        })
    };
    let count = if rectangles.len() > CLIPS {
        let bounds = rectangles
            .iter()
            .copied()
            .reduce(|a, b| {
                let (x, y) = (a.x.min(b.x), a.y.min(b.y));
                canvas::Rect::new(
                    x,
                    y,
                    a.right().max(b.right()) - x,
                    a.bottom().max(b.bottom()) - y,
                )
            })
            .expect("nonempty repair region");
        clips[0] = convert(bounds)?;
        1
    } else {
        for (clip, rect) in clips.iter_mut().zip(rectangles) {
            *clip = convert(*rect)?;
        }
        rectangles.len()
    };
    device.dirty(framebuffer, &clips[..count])
}
