//! Standard atomic KMS request assembly; storage is retained between frames.
use super::{DrmDevice, Mode, PropertyBlob, raw};
use std::io;

/// One standard `drm_mode_atomic` transaction; repeated object/property assignments are legal.
#[derive(Default)]
pub struct AtomicRequest {
    objects: Vec<u32>,
    counts: Vec<u32>,
    properties: Vec<u32>,
    values: Vec<u64>,
}
impl AtomicRequest {
    pub const EVENT: u32 = 1;
    pub const TEST_ONLY: u32 = 0x100;
    pub const NONBLOCK: u32 = 0x200;
    pub const ALLOW_MODESET: u32 = 0x400;

    /// Removes assignments while retaining allocation capacity.
    pub fn clear(&mut self) {
        self.objects.clear();
        self.counts.clear();
        self.properties.clear();
        self.values.clear();
    }
    /// Appends an assignment using IDs discovered from this device's property queries.
    /// # Errors
    /// Reports count overflow or allocation failure without partially adding an assignment.
    pub fn set(&mut self, object: u32, property: u32, value: u64) -> io::Result<()> {
        let new_object = self.objects.last() != Some(&object);
        if self.properties.len() == u32::MAX as usize || self.objects.len() == u32::MAX as usize {
            return Err(io::Error::other("atomic request count overflow"));
        }
        self.properties.try_reserve(1).map_err(io::Error::other)?;
        self.values.try_reserve(1).map_err(io::Error::other)?;
        if new_object {
            self.objects.try_reserve(1).map_err(io::Error::other)?;
            self.counts.try_reserve(1).map_err(io::Error::other)?;
            self.objects.push(object);
            self.counts.push(0);
        }
        *self
            .counts
            .last_mut()
            .expect("atomic object has a property count") += 1;
        self.properties.push(property);
        self.values.push(value);
        Ok(())
    }
}
impl DrmDevice {
    /// Submits or checks one standard atomic transaction.
    /// # Parameters
    /// `flags` combines `AtomicRequest` constants; `user_data` is echoed by EVENT completion.
    /// # Errors
    /// Reports kernel validation, authorization, busy, memory or interrupted-wait errors.
    pub fn atomic_commit(
        &self,
        changes: &AtomicRequest,
        flags: u32,
        user_data: u64,
    ) -> io::Result<()> {
        let mut request = raw::DrmAtomic {
            flags,
            count: changes.objects.len() as u32,
            objects: changes.objects.as_ptr() as u64,
            counts: changes.counts.as_ptr() as u64,
            properties: changes.properties.as_ptr() as u64,
            values: changes.values.as_ptr() as u64,
            reserved: 0,
            user_data,
        };
        self.ioctl(raw::DRM_IOCTL_MODE_ATOMIC, (&raw mut request).cast())
    }
}
impl Mode {
    /// Creates a standard MODE_ID blob, explicitly encoding the fixed modeinfo layout.
    /// # Errors
    /// Returns the DRM blob allocation/ioctl error.
    pub fn property_blob(self, device: &DrmDevice) -> io::Result<PropertyBlob> {
        let mode = self.0;
        let mut bytes = [0u8; 68];
        bytes[..4].copy_from_slice(&mode.clock.to_ne_bytes());
        for (index, value) in [
            mode.hdisplay,
            mode.hsync_start,
            mode.hsync_end,
            mode.htotal,
            mode.hskew,
            mode.vdisplay,
            mode.vsync_start,
            mode.vsync_end,
            mode.vtotal,
            mode.vscan,
        ]
        .into_iter()
        .enumerate()
        {
            bytes[4 + index * 2..6 + index * 2].copy_from_slice(&value.to_ne_bytes());
        }
        for (offset, value) in [(24, mode.vrefresh), (28, mode.flags), (32, mode.mode_type)] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }
        bytes[36..].copy_from_slice(&mode.name);
        device.create_property_blob(&bytes)
    }
}
