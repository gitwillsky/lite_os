use std::{
    collections::VecDeque,
    fs::File,
    io::Write,
    os::fd::{AsRawFd, OwnedFd},
};

use wayland_wire::{
    protocol::{
        self, AnyEvent, Interface, wl_buffer, wl_callback, wl_compositor, wl_display, wl_registry,
        wl_shm, wl_shm_pool, wl_surface, xdg_surface, xdg_toplevel, xdg_wm_base,
    },
    wire::{DynamicNewId, Framer, ObjectId, Reader, Writer},
};

use super::{Client, Disconnected, Output, WindowRequest};

const OUTPUT: Output = Output {
    width: 1280,
    height: 800,
    scale: 2,
};

fn id(value: u32) -> ObjectId {
    ObjectId::new(value).unwrap()
}

fn shm_file(bytes: &[u8]) -> OwnedFd {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "compositor-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all(bytes).unwrap();
    std::fs::remove_file(path).unwrap();
    file.into()
}

/// 扮演客户端：编码请求、喂给 `Client`、解码它写回的事件。
struct Harness {
    client: Client,
}

struct Event {
    object: u32,
    event: AnyEvent,
}

impl Harness {
    fn new() -> Self {
        Self {
            client: Client::new(OUTPUT),
        }
    }

    fn send(&mut self, build: impl FnOnce(&mut Writer)) -> Result<(), Disconnected> {
        self.send_with_fds(build, Vec::new())
    }

    fn send_with_fds(
        &mut self,
        build: impl FnOnce(&mut Writer),
        fds: Vec<OwnedFd>,
    ) -> Result<(), Disconnected> {
        let mut writer = Writer::new();
        build(&mut writer);
        self.client.receive(writer.bytes(), fds)
    }

    /// 取走并解码客户端待收的全部事件；`interfaces` 给出各对象 id 的接口（display 固定为 id 1）。
    fn events(&mut self, interfaces: &[(u32, Interface)]) -> Vec<Event> {
        let (bytes, _) = self.client.output();
        let bytes = bytes.to_vec();
        self.client.consume_output(bytes.len());
        let mut framer = Framer::new();
        framer.push(&bytes);
        let mut out = Vec::new();
        while let Some(frame) = framer.next_frame().unwrap() {
            let interface = if frame.object.get() == 1 {
                Interface::WlDisplay
            } else {
                interfaces
                    .iter()
                    .find(|(object, _)| *object == frame.object.get())
                    .expect("event for an unexpected object")
                    .1
            };
            let mut fds = VecDeque::new();
            let event = protocol::decode_event(
                interface,
                frame.opcode,
                &mut Reader::new(&frame.body, &mut fds),
            )
            .unwrap();
            out.push(Event {
                object: frame.object.get(),
                event,
            });
        }
        out
    }

    fn expect_error(&mut self, code: u32) -> String {
        let events = self.events(&[]);
        match events.as_slice() {
            [
                Event {
                    object: 1,
                    event:
                        AnyEvent::WlDisplay(wl_display::Event::Error {
                            code: actual,
                            message,
                            ..
                        }),
                },
            ] => {
                assert_eq!(*actual, code, "{message}");
                message.clone()
            }
            other => panic!("expected one wl_display.error, got {} events", other.len()),
        }
    }

    /// 完成握手并绑定 compositor（id 3）、shm（id 4）与 xdg_wm_base（id 20）。
    fn bound() -> Self {
        let mut harness = Self::new();
        harness
            .send(|w| {
                wl_display::Request::GetRegistry { registry: id(2) }
                    .encode(ObjectId::DISPLAY, w)
                    .unwrap()
            })
            .unwrap();
        harness.events(&[(2, Interface::WlRegistry)]);
        for (name, interface, version, new) in [
            (1, "wl_compositor", 6, 3),
            (2, "wl_shm", 1, 4),
            (3, "xdg_wm_base", 3, 20),
        ] {
            harness
                .send(|w| {
                    wl_registry::Request::Bind {
                        name,
                        id: DynamicNewId {
                            interface: interface.into(),
                            version,
                            id: id(new),
                        },
                    }
                    .encode(id(2), w)
                    .unwrap()
                })
                .unwrap();
        }
        harness.events(&[(4, Interface::WlShm)]);
        harness
    }

    fn create_surface(&mut self, surface: u32) {
        self.send(|w| {
            wl_compositor::Request::CreateSurface { id: id(surface) }
                .encode(id(3), w)
                .unwrap()
        })
        .unwrap();
        // 版本 6 的 surface 创建时会收到 preferred_buffer_scale；多数测试不关心它。
        self.events(&[(surface, Interface::WlSurface)]);
    }

    /// 建立 pool（id 10）与 buffer（id 11）。
    fn create_buffer(
        &mut self,
        bytes: &[u8],
        width: i32,
        height: i32,
        stride: i32,
        format: wl_shm::Format,
    ) -> Result<(), Disconnected> {
        let fd = shm_file(bytes);
        let raw = fd.as_raw_fd();
        self.send_with_fds(
            |w| {
                w.begin(id(4), 0);
                w.object(Some(id(10)));
                w.fd(raw);
                w.int(bytes.len() as i32);
                w.end().unwrap();
            },
            vec![fd],
        )?;
        self.send(|w| {
            wl_shm_pool::Request::CreateBuffer {
                id: id(11),
                offset: 0,
                width,
                height,
                stride,
                format,
            }
            .encode(id(10), w)
            .unwrap()
        })
    }
}

#[test]
fn registry_advertises_the_implemented_globals() {
    let mut harness = Harness::new();
    harness
        .send(|w| {
            wl_display::Request::GetRegistry { registry: id(2) }
                .encode(ObjectId::DISPLAY, w)
                .unwrap()
        })
        .unwrap();
    let globals: Vec<(u32, String, u32)> = harness
        .events(&[(2, Interface::WlRegistry)])
        .into_iter()
        .map(|event| match event.event {
            AnyEvent::WlRegistry(wl_registry::Event::Global {
                name,
                interface,
                version,
            }) => (name, interface, version),
            _ => panic!("unexpected event"),
        })
        .collect();
    assert_eq!(
        globals,
        [
            (1, "wl_compositor".to_owned(), 6),
            (2, "wl_shm".to_owned(), 1),
            (3, "xdg_wm_base".to_owned(), 3),
            (4, "wl_output".to_owned(), 4),
            (5, "wl_seat".to_owned(), 7)
        ]
    );
}

#[test]
fn sync_completes_and_releases_the_callback_id() {
    let mut harness = Harness::new();
    harness
        .send(|w| {
            wl_display::Request::Sync { callback: id(5) }
                .encode(ObjectId::DISPLAY, w)
                .unwrap()
        })
        .unwrap();
    let events = harness.events(&[(5, Interface::WlCallback)]);
    assert!(matches!(
        events[0].event,
        AnyEvent::WlCallback(wl_callback::Event::Done { callback_data: 0 })
    ));
    assert!(matches!(
        events[1].event,
        AnyEvent::WlDisplay(wl_display::Event::DeleteId { id: 5 })
    ));
    // id 已释放，可以复用。
    harness
        .send(|w| {
            wl_display::Request::Sync { callback: id(5) }
                .encode(ObjectId::DISPLAY, w)
                .unwrap()
        })
        .unwrap();
}

#[test]
fn binding_shm_announces_its_formats() {
    let mut harness = Harness::new();
    harness
        .send(|w| {
            wl_display::Request::GetRegistry { registry: id(2) }
                .encode(ObjectId::DISPLAY, w)
                .unwrap()
        })
        .unwrap();
    harness.events(&[(2, Interface::WlRegistry)]);
    harness
        .send(|w| {
            wl_registry::Request::Bind {
                name: 2,
                id: DynamicNewId {
                    interface: "wl_shm".into(),
                    version: 1,
                    id: id(4),
                },
            }
            .encode(id(2), w)
            .unwrap()
        })
        .unwrap();
    let formats: Vec<_> = harness
        .events(&[(4, Interface::WlShm)])
        .into_iter()
        .map(|event| match event.event {
            AnyEvent::WlShm(wl_shm::Event::Format { format }) => format,
            _ => panic!("unexpected event"),
        })
        .collect();
    assert_eq!(
        formats,
        [wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888]
    );
}

#[test]
fn invalid_binds_are_protocol_errors_that_close_the_connection() {
    for (name, interface, version) in [
        (9, "wl_shm", 1),
        (2, "wl_compositor", 1),
        (1, "wl_compositor", 7),
        (1, "wl_compositor", 0),
    ] {
        let mut harness = Harness::new();
        harness
            .send(|w| {
                wl_display::Request::GetRegistry { registry: id(2) }
                    .encode(ObjectId::DISPLAY, w)
                    .unwrap()
            })
            .unwrap();
        harness.events(&[(2, Interface::WlRegistry)]);
        let result = harness.send(|w| {
            wl_registry::Request::Bind {
                name,
                id: DynamicNewId {
                    interface: interface.into(),
                    version,
                    id: id(3),
                },
            }
            .encode(id(2), w)
            .unwrap()
        });
        assert_eq!(result, Err(Disconnected), "{name} {interface} {version}");
        harness.expect_error(wl_display::Error::InvalidObject as u32);
        assert_eq!(
            harness.send(|w| wl_display::Request::Sync { callback: id(9) }
                .encode(ObjectId::DISPLAY, w)
                .unwrap()),
            Err(Disconnected)
        );
    }
}

#[test]
fn unknown_objects_and_bad_new_ids_are_rejected() {
    let mut harness = Harness::new();
    assert!(
        harness
            .send(|w| wl_surface::Request::Commit.encode(id(77), w).unwrap())
            .is_err()
    );
    harness.expect_error(wl_display::Error::InvalidObject as u32);

    let mut harness = Harness::bound();
    harness.create_surface(5);
    assert!(
        harness
            .send(|w| wl_compositor::Request::CreateSurface { id: id(5) }
                .encode(id(3), w)
                .unwrap())
            .is_err()
    );
    harness.expect_error(wl_display::Error::InvalidObject as u32);

    let mut harness = Harness::bound();
    assert!(
        harness
            .send(|w| wl_compositor::Request::CreateSurface {
                id: id(0xFF00_0000)
            }
            .encode(id(3), w)
            .unwrap())
            .is_err()
    );
    harness.expect_error(wl_display::Error::InvalidObject as u32);
}

#[test]
fn malformed_and_too_new_requests_are_invalid_method() {
    let mut harness = Harness::bound();
    // wl_shm.release 自版本 2 起；这里绑定的是版本 1。
    assert!(
        harness
            .send(|w| wl_shm::Request::Release.encode(id(4), w).unwrap())
            .is_err()
    );
    harness.expect_error(wl_display::Error::InvalidMethod as u32);

    let mut harness = Harness::bound();
    assert!(
        harness
            .send(|w| {
                w.begin(id(3), 9);
                w.end().unwrap();
            })
            .is_err(),
        "unknown opcode"
    );
    harness.expect_error(wl_display::Error::InvalidMethod as u32);

    let mut harness = Harness::bound();
    assert!(
        harness
            .send(|w| {
                w.begin(id(3), 0);
                w.end().unwrap();
            })
            .is_err(),
        "missing argument"
    );
    harness.expect_error(wl_display::Error::InvalidMethod as u32);

    let mut harness = Harness::new();
    assert!(
        harness
            .client
            .receive(&[1, 0, 0, 0, 0, 0, 4, 0], Vec::new())
            .is_err(),
        "size below the header"
    );
    harness.expect_error(wl_display::Error::InvalidMethod as u32);
}

#[test]
fn commit_copies_pixels_releases_the_buffer_and_fires_frame_callbacks() {
    let mut harness = Harness::bound();
    harness.create_surface(5);
    let pixels: Vec<u8> = [
        0x80_10_20_30u32,
        0x00_11_22_33,
        0xFF_AA_BB_CC,
        0x40_01_02_03,
    ]
    .iter()
    .flat_map(|p| p.to_le_bytes())
    .collect();
    harness
        .create_buffer(&pixels, 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Damage {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Frame { callback: id(12) }
                .encode(id(5), w)
                .unwrap()
        })
        .unwrap();
    assert_eq!(
        harness.client.contents().count(),
        0,
        "nothing is visible before commit"
    );
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();

    let events = harness.events(&[(11, Interface::WlBuffer)]);
    assert!(matches!(
        events[..],
        [Event {
            object: 11,
            event: AnyEvent::WlBuffer(wl_buffer::Event::Release)
        }]
    ));
    let (surface, content) = harness.client.contents().next().unwrap();
    assert_eq!((surface, content.width, content.height), (id(5), 2, 2));
    assert_eq!(
        content.pixels,
        [0x80_10_20_30, 0x00_11_22_33, 0xFF_AA_BB_CC, 0x40_01_02_03]
    );

    harness.client.finish_frame(1234);
    let events = harness.events(&[(12, Interface::WlCallback)]);
    assert!(matches!(
        events[0].event,
        AnyEvent::WlCallback(wl_callback::Event::Done {
            callback_data: 1234
        })
    ));
    assert!(matches!(
        events[1].event,
        AnyEvent::WlDisplay(wl_display::Event::DeleteId { id: 12 })
    ));
    harness.client.finish_frame(2000);
    assert!(harness.events(&[]).is_empty(), "callbacks fire once");
}

#[test]
fn xrgb_buffers_are_forced_opaque_and_stride_padding_is_skipped() {
    let mut harness = Harness::bound();
    harness.create_surface(5);
    let mut bytes = Vec::new();
    for row in 0..2u32 {
        for column in 0..2u32 {
            bytes.extend((0x00_00_00_10 + row * 16 + column).to_le_bytes());
        }
        bytes.extend([0xEE; 8]); // 行尾填充
    }
    harness
        .create_buffer(&bytes, 2, 2, 16, wl_shm::Format::Xrgb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    let (_, content) = harness.client.contents().next().unwrap();
    assert_eq!(
        content.pixels,
        [0xFF00_0010, 0xFF00_0011, 0xFF00_0020, 0xFF00_0021]
    );
}

#[test]
fn attaching_nothing_unmaps_and_destroying_a_surface_recycles_its_callbacks() {
    let mut harness = Harness::bound();
    harness.create_surface(5);
    harness
        .create_buffer(&[0; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    assert_eq!(harness.client.contents().count(), 1);
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: None,
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    assert_eq!(harness.client.contents().count(), 0);

    harness.events(&[(11, Interface::WlBuffer)]);
    harness
        .send(|w| {
            wl_surface::Request::Frame { callback: id(12) }
                .encode(id(5), w)
                .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_surface::Request::Destroy.encode(id(5), w).unwrap())
        .unwrap();
    let freed: Vec<u32> = harness
        .events(&[])
        .into_iter()
        .filter_map(|event| match event.event {
            AnyEvent::WlDisplay(wl_display::Event::DeleteId { id }) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(freed, [12, 5]);
}

#[test]
fn buffer_geometry_must_fit_the_pool_and_format() {
    for (width, height, stride) in [
        (2, 2, 4),
        (0, 2, 8),
        (2, 0, 8),
        (2, 2, -8),
        (4, 2, 16),
        (2, 3, 8),
    ] {
        let mut harness = Harness::bound();
        let result =
            harness.create_buffer(&[0; 16], width, height, stride, wl_shm::Format::Argb8888);
        assert_eq!(
            result,
            Err(Disconnected),
            "{width}x{height} stride {stride}"
        );
        harness.events(&[]);
    }
    let mut harness = Harness::bound();
    assert!(
        harness
            .create_buffer(&[0; 16], 2, 2, 8, wl_shm::Format::Rgb565)
            .is_err()
    );
}

#[test]
fn a_file_truncated_after_buffer_creation_is_an_error_not_a_crash() {
    let mut harness = Harness::bound();
    harness.create_surface(5);
    let fd = shm_file(&[0; 64]);
    let raw = fd.as_raw_fd();
    let duplicate = File::from(fd.try_clone().unwrap());
    harness
        .send_with_fds(
            |w| {
                w.begin(id(4), 0);
                w.object(Some(id(10)));
                w.fd(raw);
                w.int(64);
                w.end().unwrap();
            },
            vec![fd],
        )
        .unwrap();
    harness
        .send(|w| {
            wl_shm_pool::Request::CreateBuffer {
                id: id(11),
                offset: 0,
                width: 4,
                height: 4,
                stride: 16,
                format: wl_shm::Format::Argb8888,
            }
            .encode(id(10), w)
            .unwrap()
        })
        .unwrap();
    duplicate.set_len(20).unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    assert!(
        harness
            .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
            .is_err()
    );
    harness.expect_error(wl_shm::Error::InvalidFd as u32);
}

#[test]
fn pools_only_grow() {
    let mut harness = Harness::bound();
    harness
        .create_buffer(&[0; 64], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_shm_pool::Request::Resize { size: 128 }
                .encode(id(10), w)
                .unwrap()
        })
        .unwrap();
    assert!(
        harness
            .send(|w| wl_shm_pool::Request::Resize { size: 32 }
                .encode(id(10), w)
                .unwrap())
            .is_err()
    );
}

#[test]
fn buffer_scale_attach_offset_and_transform_are_validated() {
    let mut harness = Harness::bound();
    harness.create_surface(5);
    harness
        .create_buffer(&[0; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::SetBufferScale { scale: 2 }
                .encode(id(5), w)
                .unwrap()
        })
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::SetBufferTransform { transform: 3 }
                .encode(id(5), w)
                .unwrap()
        })
        .unwrap();
    assert!(
        harness
            .send(|w| wl_surface::Request::SetBufferScale { scale: 0 }
                .encode(id(5), w)
                .unwrap())
            .is_err()
    );

    let mut harness = Harness::bound();
    harness.create_surface(5);
    assert!(
        harness
            .send(|w| wl_surface::Request::SetBufferTransform { transform: 8 }
                .encode(id(5), w)
                .unwrap())
            .is_err()
    );

    let mut harness = Harness::bound();
    harness.create_surface(5);
    harness
        .create_buffer(&[0; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    assert!(
        harness
            .send(|w| wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 1,
                y: 0
            }
            .encode(id(5), w)
            .unwrap())
            .is_err()
    );
    harness.expect_error(wl_surface::Error::InvalidOffset as u32);
}

#[test]
fn destroying_a_buffer_recycles_its_id() {
    let mut harness = Harness::bound();
    harness
        .create_buffer(&[0; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| wl_buffer::Request::Destroy.encode(id(11), w).unwrap())
        .unwrap();
    let events = harness.events(&[]);
    assert!(matches!(
        events[0].event,
        AnyEvent::WlDisplay(wl_display::Event::DeleteId { id: 11 })
    ));
    harness
        .send(|w| {
            wl_shm_pool::Request::CreateBuffer {
                id: id(11),
                offset: 0,
                width: 2,
                height: 2,
                stride: 8,
                format: wl_shm::Format::Argb8888,
            }
            .encode(id(10), w)
            .unwrap()
        })
        .unwrap();
}

#[test]
fn byte_at_a_time_delivery_is_equivalent() {
    let mut writer = Writer::new();
    wl_display::Request::GetRegistry { registry: id(2) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    wl_display::Request::Sync { callback: id(3) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    let mut whole = Client::new(OUTPUT);
    whole.receive(writer.bytes(), Vec::new()).unwrap();
    let mut split = Client::new(OUTPUT);
    for byte in writer.bytes() {
        split.receive(&[*byte], Vec::new()).unwrap();
    }
    assert_eq!(whole.output().0, split.output().0);
}

#[test]
fn fd_floods_and_random_garbage_never_panic() {
    let mut harness = Harness::new();
    let fds: Vec<OwnedFd> = (0..65).map(|_| shm_file(&[0])).collect();
    assert!(harness.client.receive(&[], fds).is_err());
    harness.expect_error(wl_display::Error::InvalidMethod as u32);

    let mut state = 0x1234_5678_9ABC_DEF1u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..200 {
        let mut harness = Harness::bound();
        let bytes: Vec<u8> = (0..(next() % 200) as usize).map(|_| next() as u8).collect();
        let _ = harness.client.receive(&bytes, Vec::new());
        // 无论结果如何，输出都必须是可解析的帧序列。
        let (output, _) = harness.client.output();
        let mut framer = Framer::new();
        framer.push(output);
        while framer.next_frame().unwrap().is_some() {}
    }
}

mod output;
mod seat;
#[cfg(target_os = "linux")]
mod seat_socket;
mod socket;
mod xdg;
