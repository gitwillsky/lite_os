//! Clipboard display-message authorization and asynchronous reply routing.

use std::io;

use display_proto::{
    ClipboardData, ClipboardRead, ClipboardWrite, MAX_MESSAGE, MessageKind, send_message,
};
use linux_uapi::unix::{PollEvents, PollFd};

use super::{Session, invalid};

impl Session {
    /// Handles one clipboard message and reports whether it consumed the kind.
    pub(super) fn receive_clipboard(
        &mut self,
        source_surface: u32,
        kind: MessageKind,
        payload: &[u8],
    ) -> io::Result<bool> {
        match kind {
            MessageKind::ClipboardRead => {
                let request = ClipboardRead::parse(payload)
                    .ok_or_else(|| invalid("invalid clipboard read"))?;
                self.accept_clipboard_read(source_surface, request)?;
            }
            MessageKind::ClipboardWrite => {
                let value = ClipboardWrite::parse(payload)
                    .ok_or_else(|| invalid("invalid clipboard write"))?;
                self.accept_clipboard_write(source_surface, value)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Appends the single vdagent descriptor and returns its array offset.
    pub(super) fn append_clipboard_poll(
        &self,
        descriptors: &mut [PollFd],
        descriptor_count: &mut usize,
    ) -> usize {
        let offset = *descriptor_count;
        let events = if self.clipboard.wants_write() {
            PollEvents::READ | PollEvents::WRITE
        } else {
            PollEvents::READ
        };
        descriptors[offset] = PollFd::new(self.clipboard.as_fd(), events);
        *descriptor_count += 1;
        offset
    }

    /// Drains ready vdagent I/O and routes every newly resolved read.
    pub(super) fn pump_clipboard(&mut self) -> io::Result<()> {
        for data in self.clipboard.pump()? {
            self.send_clipboard_data(data)?;
        }
        Ok(())
    }

    fn accept_clipboard_read(
        &mut self,
        source_surface: u32,
        request: ClipboardRead,
    ) -> io::Result<()> {
        if request.surface_id != source_surface || self.focused_surface != source_surface {
            return Err(invalid("clipboard read is not from the focused connection"));
        }
        if let Some(data) = self.clipboard.read(request)? {
            self.send_clipboard_data(data)?;
        }
        Ok(())
    }

    fn accept_clipboard_write(
        &mut self,
        source_surface: u32,
        value: ClipboardWrite,
    ) -> io::Result<()> {
        if value.surface_id != source_surface || self.focused_surface != source_surface {
            return Err(invalid(
                "clipboard write is not from the focused connection",
            ));
        }
        self.clipboard.write(value)
    }

    fn send_clipboard_data(&self, data: ClipboardData) -> io::Result<()> {
        let stream = if data.surface_id == 0 {
            self.desktop_stream()?
        } else {
            let Some(app) = self.apps.get(&data.surface_id) else {
                // Host clipboard data is asynchronous. App disconnect removes
                // pending requests, but a reply already decoded in this poll
                // turn can still race teardown and has no remaining consumer.
                return Ok(());
            };
            &app.stream
        };
        let mut bytes = vec![0u8; MAX_MESSAGE];
        let message = data
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("clipboard data encoding failed"))?;
        send_message(stream, message)
    }
}
