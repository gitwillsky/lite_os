//! `wl_surface` 的双缓冲状态：请求先写入 pending，`commit` 时一次性生效。

use wayland_wire::wire::ObjectId;

/// 已提交的像素内容。
pub struct Content {
    pub width: usize,
    pub height: usize,
    /// 预乘 `0xAARRGGBB`，行优先、无行距。
    pub pixels: Vec<u32>,
    /// 提交内容不可变，首次 GPU 使用后拥有纹理；只有 Content 析构且在途帧完成才关闭 GEM。
    texture: std::cell::OnceCell<std::rc::Rc<linux_uapi::drm::VirglResource>>,
}
impl Content {
    pub(crate) fn new(width: usize, height: usize, pixels: Vec<u32>) -> Self {
        Self {
            width,
            height,
            pixels,
            texture: std::cell::OnceCell::new(),
        }
    }
    pub(crate) fn texture(
        &self,
        context: &linux_uapi::drm::VirglContext,
    ) -> std::io::Result<std::rc::Rc<linux_uapi::drm::VirglResource>> {
        if let Some(texture) = self.texture.get() {
            return Ok(texture.clone());
        }
        let mut resource = context.create_texture(self.width as u32, self.height as u32)?;
        for y in 0..self.height {
            resource
                .row_mut(y)
                .copy_from_slice(&self.pixels[y * self.width..(y + 1) * self.width]);
        }
        resource.transfer_to_host(0, 0, self.width as u32, self.height as u32)?;
        let resource = std::rc::Rc::new(resource);
        // OWNER: 单线程 Content 的 OnceCell；失败不发布半个纹理，下次绘制可重试资源准备。
        // FAILURE: 不保留纹理会在每次窗口移动时重新上传全窗口像素，抵消 GPU 合成收益。
        let _ = self.texture.set(resource.clone());
        Ok(resource)
    }
}

/// `attach` 的结果：`Some(None)` 表示显式 attach 了空 buffer（取消映射）。
#[derive(Default)]
pub struct Pending {
    pub buffer: Option<Option<ObjectId>>,
    pub frame_callbacks: Vec<ObjectId>,
    pub scale: Option<i32>,
}

#[derive(Default)]
pub struct Surface {
    pub pending: Pending,
    pub content: Option<Content>,
    /// 已提交、等下一帧呈现后发 `done` 的 frame callback。
    pub queued_callbacks: Vec<ObjectId>,
    pub scale: i32,
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
