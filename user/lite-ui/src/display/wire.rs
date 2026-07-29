//! Role-aware decoding for asynchronous display protocol events.

use std::{io, os::unix::net::UnixStream};

use display_proto::{
    Accepted, AppClosed, AppOpened, BufferRelease, ClipboardData, CloseRequest, Configure,
    ConfigureReady, InputKey, InputPointer, InputScroll, MAX_MESSAGE, MessageKind, MoveComplete,
    Presented, SurfaceActivated, parse_frame, recv_frame_blocking,
};

use super::{Event, invalid};

pub(super) enum WireEvent {
    Public(Event),
    Accepted(u64),
    Released(u32),
    Presented { revision: u64, monotonic_ns: u64 },
}

pub(super) fn receive_configure(stream: &UnixStream, surface_id: u32) -> io::Result<Configure> {
    let mut bytes = [0u8; MAX_MESSAGE];
    let (length, fd) = recv_frame_blocking(stream, &mut bytes)?;
    if fd.is_some() {
        return Err(invalid("configure carried a descriptor"));
    }
    let frame = parse_frame(&bytes[..length])
        .filter(|frame| frame.kind() == MessageKind::Configure)
        .ok_or_else(|| invalid("initial configure missing"))?;
    Configure::parse(frame.payload())
        .filter(|configure| configure.surface_id == surface_id)
        .ok_or_else(|| invalid("initial configure invalid"))
}

pub(super) fn parse_event(
    kind: MessageKind,
    payload: &[u8],
    own_surface: u32,
) -> Option<WireEvent> {
    Some(match kind {
        MessageKind::Accepted => WireEvent::Accepted(Accepted::parse(payload)?.revision),
        MessageKind::BufferRelease => WireEvent::Released(BufferRelease::parse(payload)?.buffer_id),
        MessageKind::Presented => {
            let presented = Presented::parse(payload)?;
            WireEvent::Presented {
                revision: presented.revision,
                monotonic_ns: presented.monotonic_ns,
            }
        }
        MessageKind::AppOpened if own_surface == 0 => {
            let event = AppOpened::parse(payload)?;
            WireEvent::Public(Event::AppOpened {
                surface_id: event.surface_id,
                app_id: std::str::from_utf8(event.app_id).ok()?.to_owned(),
            })
        }
        MessageKind::AppClosed if own_surface == 0 => WireEvent::Public(Event::AppClosed {
            surface_id: AppClosed::parse(payload)?.surface_id,
        }),
        MessageKind::SurfaceActivated if own_surface == 0 => {
            WireEvent::Public(Event::SurfaceActivated {
                surface_id: SurfaceActivated::parse(payload)?.surface_id,
            })
        }
        MessageKind::MoveComplete if own_surface == 0 => {
            let event = MoveComplete::parse(payload)?;
            WireEvent::Public(Event::MoveComplete {
                surface_id: event.surface_id,
                x: event.x,
                y: event.y,
            })
        }
        MessageKind::ConfigureReady if own_surface == 0 => {
            let event = ConfigureReady::parse(payload)?;
            WireEvent::Public(Event::ConfigureReady {
                surface_id: event.surface_id,
                serial: event.serial,
            })
        }
        MessageKind::Configure if own_surface != 0 => {
            WireEvent::Public(Event::Configure(Configure::parse(payload)?))
        }
        MessageKind::CloseRequest if own_surface != 0 => {
            CloseRequest::parse(payload)?;
            WireEvent::Public(Event::Close)
        }
        MessageKind::InputPointer => {
            WireEvent::Public(Event::Pointer(InputPointer::parse(payload)?))
        }
        MessageKind::InputScroll => WireEvent::Public(Event::Scroll(InputScroll::parse(payload)?)),
        MessageKind::InputKey => WireEvent::Public(Event::Key(InputKey::parse(payload)?)),
        MessageKind::ClipboardData => {
            let data = ClipboardData::parse(payload)?;
            (data.surface_id == own_surface).then_some(())?;
            WireEvent::Public(Event::ClipboardData(data))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use display_proto::{MessageKind, Presented, parse_frame};

    use super::{WireEvent, parse_event};

    #[test]
    fn presented_keeps_the_compositor_timeline_timestamp() {
        let mut bytes = [0u8; 64];
        let encoded = Presented {
            revision: 7,
            frame_sequence: 9,
            monotonic_ns: 11_000_000,
        }
        .encode(&mut bytes)
        .expect("presented encodes");
        let frame = parse_frame(encoded).expect("presented frame parses");

        assert!(matches!(
            parse_event(MessageKind::Presented, frame.payload(), 0),
            Some(WireEvent::Presented {
                revision: 7,
                monotonic_ns: 11_000_000
            })
        ));
    }
}
