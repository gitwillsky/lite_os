//! Dynamic physical-output configuration and resize-storm coalescing.

use std::io;

use display_proto::{OutputConfigure, Size, send_message};

use super::{Session, invalid};

impl Session {
    /// Publishes the latest DRM connector size to the desktop document.
    ///
    /// Each drained hotplug burst produces at most one serial. Idle buffers for
    /// older sizes are retired before the client requests its new triple, while
    /// busy buffers remain owned until their scene is either presented or
    /// discarded.
    pub(crate) fn configure_output(&mut self, size: Size) -> io::Result<()> {
        if size == self.display {
            return Ok(());
        }
        self.output_serial = self
            .output_serial
            .checked_add(1)
            .ok_or_else(|| invalid("output serial exhausted"))?;
        self.display = size;
        self.cancel_move_for_output_change()?;
        self.retire_stale_desktop_buffers()?;
        self.routing.clear();
        self.pointer_capture = None;
        self.pointer_surface = None;
        let Some(desktop) = &self.desktop else {
            return Ok(());
        };
        let mut bytes = [0u8; 40];
        let message = OutputConfigure {
            serial: self.output_serial,
            size,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("output configure encoding failed"))?;
        send_message(&desktop.stream, message)?;
        eprintln!(
            "compositor: output configure {} {}x{}",
            self.output_serial, size.width, size.height
        );
        Ok(())
    }

    fn cancel_move_for_output_change(&mut self) -> io::Result<()> {
        let Some(grab) = self.move_grab.take() else {
            return Ok(());
        };
        self.move_changed = false;
        self.buffers
            .values
            .remove(&grab.underlay_buffer_id)
            .ok_or_else(|| invalid("move underlay disappeared"))?;
        Ok(())
    }
}
