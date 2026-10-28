//! 外壳 surface 与普通 xdg 窗口共享连接、帧和输入生命周期，角色只由 Role 枚举拥有。

use super::{Configuration, Display, Event, EventKind, Role, Window, WindowId};
pub use layer::{KeyboardInteractivity, anchor as layer_anchor};
pub use shell::Layer;
const HORIZONTAL: u32 = layer_anchor::LEFT | layer_anchor::RIGHT;
const VERTICAL: u32 = layer_anchor::TOP | layer_anchor::BOTTOM;
use std::io;
use wayland_wire::{
    protocol::{
        Interface, wl_compositor, wl_surface, zwlr_layer_shell_v1 as shell,
        zwlr_layer_surface_v1 as layer,
    },
    wire::ObjectId,
};

/// layer-shell 的双缓冲配置。宽高、边距和独占区均为逻辑像素。
/// 宽/高为零时必须锚定该方向的两条对边；正独占区为最大化窗口保留空间，-1 铺到输出边缘。
#[derive(Debug, Clone, Copy)]
pub struct LayerSettings {
    pub layer: Layer,
    pub width: u32,
    pub height: u32,
    pub anchor: u32,
    pub margin: [i32; 4],
    pub exclusive_zone: i32,
    pub keyboard: KeyboardInteractivity,
}

impl Default for LayerSettings {
    /// 默认 Top 层铺满输出、不保留独占区、不获取键盘。
    fn default() -> Self {
        Self {
            layer: Layer::Top,
            width: 0,
            height: 0,
            anchor: HORIZONTAL | VERTICAL,
            margin: [0; 4],
            exclusive_zone: 0,
            keyboard: KeyboardInteractivity::None,
        }
    }
}

impl LayerSettings {
    fn validate(self) -> io::Result<()> {
        if self.anchor & !(HORIZONTAL | VERTICAL) != 0
            || self.width > i32::MAX as u32
            || self.height > i32::MAX as u32
            || self.width == 0 && self.anchor & HORIZONTAL != HORIZONTAL
            || self.height == 0 && self.anchor & VERTICAL != VERTICAL
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid layer size or anchors",
            ));
        }
        Ok(())
    }
}

impl Display {
    /// 建立外壳图层，等待 Configure 后用普通 present 提交；namespace 仅作用途标记，不授予权限。
    ///
    /// # Errors
    ///
    /// 普通连接没有 layer-shell 时返回 PermissionDenied；无效配置、编码或 socket 错误直接返回。
    pub fn create_layer(
        &mut self,
        namespace: &str,
        settings: LayerSettings,
    ) -> io::Result<WindowId> {
        settings.validate()?;
        // new_id、surface、output、layer 四个固定参数占 16 字节；先验证再分配协议对象。
        super::validate_wire_text(namespace, 16)?;
        let shell = self.layer_shell.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "connection has no layer-shell capability",
            )
        })?;
        let surface = self.transport.allocate(Interface::WlSurface)?;
        let object = self.transport.allocate(Interface::ZwlrLayerSurfaceV1)?;
        wl_compositor::Request::CreateSurface { id: surface }
            .encode(self.compositor, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        shell::Request::GetLayerSurface {
            id: object,
            surface,
            output: None,
            layer: settings.layer,
            namespace: namespace.into(),
        }
        .encode(shell, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        let configuration = Configuration {
            width: settings.width as i32,
            height: settings.height as i32,
            scale: self.scale,
            active: false,
            maximized: false,
            fullscreen: false,
            resizing: false,
        };
        let id = WindowId(surface);
        self.windows.insert(
            id,
            Window {
                role: Role::Layer { object },
                configuration,
                configured: false,
                mapped: false,
                frame: None,
                history: Default::default(),
            },
        );
        self.configure_layer(id, settings)?;
        Ok(id)
    }

    /// 提交新的 layer 配置；也用于 unmap 后启动新的 configure/ack 握手。
    ///
    /// # Errors
    ///
    /// 配置无效、目标不是 layer，或编码/socket 失败。
    pub fn configure_layer(&mut self, window: WindowId, settings: LayerSettings) -> io::Result<()> {
        settings.validate()?;
        let Some(Role::Layer { object }) = self.windows.get(&window).map(|window| &window.role)
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a layer surface",
            ));
        };
        let object = *object;
        let writer = &mut self.transport.writer;
        layer::Request::SetLayer {
            layer: settings.layer,
        }
        .encode(object, writer)
        .map_err(io::Error::other)?;
        layer::Request::SetSize {
            width: settings.width,
            height: settings.height,
        }
        .encode(object, writer)
        .map_err(io::Error::other)?;
        layer::Request::SetAnchor {
            anchor: settings.anchor,
        }
        .encode(object, writer)
        .map_err(io::Error::other)?;
        let [top, right, bottom, left] = settings.margin;
        layer::Request::SetMargin {
            top,
            right,
            bottom,
            left,
        }
        .encode(object, writer)
        .map_err(io::Error::other)?;
        layer::Request::SetExclusiveZone {
            zone: settings.exclusive_zone,
        }
        .encode(object, writer)
        .map_err(io::Error::other)?;
        layer::Request::SetKeyboardInteractivity {
            keyboard_interactivity: settings.keyboard,
        }
        .encode(object, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Commit
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 取消 layer 映射，撤销旧配置和本地键盘重复；再次显示必须 configure_layer 并等待新 Configure。
    ///
    /// # Errors
    ///
    /// 目标不是 layer、编码或 socket 失败。
    pub fn unmap_layer(&mut self, window: WindowId) -> io::Result<()> {
        let state = self
            .windows
            .get_mut(&window)
            .filter(|window| matches!(window.role, Role::Layer { .. }))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a layer surface"))?;
        state.configured = false;
        state.frame = None;
        // 撤销映射后合成器没有内容；保留历史会让下一次空损伤只 commit 不 attach，表面永远不再出现。
        state.history = Default::default();
        self.events.retain(|event| event.window != window);
        if self.keyboard == Some(window) {
            self.keyboard = None;
            self.repeat = None;
        }
        if self.pointer == Some(window) {
            self.pointer = None;
        }
        wl_surface::Request::Attach {
            buffer: None,
            x: 0,
            y: 0,
        }
        .encode(window.0, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Commit
            .encode(window.0, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }

    pub(super) fn layer_event(&mut self, object: ObjectId, event: layer::Event) -> io::Result<()> {
        let Some((id, state)) = self.windows.iter_mut().find(
            |(_, window)| matches!(window.role, Role::Layer { object: role } if role == object),
        ) else {
            return Ok(());
        };
        let kind = match event {
            layer::Event::Configure {
                serial,
                width,
                height,
            } => {
                let mut configuration = state.configuration;
                configuration.width = i32::try_from(width).map_err(io::Error::other)?;
                configuration.height = i32::try_from(height).map_err(io::Error::other)?;
                configuration.pixel_size()?;
                layer::Request::AckConfigure { serial }
                    .encode(object, &mut self.transport.writer)
                    .map_err(io::Error::other)?;
                state.configuration = configuration;
                state.configured = true;
                EventKind::Configure(configuration)
            }
            layer::Event::Closed => EventKind::Close,
        };
        self.events.push_back(Event { window: *id, kind });
        Ok(())
    }
}
