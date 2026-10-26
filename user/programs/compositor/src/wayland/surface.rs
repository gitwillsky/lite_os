//! `wl_surface` 的双缓冲状态：请求先写入 pending，`commit` 时一次性生效。

use canvas::ImageSource;
use std::{cell::OnceCell, rc::Rc};
use wayland_wire::wire::ObjectId;

/// 不可变像素与其 GPU 纹理由同一 owner 持有；视口改动只复制采样元数据。
struct Pixels {
    pixels: Vec<u32>,
    // OWNER: 同一组不可变像素共享一个纹理缓存；视口/同步提交的快照不会重复上传同一内容。
    // 在途帧仍由 GPU retained 引用保活；缺少共享 owner 会使缓存提交复制全窗口像素。
    texture: OnceCell<Rc<linux_uapi::drm::VirglResource>>,
}

/// 已提交内容的不可变像素引用与 surface 采样投影；Clone 不复制像素或建立第二份纹理。
#[derive(Clone)]
pub struct Content {
    pub width: usize,
    pub height: usize,
    pixels: Rc<Pixels>,
    pub size: (i32, i32),
    pub source: ImageSource,
}
impl Content {
    pub(crate) fn new(width: usize, height: usize, pixels: Vec<u32>) -> Self {
        Self {
            width,
            height,
            size: (width as i32, height as i32),
            source: ImageSource::full(width as i32, height as i32),
            pixels: Rc::new(Pixels {
                pixels,
                texture: OnceCell::new(),
            }),
        }
    }
    pub(crate) fn pixels(&self) -> &[u32] {
        &self.pixels.pixels
    }
    pub(crate) fn output_size(&self, scale: i32) -> (i32, i32) {
        (
            self.size.0.saturating_mul(scale),
            self.size.1.saturating_mul(scale),
        )
    }
    pub(crate) fn texture(
        &self,
        context: &linux_uapi::drm::VirglContext,
    ) -> std::io::Result<Rc<linux_uapi::drm::VirglResource>> {
        if let Some(texture) = self.pixels.texture.get() {
            return Ok(texture.clone());
        }
        let mut resource = context.create_texture(self.width as u32, self.height as u32)?;
        for y in 0..self.height {
            resource
                .row_mut(y)
                .copy_from_slice(&self.pixels()[y * self.width..(y + 1) * self.width]);
        }
        resource.transfer_to_host(0, 0, self.width as u32, self.height as u32)?;
        let resource = Rc::new(resource);
        let _ = self.pixels.texture.set(resource.clone());
        Ok(resource)
    }
}

/// 待提交的真实缓冲 owner；destroy wl_buffer 不能撤销已经 attach 的引用。
#[derive(Clone)]
pub(super) struct Attachment {
    pub object: ObjectId,
    pub buffer: Rc<super::shm::BufferSpec>,
}

/// `attach` 的结果：`Some(None)` 表示显式 attach 了空 buffer（取消映射）。
#[derive(Default)]
pub struct Pending {
    pub buffer: Option<Option<Attachment>>,
    pub frame_callbacks: Vec<ObjectId>,
    pub scale: Option<i32>,
    pub transform: Option<i32>,
}

#[derive(Default)]
pub struct Surface {
    pub pending: Pending,
    pub content: Option<Content>,
    /// 已提交、等下一帧呈现后发 `done` 的 frame callback。
    pub queued_callbacks: Vec<ObjectId>,
    pub scale: i32,
    /// 已提交的 wl_output.transform 值，0..7；新值只在 commit 时生效。
    pub transform: i32,
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
    /// 子 surface 列表，自底向上。
    pub children: Vec<ObjectId>,
}

impl Surface {
    pub fn new() -> Self {
        Self {
            scale: 1,
            ..Self::default()
        }
    }
}
