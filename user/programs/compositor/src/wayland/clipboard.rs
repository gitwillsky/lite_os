//! 剪贴板（`wl_data_device` 的选择部分）：来源由持有者提供，合成器只保存它声明的 MIME 类型并把读取请求转交给持有者。
//!
//! 内容从不经过合成器：接收方交来一个管道的写端，合成器原样转给持有者，由持有者写入。
//! 拖放的抓取和目的方交接由同一 seat 上的 `drag` 模块管理。

use std::os::fd::OwnedFd;

use wayland_wire::{
    protocol::{Interface, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source},
    wire::ObjectId,
};

use super::client::{Client, Data, Entry, Fail};
use super::drag::{Request, Start};
use super::server::ClientId;

use wl_data_device_manager::dnd_action::{ASK, COPY, MOVE};
const ACTIONS: u32 = COPY | MOVE | ASK;

/// 一个 `wl_data_source` 声明可提供的 MIME 类型。
#[derive(Default)]
pub struct DataSource {
    mimes: Vec<String>,
    actions: Option<u32>,
    used: bool,
}

#[derive(Clone, Copy)]
pub(super) enum Offer {
    Selection(ClientId, ObjectId),
    Drag,
}

/// 客户端想让某个来源成为当前选择（`None` 表示清空）。
pub struct SelectionSet {
    pub source: Option<ObjectId>,
    pub mimes: Vec<String>,
    pub serial: u32,
}

/// 接收方想读取当前选择的某个类型；`fd` 是管道写端。
pub(super) struct ReceiveRequest {
    pub source: (ClientId, ObjectId),
    pub mime: String,
    pub fd: OwnedFd,
}

impl Client {
    pub(super) fn data_device_manager(
        &mut self,
        object: ObjectId,
        version: u32,
        request: wl_data_device_manager::Request,
    ) -> Result<(), Fail> {
        let _ = object;
        match request {
            wl_data_device_manager::Request::CreateDataSource { id } => self.create(
                id,
                Interface::WlDataSource,
                version,
                Data::DataSource(DataSource::default()),
            ),
            wl_data_device_manager::Request::GetDataDevice { id, seat } => {
                if !self.has_object(seat, Interface::WlSeat) {
                    return Err(Fail::invalid_object("data device seat is not bound"));
                }
                self.create(id, Interface::WlDataDevice, version, Data::None)
            }
        }
    }

    pub(super) fn data_source(
        &mut self,
        object: ObjectId,
        request: wl_data_source::Request,
    ) -> Result<(), Fail> {
        let Some(Entry {
            data: Data::DataSource(source),
            ..
        }) = self.objects.get_mut(&object.get())
        else {
            return Err(Fail::invalid_object("missing data source"));
        };
        match request {
            wl_data_source::Request::Offer { mime_type } if !source.used => {
                if source.mimes.len() >= 128 {
                    return Err(Fail::invalid_method(object, "too many MIME types"));
                }
                if !source.mimes.contains(&mime_type) {
                    source.mimes.push(mime_type);
                }
            }
            wl_data_source::Request::SetActions { dnd_actions } => {
                let actions = dnd_actions;
                if actions & !ACTIONS != 0 {
                    return Err(Fail::new(
                        object,
                        wl_data_source::Error::InvalidActionMask as u32,
                        "invalid drag action mask",
                    ));
                }
                if source.used || source.actions.is_some() {
                    return Err(Fail::new(
                        object,
                        wl_data_source::Error::InvalidSource as u32,
                        "drag actions already set or source used",
                    ));
                }
                source.actions = Some(actions);
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn data_device(
        &mut self,
        object: ObjectId,
        request: wl_data_device::Request,
    ) -> Result<(), Fail> {
        match request {
            wl_data_device::Request::SetSelection { source, serial } => {
                let mimes = match source {
                    Some(source) => self.claim_source(object, source, false)?.0,
                    None => Vec::new(),
                };
                self.selection_set = Some(SelectionSet {
                    source,
                    mimes,
                    serial,
                });
            }
            wl_data_device::Request::StartDrag {
                source,
                origin,
                icon,
                serial,
            } => {
                self.surface_mut(origin)?;
                if let Some(icon) = icon {
                    let surface = self.surface_mut(icon)?;
                    if surface.xdg.is_some()
                        || surface.layer.is_some()
                        || surface.subsurface_role
                        || surface.cursor
                    {
                        return Err(Fail::new(
                            object,
                            wl_data_device::Error::Role as u32,
                            "drag icon already has a role",
                        ));
                    }
                    surface.drag_icon = true;
                }
                let (mimes, actions) = match source {
                    Some(source) => self.claim_source(object, source, true)?,
                    None => (Vec::new(), COPY),
                };
                if self.drag_requests.len() >= 256 {
                    return Err(Fail::invalid_method(object, "too many drag requests"));
                }
                self.drag_requests.push(Request::Start(Start {
                    source,
                    origin,
                    icon,
                    serial,
                    mimes,
                    actions,
                }));
            }
            _ => {}
        }
        Ok(())
    }

    fn claim_source(
        &mut self,
        device: ObjectId,
        id: ObjectId,
        drag: bool,
    ) -> Result<(Vec<String>, u32), Fail> {
        let Some(Entry {
            data: Data::DataSource(source),
            ..
        }) = self.objects.get_mut(&id.get())
        else {
            return Err(Fail::invalid_object("missing data source"));
        };
        if source.used {
            return Err(Fail::new(
                device,
                wl_data_device::Error::UsedSource as u32,
                "data source already used",
            ));
        }
        if !drag && source.actions.is_some() {
            return Err(Fail::new(
                id,
                wl_data_source::Error::InvalidSource as u32,
                "drag source used as selection",
            ));
        }
        source.used = true;
        Ok((source.mimes.clone(), source.actions.unwrap_or(COPY)))
    }

    pub(super) fn data_offer(
        &mut self,
        object: ObjectId,
        request: wl_data_offer::Request,
    ) -> Result<(), Fail> {
        let Some(Entry {
            data: Data::Offer(kind),
            ..
        }) = self.objects.get(&object.get())
        else {
            return Err(Fail::invalid_object("missing data offer"));
        };
        let kind = *kind;
        let request = match request {
            wl_data_offer::Request::Receive { mime_type, fd } => match kind {
                Offer::Selection(client, source) => {
                    if self.receives.len() >= 256 {
                        return Err(Fail::invalid_method(object, "too many receive requests"));
                    }
                    self.receives.push(ReceiveRequest {
                        source: (client, source),
                        mime: mime_type,
                        fd,
                    });
                    return Ok(());
                }
                Offer::Drag => Request::Receive {
                    offer: object,
                    mime: mime_type,
                    fd,
                },
            },
            wl_data_offer::Request::Accept { serial, mime_type } if matches!(kind, Offer::Drag) => {
                Request::Accept {
                    offer: object,
                    serial,
                    mime: mime_type,
                }
            }
            wl_data_offer::Request::SetActions {
                dnd_actions,
                preferred_action,
            } => {
                let (actions, preferred) = (dnd_actions, preferred_action);
                if !matches!(kind, Offer::Drag) {
                    return Err(Fail::new(
                        object,
                        wl_data_offer::Error::InvalidOffer as u32,
                        "selection has no drag actions",
                    ));
                }
                if actions & !ACTIONS != 0 {
                    return Err(Fail::new(
                        object,
                        wl_data_offer::Error::InvalidActionMask as u32,
                        "invalid drag action mask",
                    ));
                }
                if preferred.count_ones() > 1 || preferred & !actions != 0 {
                    return Err(Fail::new(
                        object,
                        wl_data_offer::Error::InvalidAction as u32,
                        "invalid preferred action",
                    ));
                }
                Request::Actions {
                    offer: object,
                    actions,
                    preferred,
                }
            }
            wl_data_offer::Request::Finish => {
                if !matches!(kind, Offer::Drag) {
                    return Err(Fail::new(
                        object,
                        wl_data_offer::Error::InvalidFinish as u32,
                        "selection cannot be finished",
                    ));
                }
                Request::Finish(object)
            }
            wl_data_offer::Request::Destroy if matches!(kind, Offer::Drag) => {
                Request::Destroy(object)
            }
            _ => return Ok(()),
        };
        if self.drag_requests.len() >= 256 {
            return Err(Fail::invalid_method(object, "too many drag requests"));
        }
        self.drag_requests.push(request);
        Ok(())
    }

    pub(super) fn new_offer(
        &mut self,
        device: ObjectId,
        version: u32,
        kind: Offer,
        mimes: &[String],
    ) -> Result<ObjectId, Fail> {
        let offer = ObjectId::new(self.next_server_object).expect("server ids start above zero");
        self.next_server_object += 1;
        self.objects.insert(
            offer.get(),
            Entry {
                interface: Interface::WlDataOffer,
                version,
                data: Data::Offer(kind),
            },
        );
        wl_data_device::Event::DataOffer { id: offer }.encode(device, &mut self.writer)?;
        for mime in mimes {
            wl_data_offer::Event::Offer {
                mime_type: mime.clone(),
            }
            .encode(offer, &mut self.writer)?;
        }
        Ok(offer)
    }

    /// 向本连接的每个 `wl_data_device` 宣告当前选择（`None` 为空）：先建一个服务端 offer，列出类型，再发 `selection`。
    pub(super) fn send_selection(
        &mut self,
        selection: Option<(ClientId, ObjectId, &[String])>,
    ) -> Result<(), Fail> {
        let devices: Vec<(ObjectId, u32)> = self
            .objects
            .iter()
            .filter(|(_, entry)| entry.interface == Interface::WlDataDevice)
            .filter_map(|(id, entry)| Some((ObjectId::new(*id)?, entry.version)))
            .collect();
        for (device, version) in devices {
            let Some((owner, source, mimes)) = selection else {
                wl_data_device::Event::Selection { id: None }.encode(device, &mut self.writer)?;
                continue;
            };
            let offer = self.new_offer(device, version, Offer::Selection(owner, source), mimes)?;
            wl_data_device::Event::Selection { id: Some(offer) }
                .encode(device, &mut self.writer)?;
        }
        Ok(())
    }

    /// 通知来源它不再是选择（被别人替换、清空或持有者失焦之外的撤销）。
    pub(super) fn source_cancelled(&mut self, source: ObjectId) {
        if self.has_object(source, Interface::WlDataSource) {
            let _ = wl_data_source::Event::Cancelled.encode(source, &mut self.writer);
        }
    }

    /// 把接收方的管道写端转给来源，请它写入 `mime` 的内容。
    pub(super) fn source_send(&mut self, source: ObjectId, mime: String, fd: OwnedFd) {
        if !self.has_object(source, Interface::WlDataSource) {
            return;
        }
        let offset = self.writer.bytes().len();
        let event = wl_data_source::Event::Send {
            mime_type: mime,
            fd,
        };
        if event.encode(source, &mut self.writer).is_err() {
            return;
        }
        // 描述符随消息由本连接持有到发送完成（与 keymap 相同）。
        self.writer.consume(0, 1);
        let wl_data_source::Event::Send { fd, .. } = event else {
            unreachable!("constructed as a send event above");
        };
        self.outgoing_fds.push_back((offset, fd));
    }
}
