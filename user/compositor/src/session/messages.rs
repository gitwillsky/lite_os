//! Role-specific display message decoding and dispatch.

use std::io;

use display_proto::{
    BufferAlloc, CloseRequest, Configure, MessageKind, MoveBegin, SetCursorShape, SurfaceCommit,
};

use super::{Owner, Scene, Session, invalid, wire::receive};

impl Session {
    pub(super) fn receive_desktop(&mut self) -> io::Result<Option<Scene>> {
        let (kind, payload) = receive(self.desktop_stream()?)?;
        if self.receive_clipboard(0, kind, &payload)? {
            return Ok(None);
        }
        match kind {
            MessageKind::BufferAlloc => {
                self.allocate(
                    Owner::Desktop,
                    BufferAlloc::parse(&payload).ok_or_else(|| invalid("invalid allocation"))?,
                )?;
                Ok(None)
            }
            MessageKind::Configure => {
                let configure =
                    Configure::parse(&payload).ok_or_else(|| invalid("invalid configure"))?;
                self.route_configure(configure)?;
                Ok(None)
            }
            MessageKind::CloseRequest => {
                let request = CloseRequest::parse(&payload)
                    .ok_or_else(|| invalid("invalid close request"))?;
                self.route_close(request.surface_id)?;
                Ok(None)
            }
            MessageKind::MoveBegin => {
                let request =
                    MoveBegin::parse(&payload).ok_or_else(|| invalid("invalid move begin"))?;
                // A MoveBegin races the pointer stream: by the time it arrives
                // the authorizing pointer-down may have been superseded, the
                // window may no longer be presented, or the underlay buffer may
                // have been recycled. A rejected grab falls back to React move.
                if let Err(error) = self.begin_move(request) {
                    eprintln!("compositor: move grab rejected: {error}");
                }
                Ok(None)
            }
            MessageKind::SceneCommit => self.accept_scene(&payload).map(Some),
            MessageKind::SetCursorShape => {
                let request = SetCursorShape::parse(&payload)
                    .ok_or_else(|| invalid("invalid set cursor shape"))?;
                self.accept_cursor_shape(0, request)?;
                Ok(None)
            }
            _ => Err(invalid("message is invalid for desktop role")),
        }
    }

    pub(super) fn receive_app(&mut self, surface_id: u32) -> io::Result<()> {
        let stream = &self
            .apps
            .get(&surface_id)
            .ok_or_else(|| invalid("unknown app"))?
            .stream;
        let (kind, payload) = receive(stream)?;
        if self.receive_clipboard(surface_id, kind, &payload)? {
            return Ok(());
        }
        match kind {
            MessageKind::BufferAlloc => self.allocate(
                Owner::App(surface_id),
                BufferAlloc::parse(&payload).ok_or_else(|| invalid("invalid allocation"))?,
            ),
            MessageKind::SurfaceCommit => self.accept_surface(
                surface_id,
                SurfaceCommit::parse(&payload).ok_or_else(|| invalid("invalid surface commit"))?,
            ),
            MessageKind::SetCursorShape => {
                let request = SetCursorShape::parse(&payload)
                    .ok_or_else(|| invalid("invalid set cursor shape"))?;
                self.accept_cursor_shape(surface_id, request)
            }
            _ => Err(invalid("message is invalid for app role")),
        }
    }
}
