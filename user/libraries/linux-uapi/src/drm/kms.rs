//! Standard KMS plane/property discovery and immutable property-blob ownership.
use super::{DrmDevice, raw};
use std::{io, num::NonZeroU32};

/// A mode object type accepted by `OBJ_GETPROPERTIES`.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum KmsObject {
    Crtc = 0xcccccccc,
    Connector = 0xc0c0c0c0,
    Plane = 0xeeeeeeee,
}
/// One primary/overlay/cursor plane snapshot; IDs refer to the same DRM device.
pub struct Plane {
    pub id: u32,
    pub crtc: u32,
    pub framebuffer: u32,
    pub possible_crtcs: u32,
    pub formats: Vec<u32>,
}
/// Standard property metadata; the current object value comes from `properties`.
pub struct Property {
    pub id: u32,
    pub name: String,
    pub flags: u32,
    pub values: Vec<u64>,
    pub enums: Vec<(u64, String)>,
}
/// The creator's reference to one immutable KMS property blob.
///
/// Keeps its DRM OFD alive; Drop releases this reference, while submitted KMS states
/// retain their own references. This is unrelated to VirtIO resource blobs.
pub struct PropertyBlob {
    device: DrmDevice,
    // OWNER: None only after successful explicit destruction or while Drop owns the deletion.
    // Retaining the ID after destruction could delete an unrelated future object on another DRM implementation.
    id: Option<NonZeroU32>,
}
impl PropertyBlob {
    /// Returns the live device-wide mode object ID for property assignment.
    pub fn id(&self) -> u32 {
        self.id
            .expect("live property blob lost creator reference")
            .get()
    }
    /// Explicitly releases the creator reference and reports the ioctl result.
    ///
    /// # Errors
    /// Returns the kernel error; Drop retries cleanup if explicit destruction failed.
    pub fn destroy(mut self) -> io::Result<()> {
        let id = self.id();
        self.device.destroy_blob_id(id)?;
        self.id = None;
        Ok(())
    }
}
impl Drop for PropertyBlob {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.device.destroy_blob_id(id.get());
        }
    }
}
fn name(bytes: &[u8; 32]) -> String {
    String::from_utf8_lossy(bytes.split(|byte| *byte == 0).next().unwrap_or_default()).into_owned()
}
impl DrmDevice {
    /// Requests universal plane visibility on this OFD (not on unrelated opens).
    /// # Errors
    /// Returns the standard SET_CLIENT_CAP error when unsupported or invalid.
    pub fn set_universal_planes(&self, enabled: bool) -> io::Result<()> {
        let mut cap = raw::DrmClientCap {
            capability: 2,
            value: u64::from(enabled),
        };
        self.ioctl(raw::DRM_IOCTL_SET_CLIENT_CAP, (&raw mut cap).cast())
    }
    /// Enables standard atomic KMS and universal plane visibility on this OFD.
    /// # Errors
    /// Returns SET_CLIENT_CAP errors; unsupported devices must not silently accept it.
    pub fn set_atomic(&self, enabled: bool) -> io::Result<()> {
        let mut cap = raw::DrmClientCap {
            capability: 3,
            value: u64::from(enabled),
        };
        self.ioctl(raw::DRM_IOCTL_SET_CLIENT_CAP, (&raw mut cap).cast())
    }
    /// Enumerates currently visible plane IDs using the standard two-call query.
    /// # Errors
    /// Returns allocation/ioctl errors; retries if the list grows between calls.
    pub fn plane_ids(&self) -> io::Result<Vec<u32>> {
        let mut ids = Vec::new();
        loop {
            let mut request = raw::DrmPlaneResources {
                ids: ids.as_mut_ptr() as u64,
                count: ids.len() as u32,
                padding: 0,
            };
            self.ioctl(
                raw::DRM_IOCTL_MODE_GETPLANERESOURCES,
                (&raw mut request).cast(),
            )?;
            if request.count as usize <= ids.len() {
                ids.truncate(request.count as usize);
                return Ok(ids);
            }
            ids.try_reserve_exact(request.count as usize - ids.len())
                .map_err(io::Error::other)?;
            ids.resize(request.count as usize, 0);
        }
    }
    /// Queries the plane's binding, possible-CRTC bitmap and DRM fourcc formats.
    /// # Errors
    /// Returns ENOENT, allocation or ioctl errors; retries a growing format list.
    pub fn plane(&self, id: u32) -> io::Result<Plane> {
        let mut formats = Vec::new();
        loop {
            let mut request = raw::DrmPlane {
                id,
                formats: formats.as_mut_ptr() as u64,
                format_count: formats.len() as u32,
                ..Default::default()
            };
            self.ioctl(raw::DRM_IOCTL_MODE_GETPLANE, (&raw mut request).cast())?;
            if request.format_count as usize <= formats.len() {
                formats.truncate(request.format_count as usize);
                return Ok(Plane {
                    id: request.id,
                    crtc: request.crtc,
                    framebuffer: request.framebuffer,
                    possible_crtcs: request.possible_crtcs,
                    formats,
                });
            }
            formats
                .try_reserve_exact(request.format_count as usize - formats.len())
                .map_err(io::Error::other)?;
            formats.resize(request.format_count as usize, 0);
        }
    }
    /// Reads `(property ID, current value)` pairs for a typed mode object.
    /// # Errors
    /// Returns wrong-object-type, allocation or ioctl errors; retries list growth.
    pub fn properties(&self, object: u32, kind: KmsObject) -> io::Result<Vec<(u32, u64)>> {
        let (mut ids, mut values) = (Vec::new(), Vec::new());
        loop {
            let mut request = raw::DrmObjectProperties {
                ids: ids.as_mut_ptr() as u64,
                values: values.as_mut_ptr() as u64,
                count: ids.len() as u32,
                object,
                kind: kind as u32,
                padding: 0,
            };
            self.ioctl(
                raw::DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
                (&raw mut request).cast(),
            )?;
            if request.count as usize <= ids.len() {
                return Ok(ids
                    .into_iter()
                    .zip(values)
                    .take(request.count as usize)
                    .collect());
            }
            let count = request.count as usize;
            ids.try_reserve_exact(count - ids.len())
                .map_err(io::Error::other)?;
            values
                .try_reserve_exact(count - values.len())
                .map_err(io::Error::other)?;
            ids.resize(count, 0);
            values.resize(count, 0);
        }
    }
    /// Reads a property's name, flags and range/enum/object metadata.
    /// # Errors
    /// Returns ENOENT, allocation or ioctl errors; blob payloads use `property_blob`.
    pub fn property(&self, id: u32) -> io::Result<Property> {
        let (mut values, mut enums) = (Vec::new(), Vec::new());
        loop {
            let mut request = raw::DrmProperty {
                id,
                values: values.as_mut_ptr() as u64,
                enums: enums.as_mut_ptr() as u64,
                value_count: values.len() as u32,
                enum_count: enums.len() as u32,
                ..Default::default()
            };
            self.ioctl(raw::DRM_IOCTL_MODE_GETPROPERTY, (&raw mut request).cast())?;
            let (count, enum_count) = (request.value_count as usize, request.enum_count as usize);
            if count <= values.len() && enum_count <= enums.len() {
                values.truncate(count);
                return Ok(Property {
                    id,
                    name: name(&request.name),
                    flags: request.flags,
                    values,
                    enums: enums
                        .into_iter()
                        .take(enum_count)
                        .map(|entry: raw::DrmPropertyEnum| (entry.value, name(&entry.name)))
                        .collect(),
                });
            }
            values
                .try_reserve_exact(count.saturating_sub(values.len()))
                .map_err(io::Error::other)?;
            enums
                .try_reserve_exact(enum_count.saturating_sub(enums.len()))
                .map_err(io::Error::other)?;
            values.resize(count, 0);
            enums.resize(enum_count, raw::DrmPropertyEnum::default());
        }
    }
    /// Copies bytes once into a new immutable standard property blob.
    /// # Errors
    /// Returns a size/construction/ioctl error; returned ownership uses this OFD.
    pub fn create_property_blob(&self, bytes: &[u8]) -> io::Result<PropertyBlob> {
        let mut request = raw::DrmCreateBlob {
            data: bytes.as_ptr() as u64,
            length: u32::try_from(bytes.len()).map_err(io::Error::other)?,
            id: 0,
        };
        self.ioctl(
            raw::DRM_IOCTL_MODE_CREATEPROPBLOB,
            (&raw mut request).cast(),
        )?;
        let id = NonZeroU32::new(request.id)
            .ok_or_else(|| io::Error::other("DRM returned a null blob ID"))?;
        Ok(PropertyBlob {
            device: self.clone(),
            id: Some(id),
        })
    }
    /// Reads a live device-wide property blob without taking the creator reference.
    /// # Errors
    /// Returns ENOENT, allocation or ioctl errors; object IDs cannot be assumed live.
    pub fn property_blob(&self, id: u32) -> io::Result<Vec<u8>> {
        let mut request = raw::DrmGetBlob {
            id,
            ..Default::default()
        };
        self.ioctl(raw::DRM_IOCTL_MODE_GETPROPBLOB, (&raw mut request).cast())?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(request.length as usize)
            .map_err(io::Error::other)?;
        bytes.resize(request.length as usize, 0);
        request.data = bytes.as_mut_ptr() as u64;
        self.ioctl(raw::DRM_IOCTL_MODE_GETPROPBLOB, (&raw mut request).cast())?;
        if request.length as usize != bytes.len() {
            return Err(io::Error::other("DRM blob size changed"));
        }
        Ok(bytes)
    }
    fn destroy_blob_id(&self, mut id: u32) -> io::Result<()> {
        loop {
            match self.ioctl(raw::DRM_IOCTL_MODE_DESTROYPROPBLOB, (&raw mut id).cast()) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}
