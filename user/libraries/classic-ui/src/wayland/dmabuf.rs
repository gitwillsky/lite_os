//! Client-side linux-dmabuf v4 negotiation and XRGB8888 stream buffer registration.

use super::*;
use std::{
    fs::File,
    os::{fd::BorrowedFd, unix::fs::FileExt},
};
use wayland_wire::protocol::{
    zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_feedback_v1 as feedback,
    zwp_linux_dmabuf_v1 as dma,
};
use wayland_wire::wire::Writer;

const XRGB: u32 = u32::from_le_bytes(*b"XR24");
const IMPLICIT: u64 = 0x00ff_ffff_ffff_ffff;

pub(super) enum Registration {
    Unsupported,
    Advertised {
        registry: ObjectId,
        name: u32,
    },
    Bound {
        manager: ObjectId,
        device: u64,
        selected: bool,
    },
}

impl Registration {
    pub(super) fn new(registry: ObjectId, globals: &[(u32, String, u32)]) -> Self {
        globals
            .iter()
            .find(|(_, name, version)| name == dma::NAME && *version >= 4)
            .map_or(Self::Unsupported, |(name, _, _)| Self::Advertised {
                registry,
                name: *name,
            })
    }

    pub(super) fn selected(&self) -> bool {
        matches!(self, Self::Bound { selected: true, .. })
    }
}

#[derive(Default)]
struct Formats {
    table: Vec<u8>,
    main: Option<u64>,
    target: Option<u64>,
    tranche_has_xrgb: bool,
    supported: bool,
}

fn device_number(bytes: Vec<u8>) -> Option<u64> {
    Some(u64::from_ne_bytes(bytes.try_into().ok()?))
}

impl Formats {
    fn update(&mut self, event: feedback::Event) -> io::Result<()> {
        use feedback::Event::*;
        match event {
            FormatTable { fd, size } => {
                // Indices are u16 and refer to consecutive 16-byte entries.
                if size > 16 * 65_536 || !size.is_multiple_of(16) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid dma-buf table",
                    ));
                }
                self.table.resize(size as usize, 0);
                File::from(fd).read_exact_at(&mut self.table, 0)?;
            }
            MainDevice { device } => self.main = device_number(device),
            TrancheTargetDevice { device } => self.target = device_number(device),
            TrancheFormats { indices } => {
                self.tranche_has_xrgb |= indices.as_chunks::<2>().0.iter().any(|index| {
                    let offset = usize::from(u16::from_ne_bytes(*index)) * 16;
                    self.table.get(offset..offset + 16).is_some_and(|entry| {
                        entry[..4] == XRGB.to_ne_bytes() && entry[8..] == IMPLICIT.to_ne_bytes()
                    })
                });
            }
            TrancheDone => {
                self.supported |=
                    self.target.is_some() && self.target == self.main && self.tranche_has_xrgb;
                self.target = None;
                self.tranche_has_xrgb = false;
            }
            _ => {}
        }
        Ok(())
    }
}

impl Display {
    /// Queries the compositor's main DRM device for implicit XRGB8888 imports.
    /// Returns None when the required format is absent, or an error for failed Wayland I/O.
    pub fn stream_dma_device(&mut self) -> io::Result<Option<u64>> {
        let (registry, name) = match self.stream_dma {
            Registration::Unsupported => return Ok(None),
            Registration::Bound { device, .. } => return Ok(Some(device)),
            Registration::Advertised { registry, name } => (registry, name),
        };
        let manager = self.transport.allocate(Interface::ZwpLinuxDmabufV1)?;
        wl_registry::Request::Bind {
            name,
            id: DynamicNewId {
                interface: dma::NAME.into(),
                version: 4,
                id: manager,
            },
        }
        .encode(registry, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        let id = self
            .transport
            .allocate(Interface::ZwpLinuxDmabufFeedbackV1)?;
        dma::Request::GetDefaultFeedback { id }
            .encode(manager, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        let mut formats = Formats::default();
        loop {
            let (object, event) = self.transport.blocking_event()?;
            match event {
                AnyEvent::ZwpLinuxDmabufFeedbackV1(feedback::Event::Done) if object == id => break,
                AnyEvent::ZwpLinuxDmabufFeedbackV1(event) if object == id => {
                    formats.update(event)?
                }
                _ => self.dispatch(object, event)?,
            }
        }
        feedback::Request::Destroy
            .encode(id, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        let device = formats.main.filter(|_| formats.supported);
        if let Some(device) = device {
            self.stream_dma = Registration::Bound {
                manager,
                device,
                selected: false,
            };
        } else {
            dma::Request::Destroy
                .encode(manager, &mut self.transport.writer)
                .map_err(io::Error::other)?;
            self.stream_dma = Registration::Unsupported;
        }
        self.transport.flush()?;
        Ok(device)
    }

    /// Selects dma-buf for all streams on this connection after the producer opened the device.
    /// Returns an error without an eligible device or if a stream already exists.
    pub fn use_dma_streams(&mut self) -> io::Result<()> {
        let Registration::Bound { selected, .. } = &mut self.stream_dma else {
            return Err(io::ErrorKind::Unsupported.into());
        };
        if !self.streams.is_empty() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        // A successful feedback query alone cannot establish that the producer opened
        // the DRM device. Until that succeeds, legacy and current callers submit SHM.
        *selected = true;
        Ok(())
    }

    pub(super) fn dma_stream_buffer(
        &mut self,
        fd: BorrowedFd<'_>,
        width: i32,
        height: i32,
        stride: i32,
    ) -> io::Result<ObjectId> {
        let Registration::Bound {
            manager,
            selected: true,
            ..
        } = self.stream_dma
        else {
            return Err(io::ErrorKind::Unsupported.into());
        };
        let params = self.transport.allocate(Interface::ZwpLinuxBufferParamsV1)?;
        dma::Request::CreateParams { params_id: params }
            .encode(manager, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        let mut request = Writer::new();
        params::Request::Add {
            fd: fd.try_clone_to_owned()?,
            plane_idx: 0,
            offset: 0,
            stride: stride as u32,
            modifier_hi: (IMPLICIT >> 32) as u32,
            modifier_lo: IMPLICIT as u32,
        }
        .encode(params, &mut request)
        .map_err(io::Error::other)?;
        self.transport.send_fd(request.bytes(), fd)?;
        let buffer = self.transport.allocate(Interface::WlBuffer)?;
        params::Request::CreateImmed {
            buffer_id: buffer,
            width,
            height,
            format: XRGB,
            flags: 0,
        }
        .encode(params, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        params::Request::Destroy
            .encode(params, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        Ok(buffer)
    }
}
