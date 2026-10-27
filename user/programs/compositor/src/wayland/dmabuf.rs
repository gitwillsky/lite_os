//! 标准 linux-dmabuf v4：单平面 ARGB/XRGB、隐式 modifier，直接 PRIME 导入 VirGL 纹理。
use super::{
    client::{Client, Data, Entry, Fail},
    surface::{Buffer, Content},
};
use linux_uapi::drm::{FORMAT_B8G8R8A8_UNORM, FORMAT_B8G8R8X8_UNORM, VirglResource};
use std::{
    cell::RefCell,
    fs::File,
    io::{self, Seek, SeekFrom},
    os::fd::{AsFd, OwnedFd},
    rc::{Rc, Weak},
};
use wayland_wire::{
    protocol::{
        Interface, wl_buffer, zwp_linux_buffer_params_v1 as params,
        zwp_linux_dmabuf_feedback_v1 as feedback, zwp_linux_dmabuf_v1 as dma,
    },
    wire::ObjectId,
};

const ARGB: u32 = u32::from_le_bytes(*b"AR24");
const XRGB: u32 = u32::from_le_bytes(*b"XR24");
const IMPLICIT: u64 = 0x00ff_ffff_ffff_ffff;
const FORMATS: [u32; 2] = [ARGB, XRGB];
pub(super) struct Plane {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
    modifier: u64,
}
pub(super) struct Params {
    planes: [Option<Plane>; 4],
    used: bool,
}
impl Default for Params {
    fn default() -> Self {
        Self {
            planes: std::array::from_fn(|_| None),
            used: false,
        }
    }
}
pub(super) struct DmaBuffer {
    pub(super) texture: Rc<VirglResource>,
    pub(super) y_invert: bool,
    // OWNER: 一个 wl_buffer 的所有当前/在途 Content 共用一次使用期；Weak 不形成 Buffer -> Use -> Buffer 环。
    usage: RefCell<Weak<BufferUse>>,
}
pub(super) type Releases = Rc<RefCell<Vec<(ObjectId, Rc<Buffer>)>>>;
/// 使用期跨越 surface 替换和 GPU 在途帧；最后一个引用退出后才允许发 buffer.release。
pub(super) struct BufferUse {
    pub(super) buffer: Rc<Buffer>,
    object: ObjectId,
    releases: Releases,
}
impl Drop for BufferUse {
    fn drop(&mut self) {
        self.releases
            .borrow_mut()
            .push((self.object, self.buffer.clone()));
    }
}
impl DmaBuffer {
    pub(super) fn content(
        &self,
        object: ObjectId,
        buffer: Rc<Buffer>,
        releases: &Releases,
    ) -> Content {
        let existing = self.usage.borrow().upgrade();
        let usage = existing.unwrap_or_else(|| {
            let usage = Rc::new(BufferUse {
                buffer,
                object,
                releases: releases.clone(),
            });
            *self.usage.borrow_mut() = Rc::downgrade(&usage);
            usage
        });
        Content::from_dma(usage, self.texture.clone())
    }
}
impl Client {
    pub(super) fn flush_buffer_releases(&mut self) {
        let released = std::mem::take(&mut *self.buffer_releases.borrow_mut());
        for (object, buffer) in released {
            let active = matches!(buffer.as_ref(), Buffer::Dma(dma) if dma.usage.borrow().strong_count() != 0);
            if !active
                && matches!(self.objects.get(&object.get()), Some(Entry { data: Data::Buffer(current), .. }) if Rc::ptr_eq(current, &buffer))
            {
                let _ = wl_buffer::Event::Release.encode(object, &mut self.writer);
            }
        }
    }
    pub(super) fn announce_dmabuf(&mut self, object: ObjectId, version: u32) -> Result<(), Fail> {
        if version < 4 {
            for format in FORMATS {
                dma::Event::Format { format }.encode(object, &mut self.writer)?;
                if version >= 3 {
                    dma::Event::Modifier {
                        format,
                        modifier_hi: (IMPLICIT >> 32) as u32,
                        modifier_lo: IMPLICIT as u32,
                    }
                    .encode(object, &mut self.writer)?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn dmabuf(
        &mut self,
        _object: ObjectId,
        version: u32,
        request: dma::Request,
    ) -> Result<(), Fail> {
        match request {
            dma::Request::Destroy => Ok(()),
            dma::Request::CreateParams { params_id } => self.create(
                params_id,
                Interface::ZwpLinuxBufferParamsV1,
                version,
                Data::DmaParams(Params::default()),
            ),
            dma::Request::GetDefaultFeedback { id } => self.dma_feedback(id),
            dma::Request::GetSurfaceFeedback { id, surface } => {
                self.surface_mut(surface)?;
                self.dma_feedback(id)
            }
        }
    }
    fn dma_feedback(&mut self, object: ObjectId) -> Result<(), Fail> {
        let context = self
            .gpu
            .as_ref()
            .ok_or_else(|| Fail::implementation(object))?;
        let device = context
            .device_number()
            .map_err(|_| Fail::implementation(object))?
            .to_ne_bytes()
            .to_vec();
        let mut table = Vec::with_capacity(32);
        for format in FORMATS {
            table.extend_from_slice(&format.to_ne_bytes());
            table.extend_from_slice(&[0; 4]);
            table.extend_from_slice(&IMPLICIT.to_ne_bytes());
        }
        // 只在发送前写一次；排队 FD 后不保留写入 owner，符合格式表发布后不得修改的协议契约。
        let memory = linux_uapi::shared_memory::MemFd::from_bytes("dmabuf-formats", &table)
            .map_err(|error| Fail::new(object, 3, &format!("dma-buf feedback table: {error}")))?;
        let fd = memory
            .as_fd()
            .try_clone_to_owned()
            .map_err(|_| Fail::implementation(object))?;
        self.create(object, Interface::ZwpLinuxDmabufFeedbackV1, 4, Data::None)?;
        let offset = self.writer.bytes().len();
        let event = feedback::Event::FormatTable {
            fd,
            size: table.len() as u32,
        };
        event.encode(object, &mut self.writer)?;
        self.writer.consume(0, 1);
        let feedback::Event::FormatTable { fd, .. } = event else {
            unreachable!()
        };
        self.outgoing_fds.push_back((offset, fd));
        feedback::Event::MainDevice {
            device: device.clone(),
        }
        .encode(object, &mut self.writer)?;
        feedback::Event::TrancheTargetDevice { device }.encode(object, &mut self.writer)?;
        feedback::Event::TrancheFlags { flags: 0 }.encode(object, &mut self.writer)?;
        let indices = [0u16.to_ne_bytes(), 1u16.to_ne_bytes()].concat();
        feedback::Event::TrancheFormats { indices }.encode(object, &mut self.writer)?;
        feedback::Event::TrancheDone.encode(object, &mut self.writer)?;
        feedback::Event::Done.encode(object, &mut self.writer)?;
        Ok(())
    }
    pub(super) fn dma_params(
        &mut self,
        object: ObjectId,
        version: u32,
        request: params::Request,
    ) -> Result<(), Fail> {
        use params::{Error as E, Request as R};
        if matches!(request, R::Destroy) {
            return Ok(());
        }
        let Some(Entry {
            data: Data::DmaParams(state),
            ..
        }) = self.objects.get_mut(&object.get())
        else {
            return Err(Fail::invalid_object("not dma-buf params"));
        };
        if state.used {
            return Err(Fail::new(
                object,
                E::AlreadyUsed as u32,
                "dma-buf params already consumed",
            ));
        }
        let (width, height, format, flags, id) = match request {
            R::Add {
                fd,
                plane_idx,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                let slot = state.planes.get_mut(plane_idx as usize).ok_or_else(|| {
                    Fail::new(object, E::PlaneIdx as u32, "plane index exceeds four")
                })?;
                if slot.is_some() {
                    return Err(Fail::new(
                        object,
                        E::PlaneSet as u32,
                        "plane already supplied",
                    ));
                }
                *slot = Some(Plane {
                    fd,
                    offset,
                    stride,
                    modifier: (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
                });
                return Ok(());
            }
            R::Create {
                width,
                height,
                format,
                flags,
            } => (width, height, format, flags, None),
            R::CreateImmed {
                buffer_id,
                width,
                height,
                format,
                flags,
            } => (width, height, format, flags, Some(buffer_id)),
            R::Destroy => unreachable!(),
        };
        state.used = true;
        let planes = std::mem::replace(&mut state.planes, std::array::from_fn(|_| None));
        let bad = |code: E, message: &str| Fail::new(object, code as u32, message);
        if width <= 0 || height <= 0 {
            return Err(bad(E::InvalidDimensions, "non-positive dma-buf dimensions"));
        }
        if !FORMATS.contains(&format) || flags & !1 != 0 {
            return Err(bad(E::InvalidFormat, "unsupported dma-buf format or flags"));
        }
        if planes[0].is_none() || planes[1..].iter().any(Option::is_some) {
            return Err(bad(E::Incomplete, "format requires exactly one plane"));
        }
        let plane = planes
            .into_iter()
            .next()
            .flatten()
            .expect("validated plane");
        if version >= 3 && plane.modifier != IMPLICIT {
            return Err(bad(E::InvalidFormat, "unsupported dma-buf modifier"));
        }
        let format = if format == ARGB {
            FORMAT_B8G8R8A8_UNORM
        } else {
            FORMAT_B8G8R8X8_UNORM
        };
        // out_of_bounds 只用于真实 FD 尺寸越界；driver 不支持的合法布局走标准导入失败语义。
        enum ImportError {
            Bounds,
            Platform(io::Error),
        }
        let imported = (|| {
            let size = File::from(plane.fd.try_clone().map_err(ImportError::Platform)?)
                .seek(SeekFrom::End(0))
                .map_err(ImportError::Platform)?;
            let end = u64::from(plane.offset) + u64::from(plane.stride) * height as u64;
            if end > size {
                return Err(ImportError::Bounds);
            }
            if plane.offset != 0 {
                return Err(ImportError::Platform(io::ErrorKind::Unsupported.into()));
            }
            self.gpu
                .as_ref()
                .ok_or_else(|| ImportError::Platform(io::ErrorKind::Unsupported.into()))?
                .import_texture(
                    plane.fd.as_fd(),
                    width as u32,
                    height as u32,
                    plane.stride,
                    format,
                )
                .map_err(ImportError::Platform)
        })();
        let texture = match imported {
            Ok(texture) => texture,
            Err(ImportError::Bounds) => {
                return Err(bad(E::OutOfBounds, "dma-buf plane exceeds file size"));
            }
            Err(ImportError::Platform(_)) if id.is_none() => {
                params::Event::Failed.encode(object, &mut self.writer)?;
                return Ok(());
            }
            Err(ImportError::Platform(error)) => {
                return Err(bad(
                    E::InvalidWlBuffer,
                    &format!("dma-buf import failed: {error}"),
                ));
            }
        };
        let buffer = Data::Buffer(Rc::new(Buffer::Dma(DmaBuffer {
            texture,
            y_invert: flags & 1 != 0,
            usage: RefCell::new(Weak::new()),
        })));
        if let Some(id) = id {
            self.create(id, Interface::WlBuffer, 1, buffer)?;
        } else {
            let id = ObjectId::new(self.next_server_object)
                .ok_or_else(|| Fail::implementation(object))?;
            self.next_server_object = self
                .next_server_object
                .checked_add(1)
                .ok_or_else(|| Fail::implementation(object))?;
            self.objects.insert(
                id.get(),
                Entry {
                    interface: Interface::WlBuffer,
                    version: 1,
                    data: buffer,
                },
            );
            params::Event::Created { buffer: id }.encode(object, &mut self.writer)?;
        }
        Ok(())
    }
}
