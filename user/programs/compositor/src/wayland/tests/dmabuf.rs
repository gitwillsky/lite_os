use super::*;
use crate::wayland::{client::Data, dmabuf::Params};
use wayland_wire::protocol::zwp_linux_buffer_params_v1::{Error, Request};

fn params() -> Harness {
    let mut h = Harness::new();
    assert!(
        h.client
            .create(
                id(2),
                Interface::ZwpLinuxBufferParamsV1,
                4,
                Data::DmaParams(Params::default())
            )
            .is_ok()
    );
    h
}

#[test]
fn dma_params_reject_duplicate_planes_and_consume_received_fds() {
    let mut h = params();
    for attempt in 0..2 {
        let fd = shm_file(&[0; 64]);
        let request = Request::Add {
            fd,
            plane_idx: 0,
            offset: 0,
            stride: 16,
            modifier_hi: 0x00ff_ffff,
            modifier_lo: u32::MAX,
        };
        let result = h.send_with_fds(
            |w| request.encode(id(2), w).unwrap(),
            vec![request_fd(&request)],
        );
        if attempt == 0 {
            result.unwrap();
        } else {
            assert!(result.is_err());
        }
    }
    h.expect_error(Error::PlaneSet as u32);
}

fn request_fd(request: &Request) -> OwnedFd {
    let Request::Add { fd, .. } = request else {
        unreachable!()
    };
    fd.try_clone().unwrap()
}

#[test]
fn dma_params_reject_incomplete_and_invalid_dimensions_before_gpu_import() {
    for (width, expected) in [(0, Error::InvalidDimensions), (4, Error::Incomplete)] {
        let mut h = params();
        assert!(
            h.send(|w| Request::Create {
                width,
                height: 4,
                format: u32::from_le_bytes(*b"AR24"),
                flags: 0
            }
            .encode(id(2), w)
            .unwrap())
                .is_err()
        );
        h.expect_error(expected as u32);
    }
}

#[test]
fn cpu_compositor_does_not_advertise_gpu_import() {
    let mut h = Harness::new();
    h.send(|w| {
        wl_display::Request::GetRegistry { registry: id(2) }
            .encode(id(1), w)
            .unwrap()
    })
    .unwrap();
    for event in h.events(&[(2, Interface::WlRegistry)]) {
        if let AnyEvent::WlRegistry(wl_registry::Event::Global { interface, .. }) = event.event {
            assert_ne!(interface, "zwp_linux_dmabuf_v1");
        }
    }
}

#[test]
fn dma_plane_bounds_and_unsupported_layout_have_distinct_protocol_results() {
    for (length, expected) in [(8, Some(Error::OutOfBounds)), (128, None)] {
        let mut h = params();
        let request = Request::Add {
            fd: shm_file(&vec![0; length]),
            plane_idx: 0,
            offset: 4,
            stride: 16,
            modifier_hi: 0x00ff_ffff,
            modifier_lo: u32::MAX,
        };
        h.send_with_fds(
            |w| request.encode(id(2), w).unwrap(),
            vec![request_fd(&request)],
        )
        .unwrap();
        let result = h.send(|w| {
            Request::Create {
                width: 4,
                height: 4,
                format: u32::from_le_bytes(*b"AR24"),
                flags: 0,
            }
            .encode(id(2), w)
            .unwrap()
        });
        if let Some(error) = expected {
            assert!(result.is_err());
            h.expect_error(error as u32);
        } else {
            result.unwrap();
            assert!(matches!(
                &h.events(&[(2, Interface::ZwpLinuxBufferParamsV1)])[0].event,
                AnyEvent::ZwpLinuxBufferParamsV1(
                    wayland_wire::protocol::zwp_linux_buffer_params_v1::Event::Failed
                )
            ));
        }
    }
}
