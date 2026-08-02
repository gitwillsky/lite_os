//! Role-specific display message decoding and dispatch.

use std::io;

use display_proto::{
    AcceleratorSet, CloseRequest, Configure, DisplayListCommit, MessageKind, MoveBegin,
    SetCursorShape, TextureCreate, TextureDestroy, TexturePublish, TextureWrite,
};

use super::{Owner, Scene, Session, invalid, wire::receive};

impl Session {
    pub(super) fn receive_desktop(&mut self) -> io::Result<Option<Scene>> {
        let (kind, payload) = receive(self.desktop_stream()?)?;
        if self.receive_clipboard(0, kind, &payload)? {
            return Ok(None);
        }
        match kind {
            MessageKind::TextureCreate => {
                let create = TextureCreate::parse(&payload)
                    .ok_or_else(|| invalid("invalid texture create"))?;
                self.paint
                    .create_texture(&self.graphics, Owner::Desktop, create)?;
                Ok(None)
            }
            MessageKind::TextureWrite => {
                let write = TextureWrite::parse(&payload)
                    .ok_or_else(|| invalid("invalid texture write"))?;
                self.paint.write_texture(Owner::Desktop, write)?;
                Ok(None)
            }
            MessageKind::TexturePublish => {
                let publish = TexturePublish::parse(&payload)
                    .ok_or_else(|| invalid("invalid texture publish"))?;
                self.paint.publish_texture(Owner::Desktop, publish)?;
                Ok(None)
            }
            MessageKind::TextureDestroy => {
                let destroy = TextureDestroy::parse(&payload)
                    .ok_or_else(|| invalid("invalid texture destroy"))?;
                self.paint.destroy_texture(Owner::Desktop, destroy)?;
                Ok(None)
            }
            MessageKind::DisplayListCommit => {
                DisplayListCommit::parse(&payload)
                    .ok_or_else(|| invalid("invalid display list"))?;
                self.paint.commit_list(Owner::Desktop, &payload)?;
                self.queue_paint(Owner::Desktop);
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
                // window may no longer be presented, or another grab may already
                // own the sequence. The request still transfers its underlay to
                // the compositor, which returns it before reporting rejection.
                if let Some(error) = self.begin_move(request)? {
                    eprintln!("compositor: move grab rejected: {error}");
                }
                Ok(None)
            }
            MessageKind::SceneCommit => self.accept_scene(&payload).map(Some),
            MessageKind::AcceleratorSet => {
                let chords = AcceleratorSet::parse(&payload)
                    .ok_or_else(|| invalid("invalid accelerator set"))?;
                self.accelerators.replace(chords);
                Ok(None)
            }
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
            MessageKind::TextureCreate => self.paint.create_texture(
                &self.graphics,
                Owner::App(surface_id),
                TextureCreate::parse(&payload).ok_or_else(|| invalid("invalid texture create"))?,
            ),
            MessageKind::TextureWrite => self.paint.write_texture(
                Owner::App(surface_id),
                TextureWrite::parse(&payload).ok_or_else(|| invalid("invalid texture write"))?,
            ),
            MessageKind::TexturePublish => self.paint.publish_texture(
                Owner::App(surface_id),
                TexturePublish::parse(&payload)
                    .ok_or_else(|| invalid("invalid texture publish"))?,
            ),
            MessageKind::TextureDestroy => self.paint.destroy_texture(
                Owner::App(surface_id),
                TextureDestroy::parse(&payload)
                    .ok_or_else(|| invalid("invalid texture destroy"))?,
            ),
            MessageKind::DisplayListCommit => {
                DisplayListCommit::parse(&payload)
                    .ok_or_else(|| invalid("invalid display list"))?;
                self.paint.commit_list(Owner::App(surface_id), &payload)?;
                self.queue_paint(Owner::App(surface_id));
                Ok(())
            }
            MessageKind::SetCursorShape => {
                let request = SetCursorShape::parse(&payload)
                    .ok_or_else(|| invalid("invalid set cursor shape"))?;
                self.accept_cursor_shape(surface_id, request)
            }
            _ => Err(invalid("message is invalid for app role")),
        }
    }
}
