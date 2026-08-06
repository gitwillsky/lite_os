//! Connection handshake and immutable client-role assignment.

use std::{io, os::unix::net::UnixStream};

use display_proto::{
    HelloApp, HelloDesktop, MAX_APP_SURFACES, MessageKind, PROTOCOL_VERSION, Welcome, parse_frame,
    recv_frame_blocking, send_message,
};

use super::{App, Desktop, Session, invalid, wire::valid_app_id};

impl Session {
    pub(super) fn accept(&mut self) -> io::Result<()> {
        let (stream, _) = match self.listener.accept() {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut bytes = [0u8; 128];
        let length = recv_frame_blocking(&stream, &mut bytes)?;
        if length == 0 {
            return Err(invalid("invalid display handshake"));
        }
        let frame = parse_frame(&bytes[..length]).ok_or_else(|| invalid("invalid handshake"))?;
        match frame.kind() {
            MessageKind::HelloDesktop => {
                HelloDesktop::parse(frame.payload())
                    .ok_or_else(|| invalid("desktop protocol mismatch"))?;
                if self.desktop.is_some() {
                    return Err(invalid("desktop already connected"));
                }
                self.welcome(&stream, 0)?;
                self.desktop = Some(Desktop {
                    stream,
                    last_revision: 0,
                });
                eprintln!("compositor: desktop connected");
            }
            MessageKind::HelloApp => {
                let hello = HelloApp::parse(frame.payload())
                    .ok_or_else(|| invalid("app protocol mismatch"))?;
                let id = std::str::from_utf8(hello.app_id)
                    .ok()
                    .filter(|id| valid_app_id(id))
                    .ok_or_else(|| invalid("invalid app id"))?
                    .to_owned();
                if self.desktop.is_none() || self.apps.len() >= MAX_APP_SURFACES {
                    return Err(invalid("app session unavailable"));
                }
                let surface_id = self.take_surface_id()?;
                self.welcome(&stream, surface_id)?;
                self.apps.insert(
                    surface_id,
                    App {
                        stream,
                        id,
                        configure: None,
                        last_revision: 0,
                        pending: None,
                        current: None,
                        first_scene_presented: false,
                        close_deadline: None,
                    },
                );
                self.notify_opened(surface_id)?;
                eprintln!("compositor: app {surface_id} connected");
            }
            _ => return Err(invalid("handshake role required")),
        }
        Ok(())
    }

    fn welcome(&self, stream: &UnixStream, surface_id: u32) -> io::Result<()> {
        let mut bytes = [0u8; 64];
        let message = Welcome {
            version: PROTOCOL_VERSION,
            display: self.display,
            surface_id,
            session_epoch: self.epoch,
            output_serial: self.output_serial,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("welcome encoding failed"))?;
        send_message(stream, message)
    }
}
