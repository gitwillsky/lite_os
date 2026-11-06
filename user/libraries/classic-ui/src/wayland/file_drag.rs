//! 文件拖放使用 Wayland data device 与 text/uri-list。源和目标通过非阻塞 FD 交换数据。

use std::{
    io::{self, Read},
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use wayland_wire::{
    protocol::{Interface, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source},
    wire::{ObjectId, Writer},
};

use super::{Display, EventKind, WindowId};
use desktop_entry::file_uris::{MAX_BYTES as LIMIT, URI_LIST, decode, encode};

use wl_data_device_manager::dnd_action::COPY;
const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PATHS: usize = 64;

#[derive(Default)]
pub(super) struct Offer {
    pub(super) mimes: Vec<String>,
    pub(super) actions: u32,
    pub(super) action: u32,
}

pub(super) struct Hover {
    pub(super) offer: ObjectId,
    pub(super) window: WindowId,
    pub(super) serial: u32,
    pub(super) point: (f64, f64),
    accepted: bool,
}

pub(super) struct Reading {
    offer: ObjectId,
    window: WindowId,
    point: (f64, f64),
    pub(super) stream: UnixStream,
    pub(super) deadline: Instant,
    bytes: Vec<u8>,
}

impl Display {
    /// 用按钮按下的 serial 发起标准 URI 拖放。源只提供 URI 数据，目标负责文件操作。
    ///
    /// # Errors
    ///
    /// 窗口、路径或容量不合法，或 Wayland 连接无法发送请求。
    pub fn start_file_drag(
        &mut self,
        window: WindowId,
        serial: u32,
        paths: &[PathBuf],
    ) -> io::Result<()> {
        let payload = encode(paths, MAX_PATHS)?;
        let (Some(manager), Some(device)) = (self.clipboard.manager, self.clipboard.device) else {
            return Ok(());
        };
        if !self.windows.contains_key(&window) || self.clipboard.drag_sources.len() >= 16 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let source = self.transport.allocate(Interface::WlDataSource)?;
        let writer = &mut self.transport.writer;
        wl_data_device_manager::Request::CreateDataSource { id: source }
            .encode(manager, writer)
            .map_err(io::Error::other)?;
        wl_data_source::Request::Offer {
            mime_type: URI_LIST.into(),
        }
        .encode(source, writer)
        .map_err(io::Error::other)?;
        wl_data_source::Request::SetActions { dnd_actions: COPY }
            .encode(source, writer)
            .map_err(io::Error::other)?;
        wl_data_device::Request::StartDrag {
            source: Some(source),
            origin: window.0,
            icon: None,
            serial,
        }
        .encode(device, writer)
        .map_err(io::Error::other)?;
        self.clipboard
            .drag_sources
            .insert(source, Arc::from(payload));
        self.transport.flush()
    }

    /// 接受或拒绝当前悬停的文件 URI；只协商 COPY，文件是否移动由目标确认后决定。
    ///
    /// # Errors
    ///
    /// Wayland 连接无法发送请求。
    pub fn accept_file_drag(&mut self, accept: bool) -> io::Result<()> {
        let Some(hover) = &mut self.clipboard.hover else {
            return Ok(());
        };
        let offer = self.clipboard.offers.get(&hover.offer.get());
        let accept = accept
            && offer.is_some_and(|offer| {
                offer.actions & COPY != 0 && offer.mimes.iter().any(|mime| mime == URI_LIST)
            });
        if hover.accepted != accept {
            wl_data_offer::Request::SetActions {
                dnd_actions: if accept { COPY } else { 0 },
                preferred_action: if accept { COPY } else { 0 },
            }
            .encode(hover.offer, &mut self.transport.writer)
            .map_err(io::Error::other)?;
            wl_data_offer::Request::Accept {
                serial: hover.serial,
                mime_type: accept.then(|| URI_LIST.to_owned()),
            }
            .encode(hover.offer, &mut self.transport.writer)
            .map_err(io::Error::other)?;
            hover.accepted = accept;
            self.transport.flush()?;
        }
        Ok(())
    }

    pub(super) fn file_drag_enter(
        &mut self,
        surface: ObjectId,
        offer: Option<ObjectId>,
        serial: u32,
        point: (f64, f64),
    ) -> io::Result<()> {
        self.file_drag_leave()?;
        let window = WindowId(surface);
        let Some(offer) = offer else {
            return Ok(());
        };
        if !self.windows.contains_key(&window) {
            return self.destroy_offer(offer);
        }
        self.clipboard.hover = Some(Hover {
            offer,
            window,
            serial,
            point,
            accepted: false,
        });
        self.file_drag_motion(point);
        Ok(())
    }

    pub(super) fn file_drag_motion(&mut self, point: (f64, f64)) {
        if let Some(hover) = &mut self.clipboard.hover {
            hover.point = point;
            let window = hover.window;
            if !self
                .clipboard
                .offers
                .get(&hover.offer.get())
                .is_some_and(|offer| {
                    offer.actions & COPY != 0 && offer.mimes.iter().any(|mime| mime == URI_LIST)
                })
            {
                return;
            }
            self.emit(
                Some(window),
                EventKind::FileDragOver {
                    x: point.0,
                    y: point.1,
                },
            );
        }
    }

    pub(super) fn file_drag_leave(&mut self) -> io::Result<()> {
        if let Some(hover) = self.clipboard.hover.take() {
            self.emit(Some(hover.window), EventKind::FileDragLeave);
            self.destroy_offer(hover.offer)?;
        }
        Ok(())
    }

    pub(super) fn file_drag_drop(&mut self) -> io::Result<()> {
        let Some(hover) = self.clipboard.hover.take() else {
            return Ok(());
        };
        self.emit(Some(hover.window), EventKind::FileDragLeave);
        if !hover.accepted
            || self.clipboard.reading.is_some()
            || !self
                .clipboard
                .offers
                .get(&hover.offer.get())
                .is_some_and(|offer| offer.action == COPY)
        {
            return self.destroy_offer(hover.offer);
        }
        let (stream, writer) = UnixStream::pair()?;
        stream.set_nonblocking(true)?;
        let mut request = Writer::new();
        wl_data_offer::Request::Receive {
            mime_type: URI_LIST.into(),
            fd: writer.try_clone()?.into(),
        }
        .encode(hover.offer, &mut request)
        .map_err(io::Error::other)?;
        self.transport.send_fd(request.bytes(), writer.as_fd())?;
        self.clipboard.reading = Some(Reading {
            offer: hover.offer,
            window: hover.window,
            point: hover.point,
            stream,
            bytes: Vec::new(),
            deadline: Instant::now() + TIMEOUT,
        });
        Ok(())
    }

    pub(super) fn progress_file_drop(&mut self) -> io::Result<()> {
        let Some(reading) = &mut self.clipboard.reading else {
            return Ok(());
        };
        let mut data = [0u8; 8192];
        let complete = loop {
            if reading.deadline <= Instant::now() {
                break false;
            }
            match reading.stream.read(&mut data) {
                Ok(0) => break true,
                Ok(count) if reading.bytes.len() + count <= LIMIT => {
                    reading.bytes.extend_from_slice(&data[..count])
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                _ => break false,
            }
        };
        let reading = self.clipboard.reading.take().expect("active file transfer");
        if complete
            && self.windows.contains_key(&reading.window)
            && let Some(paths) = decode(&reading.bytes, MAX_PATHS)
        {
            wl_data_offer::Request::Finish
                .encode(reading.offer, &mut self.transport.writer)
                .map_err(io::Error::other)?;
            self.emit(
                Some(reading.window),
                EventKind::FilesDropped {
                    x: reading.point.0,
                    y: reading.point.1,
                    paths,
                },
            );
        }
        self.destroy_offer(reading.offer)
    }

    pub(super) fn destroy_offer(&mut self, id: ObjectId) -> io::Result<()> {
        wl_data_offer::Request::Destroy
            .encode(id, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.clipboard.offers.remove(&id.get());
        self.transport.retire_server_object(id);
        Ok(())
    }

    pub(super) fn file_drag_payload(&self, object: ObjectId, mime: &str) -> Option<Arc<str>> {
        (mime == URI_LIST)
            .then(|| self.clipboard.drag_sources.get(&object).cloned())
            .flatten()
    }
}
