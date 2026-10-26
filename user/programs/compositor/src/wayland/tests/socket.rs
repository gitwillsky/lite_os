use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::{Duration, Instant},
};

use linux_uapi::unix::{PollEvents, poll};
use wayland_wire::{
    protocol::{self, AnyEvent, Interface, wl_display, wl_registry},
    wire::{Framer, ObjectId, Reader, Writer},
};

use super::{OUTPUT, id};
use crate::wayland::Server;

pub(super) fn socket_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wl-test-{}-{name}", std::process::id()))
}

/// 一次 poll + dispatch。
pub(super) fn pump(server: &mut Server) {
    let returned: Vec<PollEvents> = {
        let mut descriptors: Vec<_> = server.descriptors().collect();
        poll(&mut descriptors, Some(Duration::from_millis(200))).unwrap();
        descriptors
            .iter()
            .map(|descriptor| descriptor.returned())
            .collect()
    };
    server.dispatch(&returned).unwrap();
}

pub(super) fn read_frames(
    stream: &mut UnixStream,
    server: &mut Server,
    wanted: usize,
) -> Vec<AnyEvent> {
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut framer = Framer::new();
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while events.len() < wanted && Instant::now() < deadline {
        pump(server);
        let mut buffer = [0u8; 4096];
        if let Ok(count) = stream.read(&mut buffer) {
            framer.push(&buffer[..count]);
        }
        while let Some(frame) = framer.next_frame().unwrap() {
            let interface = match frame.object.get() {
                1 => Interface::WlDisplay,
                2 => Interface::WlRegistry,
                other => panic!("event for object {other}"),
            };
            let mut fds = std::collections::VecDeque::new();
            events.push(
                protocol::decode_event(
                    interface,
                    frame.opcode,
                    &mut Reader::new(&frame.body, &mut fds),
                )
                .unwrap(),
            );
        }
    }
    events
}

#[test]
fn a_real_socket_connection_completes_the_registry_handshake() {
    let path = socket_path("handshake");
    let mut server = Server::bind(&path, OUTPUT).unwrap();
    let mut stream = UnixStream::connect(&path).unwrap();
    let mut writer = Writer::new();
    wl_display::Request::GetRegistry { registry: id(2) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    stream.write_all(writer.bytes()).unwrap();
    let events = read_frames(&mut stream, &mut server, 5);
    assert!(
        matches!(&events[0], AnyEvent::WlRegistry(wl_registry::Event::Global { name: 1, interface, .. }) if interface == "wl_compositor")
    );
    assert_eq!(events.len(), 5);
    assert_eq!(server.connection_count(), 1);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn protocol_errors_reach_the_client_before_the_connection_is_closed() {
    let path = socket_path("error");
    let mut server = Server::bind(&path, OUTPUT).unwrap();
    let mut stream = UnixStream::connect(&path).unwrap();
    let mut writer = Writer::new();
    writer.begin(id(77), 0);
    writer.end().unwrap();
    stream.write_all(writer.bytes()).unwrap();
    let events = read_frames(&mut stream, &mut server, 1);
    assert!(matches!(
        events[0],
        AnyEvent::WlDisplay(wl_display::Event::Error { code: 0, .. })
    ));
    for _ in 0..3 {
        pump(&mut server);
    }
    assert_eq!(
        server.connection_count(),
        0,
        "connection is dropped after the error is delivered"
    );
    let mut rest = Vec::new();
    assert_eq!(stream.read_to_end(&mut rest).unwrap(), 0, "peer sees EOF");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_closed_client_is_removed_and_a_stale_socket_file_is_replaced() {
    let path = socket_path("close");
    std::fs::write(&path, b"stale").unwrap();
    let mut server = Server::bind(&path, OUTPUT).unwrap();
    drop(UnixStream::connect(&path).unwrap());
    for _ in 0..4 {
        pump(&mut server);
    }
    assert_eq!(server.connection_count(), 0);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn many_clients_are_served_independently() {
    let path = socket_path("many");
    let mut server = Server::bind(&path, OUTPUT).unwrap();
    let mut streams: Vec<UnixStream> = (0..8)
        .map(|_| UnixStream::connect(&path).unwrap())
        .collect();
    for stream in &mut streams {
        let mut writer = Writer::new();
        wl_display::Request::GetRegistry { registry: id(2) }
            .encode(ObjectId::DISPLAY, &mut writer)
            .unwrap();
        stream.write_all(writer.bytes()).unwrap();
    }
    for stream in &mut streams {
        assert_eq!(read_frames(stream, &mut server, 5).len(), 5);
    }
    assert_eq!(server.connection_count(), 8);
    std::fs::remove_file(path).unwrap();
}
