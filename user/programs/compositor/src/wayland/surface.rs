//! `wl_surface` 的双缓冲状态：请求先写入 pending，`commit` 时一次性生效。

use canvas::ImageSource;
use std::{cell::OnceCell, rc::Rc};
use wayland_wire::wire::ObjectId;

/// 已提交像素：CPU 后端保存在内存，GPU 后端只在纹理中；视口改动只复制采样元数据。
// OWNER: 纹理随像素由同一 Rc 共享；视口/同步提交的快照不会重复上传同一内容。
// 在途帧仍由 GPU 采样视图引用保活；缺少共享 owner 会使缓存提交复制全窗口像素。
enum Pixels {
    /// CPU 后端的 SHM 内容。
    Memory(Vec<u32>),
    /// GPU 后端的 SHM 内容：像素只在纹理中，不再保留第二份 CPU 副本。
    /// `readback` 是 CPU 消费者（硬件光标）的按需拷贝；原地更新后必须清空，否则读到旧像素。
    Texture {
        texture: Rc<linux_uapi::drm::VirglResource>,
        readback: OnceCell<Vec<u32>>,
    },
    Dma {
        usage: Rc<super::dmabuf::BufferUse>,
        texture: Rc<linux_uapi::drm::VirglResource>,
        readback: OnceCell<Vec<u32>>,
    },
}
/// wl_buffer 的真实来源；两种标准缓冲共用 attach/commit，不建立第二条 surface 状态机。
pub(super) enum Buffer {
    Shm(super::shm::BufferSpec),
    Dma(super::dmabuf::DmaBuffer),
}

/// 已提交内容的像素引用与 surface 采样投影；Clone 不复制像素或建立第二份纹理。
///
/// 像素是写时复制：只有唯一持有者（surface 自身）才会被下一次 SHM 提交原地更新；
/// 任何其他持有者（同步子 surface 的缓存、在用光标）都让下一次提交另建快照，
/// 因此持有者看到的始终是它取得时的内容。
#[derive(Clone)]
pub struct Content {
    pub width: usize,
    pub height: usize,
    pixels: Rc<Pixels>,
    pub size: (i32, i32),
    pub source: ImageSource,
    pub offset: (i64, i64),
}
impl Content {
    #[cfg(test)]
    pub(crate) fn new(width: usize, height: usize, pixels: Vec<u32>) -> Self {
        Self::with(width, height, Pixels::Memory(pixels))
    }
    fn with(width: usize, height: usize, pixels: Pixels) -> Self {
        Self {
            width,
            height,
            size: (width as i32, height as i32),
            source: ImageSource::full(width as i32, height as i32),
            offset: (0, 0),
            pixels: Rc::new(pixels),
        }
    }
    pub(super) fn from_dma(
        usage: Rc<super::dmabuf::BufferUse>,
        texture: Rc<linux_uapi::drm::VirglResource>,
    ) -> Self {
        let (width, height) = (texture.width() as usize, texture.height() as usize);
        Self::with(
            width,
            height,
            Pixels::Dma {
                usage,
                texture,
                readback: OnceCell::new(),
            },
        )
    }
    /// SHM 提交：可原地更新的上一份内容只重读 `damage`，否则整幅读取成新快照。
    ///
    /// # Parameters
    ///
    /// - `previous`: 本 surface 上一份已提交内容。
    /// - `damage`: 本次提交的 buffer 像素损伤，已裁剪到 buffer 内。
    /// - `gpu`: GPU 合成时直接写入纹理；`None` 时保存 CPU 像素。
    ///
    /// # Errors
    ///
    /// 客户端截断了共享内存、无法分配，或纹理建立/上传失败。
    pub(super) fn from_shm(
        previous: Option<Self>,
        spec: &super::shm::BufferSpec,
        damage: &[canvas::Rect],
        gpu: Option<&linux_uapi::drm::VirglContext>,
    ) -> Result<Self, super::shm::ReadError> {
        use super::shm::ReadError::Upload;
        if let Some(mut content) = previous
            && content.update(spec, damage, gpu)?
        {
            return Ok(content);
        }
        let (width, height) = (spec.width, spec.height);
        let pixels = match gpu {
            None => {
                let mut pixels = Vec::new();
                pixels
                    .try_reserve_exact(width * height)
                    .map_err(|_| super::shm::ReadError::NoMemory)?;
                spec.read_rect(spec.bounds(), |_, row| pixels.extend_from_slice(row))?;
                Pixels::Memory(pixels)
            }
            Some(gpu) => {
                let texture = gpu
                    .create_texture(width as u32, height as u32)
                    .map_err(|_| Upload)?;
                spec.read_rect(spec.bounds(), |y, row| texture.write_row(y, 0, row))?;
                texture
                    .transfer_to_host(0, 0, width as u32, height as u32)
                    .map_err(|_| Upload)?;
                Pixels::Texture {
                    texture: Rc::new(texture),
                    readback: OnceCell::new(),
                }
            }
        };
        Ok(Self::with(width, height, pixels))
    }
    /// 尺寸与存储方式相同且没有其他持有者时，原地重读损伤矩形；返回是否已更新。
    fn update(
        &mut self,
        spec: &super::shm::BufferSpec,
        damage: &[canvas::Rect],
        gpu: Option<&linux_uapi::drm::VirglContext>,
    ) -> Result<bool, super::shm::ReadError> {
        use super::shm::ReadError::Upload;
        if (self.width, self.height) != (spec.width, spec.height) {
            return Ok(false);
        }
        let width = self.width;
        let Some(pixels) = Rc::get_mut(&mut self.pixels) else {
            return Ok(false);
        };
        match (pixels, gpu) {
            (Pixels::Memory(memory), None) => {
                for rect in damage {
                    let x = rect.x as usize;
                    spec.read_rect(*rect, |y, row| {
                        memory[y * width + x..][..row.len()].copy_from_slice(row);
                    })?;
                }
            }
            (Pixels::Texture { texture, readback }, Some(gpu)) => {
                *readback = OnceCell::new();
                // 纹理可能仍被在途帧引用；write_texture 按资源状态选择 transfer 或命令流内联写入，不等待 fence。
                let mut rows = Vec::new();
                for rect in damage {
                    rows.clear();
                    rows.try_reserve_exact(rect.width as usize * rect.height as usize)
                        .map_err(|_| super::shm::ReadError::NoMemory)?;
                    spec.read_rect(*rect, |_, row| rows.extend_from_slice(row))?;
                    gpu.write_texture(
                        texture,
                        rect.x as u32,
                        rect.y as u32,
                        rect.width as u32,
                        rect.height as u32,
                        &rows,
                    )
                    .map_err(|_| Upload)?;
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    pub(super) fn y_inverted(&self) -> bool {
        matches!(self.pixels.as_ref(), Pixels::Dma { usage, .. } if matches!(usage.buffer.as_ref(), Buffer::Dma(buffer) if buffer.y_invert))
    }
    /// 是否引用客户端 dma-buf；这类内容在帧显示完成前必须保持协议使用期，不能提前 release。
    pub(crate) fn is_dma(&self) -> bool {
        matches!(self.pixels.as_ref(), Pixels::Dma { .. })
    }
    /// GPU 合成采样的纹理；CPU 后端的内容没有纹理。
    pub(crate) fn texture(&self) -> Option<&Rc<linux_uapi::drm::VirglResource>> {
        match self.pixels.as_ref() {
            Pixels::Memory(_) => None,
            Pixels::Texture { texture, .. } | Pixels::Dma { texture, .. } => Some(texture),
        }
    }
    /// CPU 消费者按需读回（例如硬件光标）；GPU 场景只借用 texture，不进入此路径。
    pub(crate) fn pixels(&self) -> std::io::Result<&[u32]> {
        let (texture, readback) = match self.pixels.as_ref() {
            Pixels::Memory(pixels) => return Ok(pixels),
            Pixels::Texture { texture, readback }
            | Pixels::Dma {
                texture, readback, ..
            } => (texture, readback),
        };
        if readback.get().is_none() {
            // 内联写入只到达主机纹理，guest backing 可能落后；与 dma-buf 一样先从主机读回。
            texture.wait()?;
            texture.transfer_from_host(0, 0, texture.width(), texture.height())?;
            texture.wait()?;
            let mut pixels = Vec::new();
            pixels
                .try_reserve_exact(self.width * self.height)
                .map_err(std::io::Error::other)?;
            pixels.resize(self.width * self.height, 0);
            for (row, line) in pixels.chunks_exact_mut(self.width).enumerate() {
                texture.read_row(row, 0, line);
            }
            let _ = readback.set(pixels);
        }
        Ok(readback.get().expect("readback published"))
    }
    pub(crate) fn same_image(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.pixels, &other.pixels)
            && self.size == other.size
            && self.source == other.source
    }
    pub(crate) fn output_size(&self, scale: i32) -> (i32, i32) {
        (
            self.size.0.saturating_mul(scale),
            self.size.1.saturating_mul(scale),
        )
    }
}

/// 待提交的真实缓冲 owner；destroy wl_buffer 不能撤销已经 attach 的引用。
#[derive(Clone)]
pub(super) struct Attachment {
    pub object: ObjectId,
    pub buffer: Rc<Buffer>,
}

/// `attach` 的结果：`Some(None)` 表示显式 attach 了空 buffer（取消映射）。
#[derive(Default)]
pub struct Pending {
    pub buffer: Option<Option<Attachment>>,
    pub frame_callbacks: Vec<ObjectId>,
    pub scale: Option<i32>,
    pub transform: Option<i32>,
    pub order: Option<Vec<ObjectId>>,
    pub input_region: Option<Option<Rc<super::region::Region>>>,
    pub offset: Option<(i32, i32)>,
    pub damage: super::damage::Damage,
}

/// wl_surface 一次提交的完整状态；缓存提交与当前显示状态共享不可变像素，元数据各自属于那次提交。
#[derive(Clone)]
pub(super) struct State {
    pub content: Option<Content>,
    pub scale: i32,
    pub transform: i32,
    pub viewport: super::viewport::Params,
    pub offset: (i64, i64),
    /// 已提交的堆叠顺序，含父 surface 标记；初始只有父，新孩子默认置于最上。
    pub order: Vec<ObjectId>,
    pub input_region: Option<Rc<super::region::Region>>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            content: None,
            scale: 1,
            transform: 0,
            viewport: (None, None),
            offset: (0, 0),
            order: Vec::new(),
            input_region: None,
        }
    }
}

/// 同步子 surface 已提交但尚未随祖先生效的状态与回调；不是第二份可写像素。
pub(super) struct Committed {
    pub state: State,
    /// 最后一次 attach 明确提交 null；普通无 attach 的初始/元数据提交不能触发角色 unmap。
    /// 同步缓存必须保留该请求含义，否则父应用时无法区分初始协商与撤销映射。
    pub unmapping: bool,
    pub callbacks: Vec<ObjectId>,
    /// 这些位置请求已由该父的 commit 冻结；之后的新请求仍留在 Subsurface.pending 中。
    pub positions: std::collections::BTreeMap<ObjectId, (i32, i32)>,
}

#[derive(Default)]
pub struct Surface {
    pub pending: Pending,
    pub current: State,
    pub cached: Option<Committed>,
    /// 已提交、等下一帧呈现后发 `done` 的 frame callback。
    pub queued_callbacks: Vec<ObjectId>,
    /// 当前已提交显示帧的 callback；flip 完成不能误消费随后新 commit 的回调。
    pub in_flight_callbacks: Vec<ObjectId>,
    /// 该 surface 的 `xdg_surface` 对象（若已建立）。
    pub xdg: Option<ObjectId>,
    /// 该 surface 的 `zwlr_layer_surface_v1` 对象（若已建立）。
    pub layer: Option<ObjectId>,
    /// cursor 角色在 surface 生命周期内保留；否则光标可能被再次用作应用窗口。
    pub cursor: bool,
    /// 已通知客户端进入输出（`wl_surface.enter`）。
    pub entered: bool,
    /// `wp_viewport` 的裁剪与缩放；缺少它 Wine 的全屏缩放无法生效。
    pub viewport: super::viewport::ViewportState,
    /// 子 surface 的角色状态；有它的 surface 随父绘制。
    pub subsurface: Option<super::subsurface::Subsurface>,
    /// 角色永久标记：销毁 wl_subsurface 只去掉关联，不能把同一 surface 改用为 xdg/layer/cursor。
    pub subsurface_role: bool,
    /// 仍关联的子 surface 索引；绘制顺序只由已提交 State.order 拥有。
    pub children: Vec<ObjectId>,
}

impl Surface {
    pub fn new(object: ObjectId) -> Self {
        let mut state = Self::default();
        state.current.order.push(object);
        state
    }

    pub(super) fn pending_viewport(&mut self) -> &mut super::viewport::Params {
        let current = self
            .cached
            .as_ref()
            .map_or(self.current.viewport, |commit| commit.state.viewport);
        self.viewport.pending.get_or_insert(current)
    }
}
