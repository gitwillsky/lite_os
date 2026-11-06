//! 剪贴板客户端：经 `wl_data_device` 协商选择的 MIME 类型。
//!
//! 内容经管道直接在两个程序之间传递，合成器只转交管道的写端。

use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    sync::Arc,
    time::Duration,
};

use wayland_wire::{
    protocol::{
        AnyEvent, Interface, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    },
    wire::{ObjectId, Writer},
};

use super::{
    Display, EventKind,
    file_drag::{Hover, Offer, Reading},
};

/// 按偏好排列的文本类型。
const TEXT_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];
/// 读取对方内容的最长等待：对方是另一个进程，卡住不能拖住本程序界面。
const READ_TIMEOUT: Duration = Duration::from_millis(800);
/// 读取的内容上限。
const MAX_TEXT: u64 = 1 << 20;

#[derive(Default)]
pub(super) struct Clipboard {
    pub(super) manager: Option<ObjectId>,
    pub(super) device: Option<ObjectId>,
    /// 已收到、还没成为选择的 offer 与它们声明的类型。
    pub(super) offers: HashMap<u32, Offer>,
    pub(super) drag_sources: HashMap<ObjectId, Arc<str>>,
    pub(super) hover: Option<Hover>,
    pub(super) reading: Option<Reading>,
    /// 当前选择对应的 offer；`None` 表示选择为空。
    selection: Option<ObjectId>,
    /// 本进程提供的来源与内容；被别人替换（`cancelled`）后清空。
    own: Option<SelectionSource>,
    /// 本地 offer 生命周期的身份，零表示尚无选择。异步粘贴用它拒绝覆盖更新后的选择。
    revision: u64,
    outgoing: Vec<Transfer>,
    /// 最近一次键盘或按钮事件的序号；`set_selection` 必须带它。
    pub(super) serial: u32,
}

struct SelectionSource {
    id: ObjectId,
    payloads: Vec<(String, Arc<str>)>,
}

struct Transfer {
    file: File,
    text: Arc<str>,
    offset: usize,
}

impl Clipboard {
    pub(super) fn write_sources(&self) -> impl Iterator<Item = linux_uapi::unix::PollFd<'_>> {
        self.outgoing.iter().map(|transfer| {
            linux_uapi::unix::PollFd::new(
                transfer.file.as_fd(),
                linux_uapi::unix::PollEvents::WRITE,
            )
        })
    }
    pub(super) fn progress_writes(&mut self) {
        // 每轮每个传输最多写 64 KiB，来源最多 8 个；慢消费者不创建线程，也不阻塞协议循环。
        self.outgoing.retain_mut(|transfer| {
            if transfer.offset == transfer.text.len() {
                return false;
            }
            let end = transfer.text.len().min(transfer.offset + 65536);
            match transfer
                .file
                .write(&transfer.text.as_bytes()[transfer.offset..end])
            {
                Ok(0) => false,
                Ok(bytes) => {
                    transfer.offset += bytes;
                    transfer.offset != transfer.text.len()
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    true
                }
                Err(_) => false,
            }
        });
    }
    /// 绑定了管理器、还没有设备和选择的初始状态。
    pub(super) fn with_manager(manager: Option<ObjectId>) -> Self {
        Self {
            manager,
            ..Self::default()
        }
    }
}

impl Display {
    /// 为 seat 创建 `wl_data_device`；compositor 没有剪贴板全局对象时什么也不做。
    pub(super) fn bind_data_device(&mut self) -> io::Result<()> {
        let Some(manager) = self.clipboard.manager else {
            return Ok(());
        };
        let device = self.transport.allocate(Interface::WlDataDevice)?;
        wl_data_device_manager::Request::GetDataDevice {
            id: device,
            seat: self.seat,
        }
        .encode(manager, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        self.clipboard.device = Some(device);
        Ok(())
    }

    /// 把 `text` 放进剪贴板。没有剪贴板协议或设备时返回 `Ok`，什么也不做。
    ///
    /// # Errors
    ///
    /// 写 socket 失败。
    pub fn set_clipboard(&mut self, text: &str) -> io::Result<()> {
        let text = Arc::from(text);
        let formats = TEXT_TYPES
            .map(|mime| (mime.to_owned(), Arc::clone(&text)))
            .into();
        self.set_selection(formats, None).map(drop)
    }

    /// 提供 MIME 数据。`expected` 限定替换仍在焦点内的同一次选择；空列表清空选择。
    /// 返回是否已发送请求。格式或大小非法、或 socket 失败时返回错误。
    pub fn set_selection(
        &mut self,
        formats: Vec<(String, Arc<str>)>,
        expected: Option<u64>,
    ) -> io::Result<bool> {
        if formats.len() > 8
            || formats.iter().any(|(mime, data)| {
                mime.is_empty()
                    || mime.len() > 255
                    || mime.contains('\0')
                    || data.len() > MAX_TEXT as usize
            })
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let (Some(manager), Some(device)) = (self.clipboard.manager, self.clipboard.device) else {
            return Ok(false);
        };
        if expected.is_some_and(|revision| {
            self.keyboard_focus().is_none() || revision != self.clipboard.revision
        }) {
            return Ok(false);
        }
        let next = if formats.is_empty() {
            None
        } else {
            let source = self.transport.allocate(Interface::WlDataSource)?;
            let writer = &mut self.transport.writer;
            wl_data_device_manager::Request::CreateDataSource { id: source }
                .encode(manager, writer)
                .map_err(io::Error::other)?;
            for (mime, _) in &formats {
                wl_data_source::Request::Offer {
                    mime_type: mime.clone(),
                }
                .encode(source, writer)
                .map_err(io::Error::other)?;
            }
            Some(SelectionSource {
                id: source,
                payloads: formats,
            })
        };
        let writer = &mut self.transport.writer;
        wl_data_device::Request::SetSelection {
            source: next.as_ref().map(|source| source.id),
            serial: self.clipboard.serial,
        }
        .encode(device, writer)
        .map_err(io::Error::other)?;
        if let Some(old) = std::mem::replace(&mut self.clipboard.own, next) {
            wl_data_source::Request::Destroy
                .encode(old.id, writer)
                .map_err(io::Error::other)?;
        }
        self.clear_selection_offer()?;
        self.selection_changed();
        self.transport.flush()?;
        Ok(true)
    }

    fn selection_changed(&mut self) {
        self.clipboard.revision = self.clipboard.revision.wrapping_add(1).max(1);
        self.emit(self.keyboard_focus(), EventKind::ClipboardChanged);
    }

    fn clear_selection_offer(&mut self) -> io::Result<()> {
        if let Some(offer) = self.clipboard.selection.take() {
            self.destroy_offer(offer)?;
        }
        Ok(())
    }

    /// 指定 MIME 的当前选择身份；没有该格式时返回 None。
    pub fn selection_revision(&self, mime: &str) -> Option<u64> {
        self.keyboard_focus()?;
        let available = if let Some(source) = &self.clipboard.own {
            source.payloads.iter().any(|(kind, _)| kind == mime)
        } else {
            self.clipboard
                .selection
                .and_then(|offer| self.clipboard.offers.get(&offer.get()))
                .is_some_and(|offer| offer.mimes.iter().any(|kind| kind == mime))
        };
        available.then_some(self.clipboard.revision)
    }

    /// 请求纯文本的非阻塞读端；无文本 offer 返回 None。
    /// 管道、fd 发送或队列满返回错误；调用者负责限制容量与超时。
    pub fn receive_clipboard(&mut self) -> io::Result<Option<UnixStream>> {
        let Some(mime) = TEXT_TYPES
            .into_iter()
            .find(|mime| self.selection_revision(mime).is_some())
        else {
            return Ok(None);
        };
        self.receive_selection(mime)
    }

    /// 请求指定 MIME 的非阻塞读端；没有该格式时返回 None。
    /// 管道、fd 发送或队列满返回错误；调用者负责限制容量与超时。
    pub fn receive_selection(&mut self, mime: &str) -> io::Result<Option<UnixStream>> {
        if self.selection_revision(mime).is_none() {
            return Ok(None);
        }
        let (reader, writer_end) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        if let Some(source) = &self.clipboard.own {
            let text = source
                .payloads
                .iter()
                .find(|(kind, _)| kind == mime)
                .expect("available clipboard MIME")
                .1
                .clone();
            if self.clipboard.outgoing.len() >= 8 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            writer_end.set_nonblocking(true)?;
            self.clipboard.outgoing.push(Transfer {
                file: File::from(std::os::fd::OwnedFd::from(writer_end)),
                text,
                offset: 0,
            });
            return Ok(Some(reader));
        }
        let offer = self.clipboard.selection.expect("available clipboard offer");
        let mut request = Writer::new();
        wl_data_offer::Request::Receive {
            mime_type: mime.to_owned(),
            fd: writer_end.try_clone()?.into(),
        }
        .encode(offer, &mut request)
        .map_err(io::Error::other)?;
        self.transport
            .send_fd(request.bytes(), writer_end.as_fd())?;
        Ok(Some(reader))
    }

    /// 读出剪贴板里的文本；没有文本、对方没有响应或超时返回 `Ok(None)`。
    ///
    /// # Errors
    ///
    /// 建立管道或写 socket 失败。
    pub fn clipboard_text(&mut self) -> io::Result<Option<String>> {
        if let Some(source) = &self.clipboard.own {
            return Ok(TEXT_TYPES.into_iter().find_map(|mime| {
                source
                    .payloads
                    .iter()
                    .find(|(kind, _)| kind == mime)
                    .map(|(_, text)| text.to_string())
            }));
        }
        let Some(offer) = self.clipboard.selection else {
            return Ok(None);
        };
        let Some(mime) = self.clipboard.offers.get(&offer.get()).and_then(|mimes| {
            TEXT_TYPES
                .iter()
                .find(|wanted| mimes.mimes.iter().any(|m| m == *wanted))
        }) else {
            return Ok(None);
        };
        let (reader, writer_end) = UnixStream::pair()?;
        // 请求带着管道写端发出；发完立刻关闭本地的写端，持有者写完关闭后读端才会得到 EOF。
        let mut request = Writer::new();
        wl_data_offer::Request::Receive {
            mime_type: (*mime).to_owned(),
            fd: writer_end.try_clone()?.into(),
        }
        .encode(offer, &mut request)
        .map_err(io::Error::other)?;
        self.transport
            .send_fd(request.bytes(), writer_end.as_fd())?;
        drop(writer_end);
        reader.set_read_timeout(Some(READ_TIMEOUT))?;
        let mut bytes = Vec::new();
        match reader.take(MAX_TEXT).read_to_end(&mut bytes) {
            Ok(_) => {}
            // 超时：用已读到的部分，而不是整个丢弃。
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        Ok((!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// 处理剪贴板相关事件；返回是否已消费。
    pub(super) fn clipboard_event(
        &mut self,
        object: ObjectId,
        event: &AnyEvent,
    ) -> io::Result<bool> {
        match event {
            AnyEvent::WlDataDevice(wl_data_device::Event::DataOffer { id }) => {
                self.transport.server_object(*id, Interface::WlDataOffer)?;
                self.clipboard.offers.insert(id.get(), Offer::default());
            }
            AnyEvent::WlDataOffer(wl_data_offer::Event::Offer { mime_type }) => {
                if let Some(mimes) = self.clipboard.offers.get_mut(&object.get()) {
                    mimes.mimes.push(mime_type.clone());
                }
            }
            AnyEvent::WlDataDevice(wl_data_device::Event::Selection { id }) => {
                // 旧选择的 offer 不再需要，按协议由客户端销毁。
                if let Some(old) = self.clipboard.selection.take()
                    && Some(old) != *id
                {
                    self.destroy_offer(old)?;
                }
                self.clipboard.selection = *id;
                self.selection_changed();
            }
            AnyEvent::WlDataOffer(wl_data_offer::Event::SourceActions { source_actions }) => {
                if let Some(offer) = self.clipboard.offers.get_mut(&object.get()) {
                    offer.actions = *source_actions;
                }
            }
            AnyEvent::WlDataOffer(wl_data_offer::Event::Action { dnd_action }) => {
                if let Some(offer) = self.clipboard.offers.get_mut(&object.get()) {
                    offer.action = *dnd_action;
                }
            }
            AnyEvent::WlDataDevice(wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            }) => {
                self.file_drag_enter(*surface, *id, *serial, (x.to_f64(), y.to_f64()))?;
            }
            AnyEvent::WlDataDevice(wl_data_device::Event::Motion { x, y, .. }) => {
                self.file_drag_motion((x.to_f64(), y.to_f64()));
            }
            AnyEvent::WlDataDevice(wl_data_device::Event::Leave) => self.file_drag_leave()?,
            AnyEvent::WlDataDevice(wl_data_device::Event::Drop) => self.file_drag_drop()?,
            AnyEvent::WlDataSource(wl_data_source::Event::Send { mime_type, fd }) => {
                let text = self
                    .clipboard
                    .own
                    .as_ref()
                    .filter(|source| source.id == object)
                    .and_then(|source| source.payloads.iter().find(|(mime, _)| mime == mime_type))
                    .map(|(_, text)| text.clone())
                    .or_else(|| self.file_drag_payload(object, mime_type));
                if let Some(text) = text
                    && self.clipboard.outgoing.len() < 8
                {
                    let file = File::from(fd.try_clone()?);
                    linux_uapi::unix::set_nonblocking(file.as_fd(), true)?;
                    self.clipboard.outgoing.push(Transfer {
                        file,
                        text,
                        offset: 0,
                    });
                }
            }
            AnyEvent::WlDataSource(
                wl_data_source::Event::Cancelled | wl_data_source::Event::DndFinished,
            ) => {
                let clipboard = self
                    .clipboard
                    .own
                    .as_ref()
                    .is_some_and(|source| source.id == object);
                if clipboard {
                    self.clipboard.own = None;
                    self.selection_changed();
                }
                if clipboard || self.clipboard.drag_sources.remove(&object).is_some() {
                    wl_data_source::Request::Destroy
                        .encode(object, &mut self.transport.writer)
                        .map_err(io::Error::other)?;
                }
            }
            AnyEvent::WlDataSource(_) => {}
            _ => return Ok(false),
        }
        Ok(true)
    }
}
