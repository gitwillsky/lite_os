//! 剪贴板（`wl_data_device` 的选择部分）：来源由持有者提供，合成器只保存它声明的 MIME 类型并把读取请求转交给持有者。
//!
//! 内容从不经过合成器：接收方交来一个管道的写端，合成器原样转给持有者，由持有者写入。
//! 没有拖放：`start_drag` 立即以 `cancelled` 回绝来源。

use std::os::fd::OwnedFd;

use wayland_wire::{
    protocol::{Interface, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source},
    wire::ObjectId,
};

use super::client::{Client, Data, Entry, Fail};

/// 一个 `wl_data_source` 声明可提供的 MIME 类型。
#[derive(Default)]
pub struct DataSource {
    mimes: Vec<String>,
}

/// 客户端想让某个来源成为当前选择（`None` 表示清空）。
pub struct SelectionSet {
    pub source: Option<ObjectId>,
    pub mimes: Vec<String>,
}

/// 接收方想读取当前选择的某个类型；`fd` 是管道写端。
pub struct ReceiveRequest {
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
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                self.create(id, Interface::WlDataDevice, version, Data::None)
            }
        }
    }

    pub(super) fn data_source(
        &mut self,
        object: ObjectId,
        request: wl_data_source::Request,
    ) -> Result<(), Fail> {
        if let wl_data_source::Request::Offer { mime_type } = request
            && let Some(Entry {
                data: Data::DataSource(source),
                ..
            }) = self.objects.get_mut(&object.get())
        {
            source.mimes.push(mime_type);
        }
        Ok(())
    }

    pub(super) fn data_device(&mut self, request: wl_data_device::Request) -> Result<(), Fail> {
        match request {
            wl_data_device::Request::SetSelection { source, .. } => {
                let mimes = source
                    .and_then(|source| match self.objects.get(&source.get()) {
                        Some(Entry {
                            data: Data::DataSource(data),
                            ..
                        }) => Some(data.mimes.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                self.selection_set = Some(SelectionSet { source, mimes });
            }
            wl_data_device::Request::StartDrag {
                source: Some(source),
                ..
            } => {
                wl_data_source::Event::Cancelled.encode(source, &mut self.writer)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn data_offer(&mut self, request: wl_data_offer::Request) -> Result<(), Fail> {
        if let wl_data_offer::Request::Receive { mime_type, fd } = request {
            self.receives.push(ReceiveRequest {
                mime: mime_type,
                fd,
            });
        }
        Ok(())
    }

    /// 向本连接的每个 `wl_data_device` 宣告当前选择（`None` 为空）：先建一个服务端 offer，列出类型，再发 `selection`。
    pub(super) fn send_selection(&mut self, mimes: Option<&[String]>) -> Result<(), Fail> {
        let devices: Vec<(ObjectId, u32)> = self
            .objects
            .iter()
            .filter(|(_, entry)| entry.interface == Interface::WlDataDevice)
            .filter_map(|(id, entry)| Some((ObjectId::new(*id)?, entry.version)))
            .collect();
        for (device, version) in devices {
            let Some(mimes) = mimes else {
                wl_data_device::Event::Selection { id: None }.encode(device, &mut self.writer)?;
                continue;
            };
            let offer =
                ObjectId::new(self.next_server_object).expect("server ids start above zero");
            self.next_server_object += 1;
            self.objects.insert(
                offer.get(),
                Entry {
                    interface: Interface::WlDataOffer,
                    version,
                    data: Data::None,
                },
            );
            wl_data_device::Event::DataOffer { id: offer }.encode(device, &mut self.writer)?;
            for mime in mimes {
                wl_data_offer::Event::Offer {
                    mime_type: mime.clone(),
                }
                .encode(offer, &mut self.writer)?;
            }
            wl_data_device::Event::Selection { id: Some(offer) }
                .encode(device, &mut self.writer)?;
        }
        Ok(())
    }

    /// 通知来源它不再是选择（被别人替换、清空或持有者失焦之外的撤销）。
    pub(super) fn source_cancelled(&mut self, source: ObjectId) {
        let _ = wl_data_source::Event::Cancelled.encode(source, &mut self.writer);
    }

    /// 把接收方的管道写端转给来源，请它写入 `mime` 的内容。
    pub(super) fn source_send(&mut self, source: ObjectId, mime: String, fd: OwnedFd) {
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
