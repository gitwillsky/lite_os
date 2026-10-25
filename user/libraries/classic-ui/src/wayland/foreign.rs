//! 任务栏消费 foreign-toplevel；pending 在 done 时发布，closed 立即撤销句柄。
use super::Display;
use std::io;
use wayland_wire::{
    protocol::{
        AnyEvent, Interface, zwlr_foreign_toplevel_handle_v1 as handle,
        zwlr_foreign_toplevel_manager_v1 as manager,
    },
    wire::ObjectId,
};

/// 本连接中服务端拥有的顶层窗口句柄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToplevelId(ObjectId);
/// 已由 done 确认的任务栏信息；只通过协议事件更新。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toplevel {
    pub title: String,
    pub app_id: String,
    pub maximized: bool,
    pub minimized: bool,
    pub active: bool,
    pub fullscreen: bool,
}
pub(super) struct Pending {
    current: Option<Toplevel>,
    next: Toplevel,
}
/// 外壳可向 compositor 提出的标准窗口操作；结果以之后的状态事件为准。
#[derive(Debug, Clone, Copy)]
pub enum ToplevelRequest {
    Activate,
    Minimize(bool),
    Maximize(bool),
    Close,
}
impl Display {
    /// 当前完整的顶层窗口快照；迭代器不含尚未 done 或已经 closed 的对象。
    pub fn toplevels(&self) -> impl Iterator<Item = (ToplevelId, &Toplevel)> {
        self.foreign.iter().filter_map(|(id, state)| {
            state
                .current
                .as_ref()
                .map(|current| (ToplevelId(ObjectId::new(*id).unwrap()), current))
        })
    }
    /// 窗口列表的发布版本；桌面据此合并重绘，不为每条属性消息分配事件。
    pub fn toplevel_revision(&self) -> u64 {
        self.foreign_revision
    }
    /// 向 `id` 发送 `action`；Ok 只表示请求已发送，实际状态仍等待后续 done。
    /// 无效或已关闭的句柄返回 NotFound，编码/socket 失败返回对应错误。
    pub fn control_toplevel(&mut self, id: ToplevelId, action: ToplevelRequest) -> io::Result<()> {
        if !self.foreign.contains_key(&id.0.get()) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let request = match action {
            ToplevelRequest::Activate => handle::Request::Activate { seat: self.seat },
            ToplevelRequest::Minimize(true) => handle::Request::SetMinimized,
            ToplevelRequest::Minimize(false) => handle::Request::UnsetMinimized,
            ToplevelRequest::Maximize(true) => handle::Request::SetMaximized,
            ToplevelRequest::Maximize(false) => handle::Request::UnsetMaximized,
            ToplevelRequest::Close => handle::Request::Close,
        };
        request
            .encode(id.0, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }
    pub(super) fn foreign_event(&mut self, object: ObjectId, event: &AnyEvent) -> io::Result<bool> {
        match event {
            AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Toplevel { toplevel }) => {
                self.transport
                    .server_object(*toplevel, Interface::ZwlrForeignToplevelHandleV1)?;
                self.foreign.insert(
                    toplevel.get(),
                    Pending {
                        current: None,
                        next: Toplevel::default(),
                    },
                );
            }
            AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Finished) => {}
            AnyEvent::ZwlrForeignToplevelHandleV1(event) => {
                let state = self
                    .foreign
                    .get_mut(&object.get())
                    .ok_or_else(|| io::Error::other("unknown foreign handle"))?;
                match event {
                    handle::Event::Title { title } => state.next.title.clone_from(title),
                    handle::Event::AppId { app_id } => state.next.app_id.clone_from(app_id),
                    handle::Event::State { state: bytes } => {
                        if bytes.len() % 4 != 0 {
                            return Err(io::ErrorKind::InvalidData.into());
                        }
                        let values: Vec<_> = bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|b| u32::from_ne_bytes(*b))
                            .collect();
                        state.next.maximized = values.contains(&0);
                        state.next.minimized = values.contains(&1);
                        state.next.active = values.contains(&2);
                        state.next.fullscreen = values.contains(&3);
                    }
                    handle::Event::Done => {
                        state.current = Some(state.next.clone());
                        self.foreign_revision += 1;
                    }
                    handle::Event::Closed => {
                        self.foreign.remove(&object.get());
                        self.foreign_revision += 1;
                        self.transport.retire_server_object(object);
                        handle::Request::Destroy
                            .encode(object, &mut self.transport.writer)
                            .map_err(io::Error::other)?;
                    }
                    _ => {}
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
