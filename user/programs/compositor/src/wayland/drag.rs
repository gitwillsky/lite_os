//! Wayland 拖放的 seat 抓取、目标 offer 与交付寿命。载荷仍由来源直接写入接收者交出的 FD。

use std::os::fd::OwnedFd;

use wayland_wire::{
    protocol::{
        Interface, wl_data_device,
        wl_data_device_manager::dnd_action::{ASK, COPY, MOVE},
        wl_data_offer, wl_data_source,
    },
    wire::{Fixed, ObjectId},
};

use super::{Client, ClientId, Server, client::Fail, clipboard::Offer};

pub(super) struct Start {
    pub source: Option<ObjectId>,
    pub origin: ObjectId,
    pub icon: Option<ObjectId>,
    pub serial: u32,
    pub mimes: Vec<String>,
    pub actions: u32,
}

pub(super) enum Request {
    Start(Start),
    Accept {
        offer: ObjectId,
        serial: u32,
        mime: Option<String>,
    },
    Actions {
        offer: ObjectId,
        actions: u32,
        preferred: u32,
    },
    Receive {
        offer: ObjectId,
        mime: String,
        fd: OwnedFd,
    },
    Finish(ObjectId),
    Destroy(ObjectId),
}

type Surface = (ClientId, ObjectId);

pub(super) struct Drag {
    owner: ClientId,
    start: Start,
    target: Option<Destination>,
    dropped_at: u32,
}

struct Destination {
    surface: Surface,
    device: ObjectId,
    version: u32,
    offer: Option<ObjectId>,
    serial: u32,
    mime: Option<String>,
    action: u32,
}

impl Destination {
    fn matches(&self, client: ClientId, offer: ObjectId) -> bool {
        self.surface.0 == client && self.offer == Some(offer)
    }
}

/// 一次 drop 后仍可接收 FD，直到 finish/destroy 或连接退出。限制积压，避免目标无限保有来源。
const MAX_PENDING_DROPS: usize = 16;
/// 目标失去响应时释放其保留的来源，不能耗尽随后所有拖放的槽位。
const DROP_TIMEOUT_MS: u32 = 60_000;

impl Server {
    /// 当前 seat 正在路由拖放，常规 wl_pointer motion/buttons 暂停。
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// 可选的拖动图标，由 scene 使用普通 surface commit 绘制。
    pub fn drag_icon(&self) -> Option<Surface> {
        let drag = self.drag.as_ref()?;
        Some((drag.owner, drag.start.icon?))
    }

    pub(super) fn drag_request(
        &mut self,
        owner: ClientId,
        request: Request,
        grab: Option<(ClientId, ObjectId, u32)>,
    ) {
        match request {
            Request::Start(start) => {
                let valid = grab == Some((owner, start.origin, start.serial))
                    && !self.dragging()
                    && self.pending_drops.len() < MAX_PENDING_DROPS
                    && self.drag_objects_alive(owner, &start);
                let drag = Drag {
                    owner,
                    start,
                    target: None,
                    dropped_at: 0,
                };
                if valid {
                    self.drag = Some(drag);
                } else {
                    self.drag_source(&drag, wl_data_source::Event::Cancelled);
                }
            }
            Request::Accept {
                offer,
                serial,
                mime,
            } => {
                let Some(mut drag) = self.drag.take() else {
                    return;
                };
                if let Some(target) = drag.target.as_mut()
                    && target.matches(owner, offer)
                    && target.serial == serial
                {
                    target.mime = mime.filter(|mime| drag.start.mimes.contains(mime));
                    let mime_type = target.mime.clone();
                    self.drag_source(&drag, wl_data_source::Event::Target { mime_type });
                }
                self.drag = Some(drag);
            }
            Request::Actions {
                offer,
                actions,
                preferred,
            } => {
                if let Some(mut drag) = self.drag.take() {
                    if let Some(target) = drag.target.as_mut()
                        && target.matches(owner, offer)
                    {
                        let compatible = drag.start.actions & actions;
                        let action = if compatible & preferred != 0 {
                            preferred
                        } else {
                            [COPY, MOVE, ASK]
                                .into_iter()
                                .find(|bit| compatible & bit != 0)
                                .unwrap_or(0)
                        };
                        if target.action != action {
                            target.action = action;
                            self.with_client(owner, |client| {
                                let _ = wl_data_offer::Event::Action { dnd_action: action }
                                    .encode(offer, &mut client.writer);
                            });
                            self.drag_source(
                                &drag,
                                wl_data_source::Event::Action { dnd_action: action },
                            );
                        }
                    }
                    self.drag = Some(drag);
                }
                // ASK 已落下，目的方可从来源动作中选择最后的 COPY 或 MOVE。
                if let Some(drag) = self.pending_drops.iter_mut().find(|drag| {
                    drag.target
                        .as_ref()
                        .is_some_and(|target| target.matches(owner, offer))
                }) {
                    let target = drag.target.as_mut().expect("matched destination");
                    if target.action == ASK
                        && matches!(preferred, COPY | MOVE)
                        && drag.start.actions & actions & preferred != 0
                    {
                        target.action = preferred;
                        let source = drag.start.source.map(|source| (drag.owner, source));
                        if let Some((owner, source)) = source {
                            self.with_client(owner, |client| {
                                client.drag_source(
                                    source,
                                    wl_data_source::Event::Action {
                                        dnd_action: preferred,
                                    },
                                )
                            });
                        }
                    }
                }
            }
            Request::Receive { offer, mime, fd } => self.drag_receive(owner, offer, mime, fd),
            Request::Finish(offer) => {
                if let Some(index) = self.pending_drops.iter().position(|drag| {
                    drag.target.as_ref().is_some_and(|target| {
                        target.matches(owner, offer)
                            && target.mime.is_some()
                            && matches!(target.action, COPY | MOVE)
                    })
                }) {
                    let drag = self.pending_drops.swap_remove(index);
                    self.drag_source(&drag, wl_data_source::Event::DndFinished);
                } else {
                    self.with_client(owner, |client| {
                        let _ = client.fatal(Fail::new(
                            offer,
                            wl_data_offer::Error::InvalidFinish as u32,
                            "offer has no accepted drop",
                        ));
                    });
                }
            }
            Request::Destroy(offer) => {
                if let Some(index) = self.pending_drops.iter().position(|drag| {
                    drag.target
                        .as_ref()
                        .is_some_and(|target| target.matches(owner, offer))
                }) {
                    let drag = self.pending_drops.swap_remove(index);
                    let legacy = drag
                        .target
                        .as_ref()
                        .is_some_and(|target| target.version < 3);
                    self.drag_source(
                        &drag,
                        if legacy {
                            wl_data_source::Event::DndFinished
                        } else {
                            wl_data_source::Event::Cancelled
                        },
                    );
                }
                if let Some(target) = self.drag.as_mut().and_then(|drag| drag.target.as_mut())
                    && target.matches(owner, offer)
                {
                    target.offer = None;
                    target.mime = None;
                    target.action = 0;
                }
            }
        }
    }

    fn drag_receive(&mut self, owner: ClientId, offer: ObjectId, mime: String, fd: OwnedFd) {
        let source = self
            .drag
            .iter()
            .chain(&self.pending_drops)
            .find_map(|drag| {
                if drag
                    .target
                    .as_ref()
                    .is_some_and(|target| target.matches(owner, offer))
                    && drag.start.mimes.contains(&mime)
                {
                    drag.start.source.map(|source| (drag.owner, source))
                } else {
                    None
                }
            });
        if let Some((client, source)) = source {
            self.with_client(client, |client| client.source_send(source, mime, fd));
        }
    }

    /// 更新拖放指针目标，坐标使用 wl_surface 的逻辑像素。
    pub fn drag_motion(
        &mut self,
        hit: Option<(Surface, Fixed, Fixed)>,
        serial: u32,
        time: u32,
        moved: bool,
    ) {
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        let hit =
            hit.filter(|(surface, _, _)| drag.start.source.is_some() || surface.0 == drag.owner);
        let surface = hit.map(|(surface, _, _)| surface);
        if drag.target.as_ref().map(|target| target.surface) != surface {
            if let Some(old) = drag.target.take() {
                self.drag_device(&old, wl_data_device::Event::Leave);
                self.drag_source(&drag, wl_data_source::Event::Target { mime_type: None });
                self.drag_source(&drag, wl_data_source::Event::Action { dnd_action: 0 });
            }
            if let Some((surface, x, y)) = hit {
                self.with_client(surface.0, |client| {
                    drag.target = client
                        .drag_enter(surface, &drag.start, serial, x, y)
                        .ok()
                        .flatten();
                });
            }
        } else if moved && let (Some(target), Some((_, x, y))) = (&drag.target, hit) {
            self.drag_device(target, wl_data_device::Event::Motion { time, x, y });
        }
        self.drag = Some(drag);
    }

    /// 结束指针抓取。支持目标在 drop 后再读取数据，未接受的操作通知来源取消。
    pub fn drop_drag(&mut self, time: u32) {
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        self.drag_source(&drag, wl_data_source::Event::DndDropPerformed);
        if let Some(target) = &drag.target {
            if drag.start.source.is_none()
                || (target.action != 0 && (target.version < 3 || target.mime.is_some()))
            {
                self.drag_device(target, wl_data_device::Event::Drop);
                if drag.start.source.is_some() {
                    drag.dropped_at = time;
                    self.pending_drops.push(drag);
                }
                return;
            }
            self.drag_device(target, wl_data_device::Event::Leave);
        }
        self.drag_source(&drag, wl_data_source::Event::Cancelled);
    }

    /// 撤销当前指针拖放（安全注意、源销毁等），已交付的 drop 仍独立完成。
    pub fn cancel_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            if let Some(target) = &drag.target {
                self.drag_device(target, wl_data_device::Event::Leave);
            }
            self.drag_source(&drag, wl_data_source::Event::Cancelled);
        }
    }

    pub(super) fn prune_drags(&mut self, time: u32) {
        if self
            .drag
            .as_ref()
            .is_some_and(|drag| !self.drag_objects_alive(drag.owner, &drag.start))
        {
            self.cancel_drag();
        }
        let mut index = 0;
        while index < self.pending_drops.len() {
            let drag = &self.pending_drops[index];
            let target = drag.target.as_ref().expect("pending destination");
            if time.wrapping_sub(drag.dropped_at) >= DROP_TIMEOUT_MS
                || !self.drag_source_alive(drag.owner, drag.start.source)
                || !target.offer.is_some_and(|offer| {
                    self.client_has(target.surface.0, offer, Interface::WlDataOffer)
                })
            {
                let drag = self.pending_drops.swap_remove(index);
                self.drag_source(&drag, wl_data_source::Event::Cancelled);
            } else {
                index += 1;
            }
        }
    }

    fn drag_objects_alive(&self, owner: ClientId, start: &Start) -> bool {
        self.client_has(owner, start.origin, Interface::WlSurface)
            && self.drag_source_alive(owner, start.source)
    }

    fn drag_source_alive(&self, owner: ClientId, source: Option<ObjectId>) -> bool {
        source.is_none_or(|source| self.client_has(owner, source, Interface::WlDataSource))
    }

    fn drag_source(&mut self, drag: &Drag, event: wl_data_source::Event) {
        if let Some(source) = drag.start.source {
            self.with_client(drag.owner, |client| client.drag_source(source, event));
        }
    }

    fn drag_device(&mut self, target: &Destination, event: wl_data_device::Event) {
        self.with_client(target.surface.0, |client| {
            if client.has_object(target.device, Interface::WlDataDevice) {
                let _ = event.encode(target.device, &mut client.writer);
            }
        });
    }
}

impl Client {
    pub(super) fn has_object(&self, object: ObjectId, interface: Interface) -> bool {
        self.objects
            .get(&object.get())
            .is_some_and(|entry| entry.interface == interface)
    }

    fn drag_source(&mut self, source: ObjectId, event: wl_data_source::Event) {
        if self.objects.get(&source.get()).is_some_and(|entry| {
            entry.interface == Interface::WlDataSource && entry.version >= event.since()
        }) {
            let _ = event.encode(source, &mut self.writer);
        }
    }

    fn drag_enter(
        &mut self,
        surface: Surface,
        start: &Start,
        serial: u32,
        x: Fixed,
        y: Fixed,
    ) -> Result<Option<Destination>, Fail> {
        let Some((device, version)) = self
            .objects
            .iter()
            .filter(|(_, entry)| entry.interface == Interface::WlDataDevice)
            .filter_map(|(id, entry)| Some((ObjectId::new(*id)?, entry.version)))
            .max_by_key(|(id, version)| (*version, id.get()))
        else {
            return Ok(None);
        };
        let mut offer = None;
        let mut action = 0;
        if start.source.is_some() {
            let id = self.new_offer(device, version, Offer::Drag, &start.mimes)?;
            offer = Some(id);
            if version >= 3 {
                wl_data_offer::Event::SourceActions {
                    source_actions: start.actions,
                }
                .encode(id, &mut self.writer)?;
            } else {
                action = start.actions & COPY;
            }
        }
        wl_data_device::Event::Enter {
            serial,
            surface: surface.1,
            x,
            y,
            id: offer,
        }
        .encode(device, &mut self.writer)?;
        Ok(Some(Destination {
            surface,
            device,
            version,
            offer,
            serial,
            mime: None,
            action,
        }))
    }
}
