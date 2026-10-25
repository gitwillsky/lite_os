//! `zwlr_foreign_toplevel_management_v1`：把全部顶层窗口告诉桌面外壳（任务栏按钮、窗口列表），
//! 并接受它对这些窗口的激活、最小化、最大化与关闭请求。只有特权客户端能绑定。

use wayland_wire::{
    protocol::{
        Interface, zwlr_foreign_toplevel_handle_v1 as handle,
        zwlr_foreign_toplevel_manager_v1 as manager,
    },
    wire::ObjectId,
};

use super::{
    ClientId,
    client::{Client, Data, Fail},
};

/// 窗口对外可见的状态位（`zwlr_foreign_toplevel_handle_v1.state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ForeignState {
    pub maximized: bool,
    pub minimized: bool,
    pub activated: bool,
    pub fullscreen: bool,
}

impl ForeignState {
    fn encode(self) -> Vec<u8> {
        [
            (self.maximized, 0u32),
            (self.minimized, 1),
            (self.activated, 2),
            (self.fullscreen, 3),
        ]
        .into_iter()
        .filter(|(set, _)| *set)
        .flat_map(|(_, value)| value.to_ne_bytes())
        .collect()
    }
}

/// 一个窗口的对外描述；`(client, surface)` 唯一标识窗口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignInfo {
    pub client: ClientId,
    pub surface: ObjectId,
    pub title: String,
    pub app_id: String,
    pub state: ForeignState,
}

/// 外壳对窗口提出的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignAction {
    Maximize(bool),
    Minimize(bool),
    Fullscreen(bool),
    Activate,
    Close,
    /// 最小化动画的目标：外壳 surface `surface` 内（逻辑像素）这个任务按钮的矩形。
    Rectangle {
        surface: ObjectId,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
}

/// 外壳提出的、针对某个窗口的请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignRequest {
    /// 发出请求的外壳连接；由 Server 取走请求时填入，`Client` 不知道自己的编号。
    pub shell: ClientId,
    pub client: ClientId,
    pub surface: ObjectId,
    pub action: ForeignAction,
}

/// 一个客户端绑定的管理器，以及它已经告诉外壳的窗口。
pub struct ForeignManager {
    manager: ObjectId,
    version: u32,
    /// finished 后停止宣告新窗口，但既有句柄仍独立存活；缺少它会向已销毁的 manager 发事件。
    stopped: bool,
    /// `(窗口句柄, 最近一次发送的描述)`。
    handles: Vec<(ObjectId, ForeignInfo)>,
}

/// 句柄对象指向的窗口。
pub struct ForeignHandle {
    /// closed 后撤销目标；否则客户端复用 surface id 时，旧句柄可能错误控制新窗口。
    target: Option<(ClientId, ObjectId)>,
}

/// 服务端分配的对象 id 从这里开始（客户端分配的 id 都小于它）。
pub(super) const SERVER_ID_BASE: u32 = 0xFF00_0000;

impl Client {
    pub(super) fn foreign_manager(
        &mut self,
        object: ObjectId,
        request: manager::Request,
    ) -> Result<(), Fail> {
        let manager::Request::Stop = request;
        let Some(state) = self
            .foreign
            .iter_mut()
            .find(|state| state.manager == object && !state.stopped)
        else {
            return Ok(());
        };
        state.stopped = true;
        manager::Event::Finished.encode(object, &mut self.writer)?;
        self.release_id(object);
        Ok(())
    }

    /// 每次绑定建立独立的窗口列表；停止管理器后仍保留已经创建的句柄。
    pub(super) fn bind_foreign_manager(
        &mut self,
        manager: ObjectId,
        version: u32,
    ) -> Result<(), Fail> {
        self.foreign.push(ForeignManager {
            manager,
            version,
            stopped: false,
            handles: Vec::new(),
        });
        Ok(())
    }

    pub(super) fn foreign_handle(
        &mut self,
        object: ObjectId,
        request: handle::Request,
    ) -> Result<(), Fail> {
        let (client, surface) = match self.objects.get(&object.get()) {
            Some(entry) => match &entry.data {
                Data::ForeignHandle(target) => match target.target {
                    Some(target) => target,
                    None => return Ok(()),
                },
                _ => return Err(Fail::implementation(object)),
            },
            None => return Err(Fail::implementation(object)),
        };
        use handle::Request as R;
        let action = match request {
            R::SetMaximized => ForeignAction::Maximize(true),
            R::UnsetMaximized => ForeignAction::Maximize(false),
            R::SetMinimized => ForeignAction::Minimize(true),
            R::UnsetMinimized => ForeignAction::Minimize(false),
            R::SetFullscreen { .. } => ForeignAction::Fullscreen(true),
            R::UnsetFullscreen => ForeignAction::Fullscreen(false),
            R::Activate { .. } => ForeignAction::Activate,
            R::Close => ForeignAction::Close,
            R::SetRectangle {
                surface: shell_surface,
                x,
                y,
                width,
                height,
            } => ForeignAction::Rectangle {
                surface: shell_surface,
                x,
                y,
                width,
                height,
            },
            // 保留已告知窗口的记录；对象表删除句柄后不再更新它，也不重复宣告同一窗口。
            R::Destroy => return Ok(()),
        };
        self.foreign_requests.push(ForeignRequest {
            shell: ClientId(0),
            client,
            surface,
            action,
        });
        Ok(())
    }

    /// 是否已有可以接收窗口列表的管理器。
    pub fn has_foreign_manager(&self) -> bool {
        !self.foreign.is_empty()
    }

    /// 让外壳看到的窗口列表与 `windows` 一致：新窗口发 `toplevel`，变化的发更新，消失的发 `closed`。
    ///
    /// 可以每个循环都调用：没有变化时不发送任何东西。
    pub fn sync_foreign(&mut self, windows: &[ForeignInfo]) {
        let mut managers = std::mem::take(&mut self.foreign);
        for state in &mut managers {
            state.handles.retain(|(handle, info)| {
                let alive = windows
                    .iter()
                    .any(|window| window.client == info.client && window.surface == info.surface);
                if !alive && self.objects.contains_key(&handle.get()) {
                    let _ = handle::Event::Closed.encode(*handle, &mut self.writer);
                    if let Some(super::client::Entry {
                        data: Data::ForeignHandle(handle),
                        ..
                    }) = self.objects.get_mut(&handle.get())
                    {
                        handle.target = None;
                    }
                }
                alive
            });
            for window in windows {
                let known = state.handles.iter().position(|(_, info)| {
                    info.client == window.client && info.surface == window.surface
                });
                match known {
                    Some(index) => self.update_handle(&mut state.handles[index], window),
                    None if !state.stopped => self.announce_window(state, window),
                    None => {}
                }
            }
        }
        managers.retain(|state| !state.stopped || !state.handles.is_empty());
        self.foreign = managers;
    }

    fn announce_window(&mut self, state: &mut ForeignManager, window: &ForeignInfo) {
        let (manager, version) = (state.manager, state.version);
        let handle = ObjectId::new(self.next_server_object).expect("server ids start above zero");
        self.next_server_object += 1;
        self.objects.insert(
            handle.get(),
            super::client::Entry {
                interface: Interface::ZwlrForeignToplevelHandleV1,
                version,
                data: Data::ForeignHandle(ForeignHandle {
                    target: Some((window.client, window.surface)),
                }),
            },
        );
        let writer = &mut self.writer;
        // 编码只会因超长标题失败；此时这个窗口对外壳不可见，比断开连接更合适。
        let sent = manager::Event::Toplevel { toplevel: handle }
            .encode(manager, writer)
            .and_then(|()| {
                handle::Event::Title {
                    title: window.title.clone(),
                }
                .encode(handle, writer)
            })
            .and_then(|()| {
                handle::Event::AppId {
                    app_id: window.app_id.clone(),
                }
                .encode(handle, writer)
            })
            .and_then(|()| {
                handle::Event::State {
                    state: window.state.encode(),
                }
                .encode(handle, writer)
            })
            .and_then(|()| handle::Event::Done.encode(handle, writer));
        if sent.is_ok() {
            state.handles.push((handle, window.clone()));
        }
    }

    fn update_handle(
        &mut self,
        (handle, sent): &mut (ObjectId, ForeignInfo),
        window: &ForeignInfo,
    ) {
        if !self.objects.contains_key(&handle.get()) {
            return;
        }
        let handle = *handle;
        if sent == window {
            return;
        }
        let writer = &mut self.writer;
        if sent.title != window.title {
            let _ = handle::Event::Title {
                title: window.title.clone(),
            }
            .encode(handle, writer);
        }
        if sent.app_id != window.app_id {
            let _ = handle::Event::AppId {
                app_id: window.app_id.clone(),
            }
            .encode(handle, writer);
        }
        if sent.state != window.state {
            let _ = handle::Event::State {
                state: window.state.encode(),
            }
            .encode(handle, writer);
        }
        let _ = handle::Event::Done.encode(handle, writer);
        *sent = window.clone();
    }

    /// 取走自上次以来外壳提出的窗口操作。
    pub fn take_foreign_requests(&mut self) -> Vec<ForeignRequest> {
        std::mem::take(&mut self.foreign_requests)
    }
}
