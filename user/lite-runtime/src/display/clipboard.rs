//! Plain-text clipboard requests over the display session.

use std::io;

use display_proto::{ClipboardRead, ClipboardWrite, MAX_MESSAGE, send_message};

use super::Display;

impl Display {
    /// Requests the current plain-text clipboard for one client-generated identity.
    pub fn clipboard_read(&self, request_id: u64) -> io::Result<()> {
        let mut bytes = [0u8; 24];
        let message = ClipboardRead {
            surface_id: self.surface_id,
            request_id,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("clipboard-read encoding failed"))?;
        send_message(&self.stream, message)
    }

    /// Publishes complete UTF-8 text as the system clipboard.
    pub fn clipboard_write(&self, text: String) -> io::Result<()> {
        let mut bytes = vec![0u8; MAX_MESSAGE];
        let message = ClipboardWrite {
            surface_id: self.surface_id,
            text,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("clipboard-write encoding failed"))?;
        send_message(&self.stream, message)
    }
}
