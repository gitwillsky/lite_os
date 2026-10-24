//! 经真实 Unix socket 传递 keymap 描述符。宿主 macOS 的 `recv_fds` 没有 SCM_RIGHTS 分支，所以只在 Linux 上运行；
//! 运行时由 QEMU 门禁里的真实客户端覆盖。

use std::{
    collections::VecDeque,
    fs::File,
    io::Read,
    os::{fd::AsFd, unix::net::UnixStream},
};

use linux_uapi::unix::{recv_fds, send_fd};
use wayland_wire::{
    protocol::{self, AnyEvent, wl_keyboard, wl_seat},
    wire::Framer,
};

use super::seat::{KEYBOARD, SEAT};
use super::*;
use crate::wayland::Server;

#[test]
fn a_real_socket_delivers_a_readable_keymap_descriptor_before_its_message() {
    let path = std::env::temp_dir().join(format!("wl-seat-{}", std::process::id()));
    let mut server = Server::bind(&path, OUTPUT).unwrap();
    let mut stream = UnixStream::connect(&path).unwrap();
    let mut writer = Writer::new();
    wl_display::Request::GetRegistry { registry: id(2) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    wl_registry::Request::Bind {
        name: 5,
        id: DynamicNewId {
            interface: "wl_seat".into(),
            version: 7,
            id: id(SEAT),
        },
    }
    .encode(id(2), &mut writer)
    .unwrap();
    wl_seat::Request::GetKeyboard { id: id(KEYBOARD) }
        .encode(id(SEAT), &mut writer)
        .unwrap();
    // 请求本身不带 fd；用 send_fd 只是确保 socket 是流式 Unix socket 的通常路径。
    send_fd(
        stream.as_fd(),
        writer.bytes(),
        File::open("/dev/null").unwrap().as_fd(),
    )
    .unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(20)))
        .unwrap();
    let mut framer = Framer::new();
    let mut fds = VecDeque::new();
    let mut keymap = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while keymap.is_none() && std::time::Instant::now() < deadline {
        let returned: Vec<_> = {
            let mut descriptors = server.descriptors();
            linux_uapi::unix::poll(
                &mut descriptors,
                Some(std::time::Duration::from_millis(100)),
            )
            .unwrap();
            descriptors
                .iter()
                .map(|descriptor| descriptor.returned())
                .collect()
        };
        server.dispatch(&returned).unwrap();
        let mut buffer = [0u8; 8192];
        if let Ok((count, received)) = recv_fds(stream.as_fd(), &mut buffer) {
            framer.push(&buffer[..count]);
            fds.extend(received);
        }
        while let Some(frame) = framer.next_frame().unwrap() {
            if frame.object.get() == KEYBOARD {
                let event = protocol::decode_event(
                    Interface::WlKeyboard,
                    frame.opcode,
                    &mut Reader::new(&frame.body, &mut fds),
                )
                .unwrap();
                if let AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap { fd, size, .. }) = event {
                    keymap = Some((fd, size));
                }
            }
        }
    }
    let (fd, size) = keymap.expect("keymap event with a descriptor");
    let mut text = Vec::new();
    File::from(fd).read_to_end(&mut text).unwrap();
    assert_eq!(text.len(), size as usize);
    assert_eq!(*text.last().unwrap(), 0, "NUL terminated");
    assert_eq!(
        &text[..text.len() - 1],
        keyboard::xkb::keymap_text().as_bytes()
    );
    std::fs::remove_file(path).unwrap();
}
