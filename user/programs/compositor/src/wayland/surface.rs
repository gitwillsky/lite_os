//! `wl_surface` 的双缓冲状态：请求先写入 pending，`commit` 时一次性生效。

use wayland_wire::wire::ObjectId;

/// 已提交的像素内容。
pub struct Content {
    pub width: usize,
    pub height: usize,
    /// 预乘 `0xAARRGGBB`，行优先、无行距。
    pub pixels: Vec<u32>,
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
