//! `xdg_shell`：顶层窗口的角色、configure/ack 握手与窗口请求。当前不支持 popup 与 positioner 的几何。

use wayland_wire::{
    protocol::{Interface, xdg_surface, xdg_toplevel, xdg_wm_base},
    wire::ObjectId,
};

use super::{
    Content,
    client::{Client, Data, Entry, Fail},
};

/// `xdg_surface`：把 `wl_surface` 变成可映射窗口的中间对象。
pub struct XdgSurface {
    pub surface: ObjectId,
    pub toplevel: Option<ObjectId>,
    /// 已发出、尚未被 `ack_configure` 确认的序号，按发送顺序。
    pub outstanding: Vec<u32>,
    /// 客户端是否至少确认过一次 configure；之前 commit 带 buffer 是协议错误。
    pub acked: bool,
    /// 已为该窗口发过初始 configure；缺少它，客户端会一直等待而永远不映射。
    pub initial_sent: bool,
}

pub struct Toplevel {
    pub xdg_surface: ObjectId,
    pub title: String,
    pub app_id: String,
}

/// 客户端对窗口提出的、需要窗口管理器裁决的请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRequest {
    /// `serial` 必须是客户端收到的、触发这次拖动的按钮事件的序号。
    Move {
        serial: u32,
    },
    /// `edges` 是 `xdg_toplevel.resize_edge` 的数值。
    Resize {
        serial: u32,
        edges: u32,
    },
    Maximize(bool),
    Minimize,
    Fullscreen(bool),
    /// `xdg_toplevel.show_window_menu`：`x`、`y` 是 surface 局部的逻辑坐标。
    ShowMenu {
        serial: u32,
        x: i32,
        y: i32,
    },
}

/// `xdg_toplevel.configure` 携带的窗口状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToplevelState {
    pub activated: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    /// 用户正在拖动边框缩放；客户端据此可以放宽重绘质量。
    pub resizing: bool,
}

impl ToplevelState {
    /// `xdg_toplevel.state` 的数值，按协议编码为 u32 数组。
    fn encode(self) -> Vec<u8> {
        [
            (self.maximized, 1u32),
            (self.fullscreen, 2),
            (self.resizing, 3),
            (self.activated, 4),
        ]
        .into_iter()
        .filter(|(set, _)| *set)
        .flat_map(|(_, value)| value.to_ne_bytes())
        .collect()
    }
}

/// 一个已映射的顶层窗口。
pub struct Window<'a> {
    pub surface: ObjectId,
    pub title: &'a str,
    pub app_id: &'a str,
    pub content: &'a Content,
    /// `wl_surface.set_buffer_scale`：内容的物理像素与逻辑像素之比。
    pub scale: i32,
}

impl Client {
    pub(super) fn xdg_wm_base(
        &mut self,
        object: ObjectId,
        version: u32,
        request: xdg_wm_base::Request,
    ) -> Result<(), Fail> {
        match request {
            xdg_wm_base::Request::Destroy => {
                let busy = self
                    .objects
                    .values()
                    .any(|entry| matches!(entry.data, Data::XdgSurface(_)));
                if busy {
                    return Err(Fail::new(
                        object,
                        xdg_wm_base::Error::DefunctSurfaces as u32,
                        "xdg_wm_base destroyed before its xdg_surfaces",
                    ));
                }
                Ok(())
            }
            xdg_wm_base::Request::CreatePositioner { id } => {
                self.create(id, Interface::XdgPositioner, version, Data::None)
            }
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let state = self.surface_mut(surface)?;
                if state.xdg.is_some() || state.layer.is_some() || state.cursor {
                    return Err(Fail::new(
                        object,
                        xdg_wm_base::Error::Role as u32,
                        "wl_surface already has an xdg_surface",
                    ));
                }
                let xdg = XdgSurface {
                    surface,
                    toplevel: None,
                    outstanding: Vec::new(),
                    acked: false,
                    initial_sent: false,
                };
                self.create(id, Interface::XdgSurface, version, Data::XdgSurface(xdg))?;
                self.surface_mut(surface)?.xdg = Some(id);
                Ok(())
            }
            // 本服务端不发送 ping，所以不会有需要匹配的 pong。
            xdg_wm_base::Request::Pong { .. } => Ok(()),
        }
    }

    fn xdg_surface_mut(&mut self, object: ObjectId) -> Result<&mut XdgSurface, Fail> {
        match self.objects.get_mut(&object.get()) {
            Some(Entry {
                data: Data::XdgSurface(xdg),
                ..
            }) => Ok(xdg),
            _ => Err(Fail::implementation(object)),
        }
    }

    pub(super) fn xdg_surface(
        &mut self,
        object: ObjectId,
        version: u32,
        request: xdg_surface::Request,
    ) -> Result<(), Fail> {
        match request {
            xdg_surface::Request::Destroy => {
                let xdg = self.xdg_surface_mut(object)?;
                if xdg.toplevel.is_some() {
                    return Err(Fail::new(
                        object,
                        xdg_surface::Error::DefunctRoleObject as u32,
                        "xdg_surface destroyed before its role object",
                    ));
                }
                let surface = xdg.surface;
                // 窗口随 xdg_surface 一起消失；wl_surface 本身可能已先被销毁。
                if let Ok(surface) = self.surface_mut(surface) {
                    surface.xdg = None;
                    surface.content = None;
                }
                Ok(())
            }
            xdg_surface::Request::GetToplevel { id } => {
                if self.xdg_surface_mut(object)?.toplevel.is_some() {
                    return Err(Fail::new(
                        object,
                        xdg_surface::Error::AlreadyConstructed as u32,
                        "xdg_surface already has a role",
                    ));
                }
                let toplevel = Toplevel {
                    xdg_surface: object,
                    title: String::new(),
                    app_id: String::new(),
                };
                self.create(
                    id,
                    Interface::XdgToplevel,
                    version,
                    Data::Toplevel(toplevel),
                )?;
                self.xdg_surface_mut(object)?.toplevel = Some(id);
                Ok(())
            }
            xdg_surface::Request::GetPopup { .. } => Err(Fail::new(
                ObjectId::DISPLAY,
                wayland_wire::protocol::wl_display::Error::Implementation as u32,
                "xdg_popup is not supported yet",
            )),
            xdg_surface::Request::SetWindowGeometry { width, height, .. } => {
                if width < 0 || height < 0 {
                    return Err(Fail::new(
                        object,
                        xdg_surface::Error::InvalidSize as u32,
                        "window geometry must not be negative",
                    ));
                }
                Ok(())
            }
            xdg_surface::Request::AckConfigure { serial } => {
                let xdg = self.xdg_surface_mut(object)?;
                let Some(position) = xdg.outstanding.iter().position(|sent| *sent == serial) else {
                    return Err(Fail::new(
                        object,
                        xdg_surface::Error::InvalidSerial as u32,
                        format!("serial {serial} was not sent or already acknowledged"),
                    ));
                };
                // 确认某个序号同时确认它之前的所有 configure。
                xdg.outstanding.drain(..=position);
                xdg.acked = true;
                Ok(())
            }
        }
    }

    pub(super) fn xdg_toplevel(
        &mut self,
        object: ObjectId,
        request: xdg_toplevel::Request,
    ) -> Result<(), Fail> {
        use xdg_toplevel::Request as R;
        let invalid_size = |message: &str| {
            Fail::new(
                object,
                xdg_toplevel::Error::InvalidSize as u32,
                message.to_owned(),
            )
        };
        let toplevel = match self.objects.get_mut(&object.get()) {
            Some(Entry {
                data: Data::Toplevel(toplevel),
                ..
            }) => toplevel,
            _ => return Err(Fail::implementation(object)),
        };
        match request {
            R::Destroy => {
                let xdg_surface = toplevel.xdg_surface;
                // 角色销毁后窗口取消映射，必须重新走一遍 configure/ack 才能再次映射。
                let surface = {
                    let xdg = self.xdg_surface_mut(xdg_surface)?;
                    xdg.toplevel = None;
                    xdg.initial_sent = false;
                    xdg.acked = false;
                    xdg.outstanding.clear();
                    xdg.surface
                };
                if let Ok(surface) = self.surface_mut(surface) {
                    surface.content = None;
                }
                Ok(())
            }
            R::SetTitle { title } => {
                toplevel.title = title;
                Ok(())
            }
            R::SetAppId { app_id } => {
                toplevel.app_id = app_id;
                Ok(())
            }
            R::SetMaxSize { width, height } | R::SetMinSize { width, height } => {
                if width < 0 || height < 0 {
                    return Err(invalid_size("size limits must not be negative"));
                }
                Ok(())
            }
            R::Move { serial, .. } => self.window_request(object, WindowRequest::Move { serial }),
            R::Resize { serial, edges, .. } => self.window_request(
                object,
                WindowRequest::Resize {
                    serial,
                    edges: edges as u32,
                },
            ),
            R::SetMaximized => self.window_request(object, WindowRequest::Maximize(true)),
            R::UnsetMaximized => self.window_request(object, WindowRequest::Maximize(false)),
            R::SetFullscreen { .. } => self.window_request(object, WindowRequest::Fullscreen(true)),
            R::UnsetFullscreen => self.window_request(object, WindowRequest::Fullscreen(false)),
            R::SetMinimized => self.window_request(object, WindowRequest::Minimize),
            R::ShowWindowMenu { serial, x, y, .. } => {
                self.window_request(object, WindowRequest::ShowMenu { serial, x, y })
            }
            // 父子关系由窗口管理器阶段处理；记录请求之前无需保存。
            R::SetParent { .. } => Ok(()),
        }
    }

    /// 记录窗口请求；键是窗口的 `wl_surface`，与 [`Window::surface`] 一致。
    fn window_request(&mut self, toplevel: ObjectId, request: WindowRequest) -> Result<(), Fail> {
        let surface = match self.objects.get(&toplevel.get()) {
            Some(Entry {
                data: Data::Toplevel(state),
                ..
            }) => match self.objects.get(&state.xdg_surface.get()) {
                Some(Entry {
                    data: Data::XdgSurface(xdg),
                    ..
                }) => xdg.surface,
                _ => return Err(Fail::implementation(toplevel)),
            },
            _ => return Err(Fail::implementation(toplevel)),
        };
        self.requests.push((surface, request));
        Ok(())
    }

    /// commit 之前的角色校验：已有 xdg_surface 但从未 ack 过 configure 就提交 buffer 是协议错误。
    pub(super) fn xdg_check_commit(&mut self, surface: ObjectId) -> Result<(), Fail> {
        let surface_state = self.surface_mut(surface)?;
        let (Some(xdg), Some(Some(_))) = (surface_state.xdg, surface_state.pending.buffer.as_ref())
        else {
            return Ok(());
        };
        if self.xdg_surface_mut(xdg)?.acked {
            return Ok(());
        }
        Err(Fail::new(
            xdg,
            xdg_surface::Error::UnconfiguredBuffer as u32,
            "buffer attached before the first configure was acknowledged",
        ))
    }

    /// commit 之后：窗口第一次提交（通常不带 buffer）时发送初始 configure，尺寸 0×0 表示由客户端决定。
    pub(super) fn xdg_after_commit(&mut self, surface: ObjectId) -> Result<(), Fail> {
        let Some(xdg) = self.surface_mut(surface)?.xdg else {
            return Ok(());
        };
        let xdg_state = self.xdg_surface_mut(xdg)?;
        let Some(toplevel) = xdg_state.toplevel.filter(|_| !xdg_state.initial_sent) else {
            return Ok(());
        };
        xdg_state.initial_sent = true;
        self.send_configure(xdg, toplevel, 0, 0, ToplevelState::default())
    }

    fn send_configure(
        &mut self,
        xdg: ObjectId,
        toplevel: ObjectId,
        width: i32,
        height: i32,
        state: ToplevelState,
    ) -> Result<(), Fail> {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        self.xdg_surface_mut(xdg)?.outstanding.push(serial);
        xdg_toplevel::Event::Configure {
            width,
            height,
            states: state.encode(),
        }
        .encode(toplevel, &mut self.writer)?;
        xdg_surface::Event::Configure { serial }.encode(xdg, &mut self.writer)?;
        Ok(())
    }

    /// 窗口管理器要求 `surface` 的窗口采用新的尺寸与状态。
    ///
    /// # Parameters
    ///
    /// - `width`、`height`: surface 逻辑像素；0 表示由客户端决定。
    ///
    /// 窗口尚未建立角色时忽略。
    pub fn configure(&mut self, surface: ObjectId, width: i32, height: i32, state: ToplevelState) {
        let target = self.toplevel_of(surface);
        if let Some((xdg, toplevel)) = target {
            // 序号只会因缓冲区超长而失败，configure 是定长消息。
            let _ = self.send_configure(xdg, toplevel, width, height, state);
        }
    }

    /// 要求窗口关闭（`xdg_toplevel.close`）；客户端可以拒绝或先询问用户。
    pub fn close(&mut self, surface: ObjectId) {
        if let Some((_, toplevel)) = self.toplevel_of(surface) {
            let _ = xdg_toplevel::Event::Close.encode(toplevel, &mut self.writer);
        }
    }

    fn toplevel_of(&self, surface: ObjectId) -> Option<(ObjectId, ObjectId)> {
        let Some(Entry {
            data: Data::Surface(state),
            ..
        }) = self.objects.get(&surface.get())
        else {
            return None;
        };
        let xdg = state.xdg?;
        match self.objects.get(&xdg.get()) {
            Some(Entry {
                data:
                    Data::XdgSurface(XdgSurface {
                        toplevel: Some(toplevel),
                        ..
                    }),
                ..
            }) => Some((xdg, *toplevel)),
            _ => None,
        }
    }

    /// 取走自上次以来客户端提出的窗口请求；键是窗口的 `wl_surface`。
    pub fn take_requests(&mut self) -> Vec<(ObjectId, WindowRequest)> {
        std::mem::take(&mut self.requests)
    }

    /// 全部已映射的顶层窗口：有 `xdg_toplevel` 角色且当前有内容。
    pub fn windows(&self) -> impl Iterator<Item = Window<'_>> {
        self.objects.values().filter_map(move |entry| {
            let Data::Toplevel(toplevel) = &entry.data else {
                return None;
            };
            let Some(Entry {
                data: Data::XdgSurface(xdg),
                ..
            }) = self.objects.get(&toplevel.xdg_surface.get())
            else {
                return None;
            };
            let Some(Entry {
                data: Data::Surface(surface),
                ..
            }) = self.objects.get(&xdg.surface.get())
            else {
                return None;
            };
            Some(Window {
                surface: xdg.surface,
                title: &toplevel.title,
                app_id: &toplevel.app_id,
                content: surface.content.as_ref()?,
                scale: surface.scale,
            })
        })
    }
}
