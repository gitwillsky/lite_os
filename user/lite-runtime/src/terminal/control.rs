//! Terminal control frames that do not participate in key translation.

use std::io;

use super::{INPUT, RESIZE, SCROLL, SELECT, Terminal, write_frame};

impl Terminal {
    /// Writes trusted UTF-8 clipboard text as one PTY input frame.
    pub fn paste(&mut self, text: &str) -> io::Result<()> {
        write_frame(&mut self.input, INPUT, text.as_bytes())
    }

    /// Sends one inclusive visible-grid selection to the terminal helper.
    ///
    /// `anchor_column`/`anchor_row` identify the pointer-down cell and
    /// `focus_column`/`focus_row` identify the latest drag cell.
    ///
    /// Returns an I/O error when the helper control stream cannot accept the frame.
    pub fn select(
        &mut self,
        anchor_column: u16,
        anchor_row: u16,
        focus_column: u16,
        focus_row: u16,
    ) -> io::Result<()> {
        let mut payload = [0u8; 8];
        payload[0..2].copy_from_slice(&anchor_column.to_le_bytes());
        payload[2..4].copy_from_slice(&anchor_row.to_le_bytes());
        payload[4..6].copy_from_slice(&focus_column.to_le_bytes());
        payload[6..8].copy_from_slice(&focus_row.to_le_bytes());
        write_frame(&mut self.input, SELECT, &payload)
    }

    /// Moves the helper-owned scrollback viewport by signed logical lines.
    ///
    /// Positive `lines` browse older history and negative values move toward
    /// the live bottom.
    ///
    /// Returns an I/O error when the helper control stream cannot accept the frame.
    pub fn scroll(&mut self, lines: i32) -> io::Result<()> {
        write_frame(&mut self.input, SCROLL, &lines.to_le_bytes())
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
