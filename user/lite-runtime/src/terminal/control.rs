//! Terminal control frames that do not participate in key translation.

use std::io;

use super::{INPUT, RESIZE, Terminal, write_frame};

impl Terminal {
    /// Writes trusted UTF-8 clipboard text as one PTY input frame.
    pub fn paste(&mut self, text: &str) -> io::Result<()> {
        write_frame(&mut self.input, INPUT, text.as_bytes())
    }

    /// Converts app pixels to a fixed terminal grid and sends a complete resize.
    pub fn resize(&mut self, width: u32, height: u32) -> io::Result<()> {
        let columns = (width / 8).max(1).min(u32::from(u16::MAX)) as u16;
        let rows = (height / 16).max(1).min(u32::from(u16::MAX)) as u16;
        let mut payload = [0u8; 8];
        payload[0..2].copy_from_slice(&columns.to_le_bytes());
        payload[2..4].copy_from_slice(&rows.to_le_bytes());
        payload[4..6].copy_from_slice(&(width.min(u32::from(u16::MAX)) as u16).to_le_bytes());
        payload[6..8].copy_from_slice(&(height.min(u32::from(u16::MAX)) as u16).to_le_bytes());
        write_frame(&mut self.input, RESIZE, &payload)
    }
}
