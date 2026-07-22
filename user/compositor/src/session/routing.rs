//! Input routing against the last page-flip-complete scene.

use std::{io, os::unix::net::UnixStream};

use display_proto::{InputKey, InputPointer, PointerPhase, Rect, send_message};

use super::{Session, invalid};

impl Session {
    pub(super) fn clear_pointer_capture(&mut self, surface_id: Option<u32>) {
        if self
            .pointer_capture
            .is_some_and(|capture| surface_id.is_none_or(|id| capture.0 == id))
        {
            self.pointer_capture = None;
        }
    }

    /// Routes one pointer transition against the last presented scene.
    pub fn route_pointer(
        &mut self,
        x: i32,
        y: i32,
        phase: PointerPhase,
        button: u32,
        buttons: u32,
        serial: u64,
    ) -> io::Result<()> {
        let hit = self.routing.iter().rev().find(|node| {
            node.input
                .iter()
                .any(|rectangle| contains(*rectangle, x, y))
        });
        let target = self
            .pointer_capture
            .or_else(|| hit.map(|target| (target.surface_id, target.bounds)));
        let Some((surface_id, bounds)) = target else {
            return Ok(());
        };
        if phase == PointerPhase::Down {
            self.pointer_capture = Some((surface_id, bounds));
        }
        let scale = display_proto::DEVICE_SCALE_FACTOR as i32;
        let event = InputPointer {
            surface_id,
            serial,
            phase,
            button,
            buttons,
            x: (x - bounds.x) / scale,
            y: (y - bounds.y) / scale,
        };
        let mut bytes = [0u8; 64];
        let message = event
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("pointer encoding failed"))?;
        let result = send_message(self.target_stream(surface_id)?, message);
        if phase == PointerPhase::Up {
            self.pointer_capture = None;
        }
        result
    }

    /// Routes one keyboard transition to the presented focused surface.
    pub fn route_key(&self, code: u32, value: i32, modifiers: u32) -> io::Result<()> {
        let event = InputKey {
            surface_id: self.focused_surface,
            code,
            value,
            modifiers,
        };
        let mut bytes = [0u8; 40];
        let message = event
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("key encoding failed"))?;
        send_message(self.target_stream(self.focused_surface)?, message)
    }

    fn target_stream(&self, surface_id: u32) -> io::Result<&UnixStream> {
        if surface_id == 0 {
            self.desktop_stream()
        } else {
            self.apps
                .get(&surface_id)
                .map(|app| &app.stream)
                .ok_or_else(|| invalid("input target disappeared"))
        }
    }
}

fn contains(rectangle: Rect, x: i32, y: i32) -> bool {
    x >= rectangle.x
        && y >= rectangle.y
        && x < rectangle.x.saturating_add_unsigned(rectangle.width)
        && y < rectangle.y.saturating_add_unsigned(rectangle.height)
}
