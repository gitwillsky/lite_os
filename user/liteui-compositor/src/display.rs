use core::{ffi::c_void, ptr};

use crate::{
    ffi::{self, DrmDumbCreate, DrmDumbMap, DrmMode},
    scene::{Rect, Scene},
};

mod damage;
use damage::DamageSet;
pub(crate) use damage::DamageRequest;

struct Buffer {
    framebuffer_id: u32,
    handle: u32,
    pixels: *mut u32,
    size: usize,
    pitch: usize,
    damage: DamageSet,
    // OWNER: prepare_damage 把本次像素 snapshot 移入 inflight，直到 presenter completion
    // 才提交或重新合并；缺失该 owner 会让 worker 阻塞期间到达的新输入被错误 clear。
    inflight: Option<DamageSet>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DisplayError {
    Transient,
    OutOfMemory,
    System,
}

pub struct Candidate {
    fd: i32,
    mode: DrmMode,
    buffer: Option<Buffer>,
}

pub struct Display {
    fd: i32,
    crtc_id: u32,
    connector_id: u32,
    mode: DrmMode,
    buffer: Option<Buffer>,
}

impl Display {
    pub fn open(fd: i32) -> Result<Self, ()> {
        let resources = unsafe { ffi::drmModeGetResources(fd) };
        if resources.is_null() {
            return Err(());
        }
        let value = unsafe { &*resources };
        if value.crtc_count <= 0
            || value.connector_count <= 0
            || value.crtc_ids.is_null()
            || value.connector_ids.is_null()
        {
            unsafe { ffi::drmModeFreeResources(resources) };
            return Err(());
        }
        let crtc_id = unsafe { *value.crtc_ids };
        let connector_id = unsafe { *value.connector_ids };
        unsafe { ffi::drmModeFreeResources(resources) };
        let mut display = Self {
            fd,
            crtc_id,
            connector_id,
            mode: DrmMode::default(),
            buffer: None,
        };
        display.mode = display.query_mode().map_err(|_| ())?;
        Ok(display)
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (
            usize::from(self.mode.hdisplay),
            usize::from(self.mode.vdisplay),
        )
    }

    pub fn activate(&mut self, scene: &Scene) -> Result<(), ()> {
        let mut candidate = self.prepare(self.mode, scene).map_err(|_| ())?;
        self.commit(&mut candidate).map_err(|_| ())
    }

    pub fn query_mode(&self) -> Result<DrmMode, DisplayError> {
        let connector = unsafe { ffi::drmModeGetConnector(self.fd, self.connector_id) };
        if connector.is_null() {
            return Err(system_error());
        }
        let value = unsafe { &*connector };
        if value.mode_count <= 0 || value.modes.is_null() {
            unsafe { ffi::drmModeFreeConnector(connector) };
            return Err(DisplayError::System);
        }
        let mode = unsafe { *value.modes };
        unsafe { ffi::drmModeFreeConnector(connector) };
        (mode.hdisplay != 0 && mode.vdisplay != 0)
            .then_some(mode)
            .ok_or(DisplayError::System)
    }

    pub fn mode_changed(&self, mode: DrmMode) -> bool {
        mode.hdisplay != self.mode.hdisplay || mode.vdisplay != self.mode.vdisplay
    }

    pub fn prepare(&self, mode: DrmMode, scene: &Scene) -> Result<Candidate, DisplayError> {
        Ok(Candidate {
            fd: self.fd,
            mode,
            buffer: Some(create_buffer(self.fd, mode, scene)?),
        })
    }

    pub fn commit(&mut self, candidate: &mut Candidate) -> Result<(), DisplayError> {
        if self
            .buffer
            .as_ref()
            .is_some_and(|buffer| buffer.inflight.is_some())
        {
            return Err(DisplayError::Transient);
        }
        let framebuffer_id = candidate
            .buffer
            .as_ref()
            .ok_or(DisplayError::System)?
            .framebuffer_id;
        set_crtc(
            self.fd,
            self.crtc_id,
            self.connector_id,
            candidate.mode,
            framebuffer_id,
        )?;
        let next = candidate.buffer.take();
        cleanup_buffer(self.fd, core::mem::replace(&mut self.buffer, next));
        self.mode = candidate.mode;
        Ok(())
    }
}

impl Drop for Candidate {
    fn drop(&mut self) {
        cleanup_buffer(self.fd, self.buffer.take());
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        cleanup_buffer(self.fd, self.buffer.take());
    }
}

fn create_buffer(fd: i32, mode: DrmMode, scene: &Scene) -> Result<Buffer, DisplayError> {
    let mut create = DrmDumbCreate {
        width: u32::from(mode.hdisplay),
        height: u32::from(mode.vdisplay),
        bpp: 32,
        ..DrmDumbCreate::default()
    };
    if unsafe {
        ffi::drmIoctl(
            fd,
            ffi::DRM_IOCTL_MODE_CREATE_DUMB,
            (&mut create as *mut DrmDumbCreate).cast(),
        )
    } < 0
    {
        return Err(system_error());
    }
    let size = match usize::try_from(create.size) {
        Ok(size) => size,
        Err(_) => {
            destroy_handle(fd, create.handle);
            return Err(DisplayError::System);
        }
    };
    let required = match usize::try_from(create.pitch)
        .ok()
        .and_then(|pitch| pitch.checked_mul(usize::from(mode.vdisplay)))
    {
        Some(required) => required,
        None => {
            destroy_handle(fd, create.handle);
            return Err(DisplayError::System);
        }
    };
    if required > size || create.pitch < u32::from(mode.hdisplay) * 4 {
        destroy_handle(fd, create.handle);
        return Err(DisplayError::System);
    }
    let mut map = DrmDumbMap {
        handle: create.handle,
        ..DrmDumbMap::default()
    };
    if unsafe {
        ffi::drmIoctl(
            fd,
            ffi::DRM_IOCTL_MODE_MAP_DUMB,
            (&mut map as *mut DrmDumbMap).cast(),
        )
    } < 0
    {
        let error = system_error();
        destroy_handle(fd, create.handle);
        return Err(error);
    }
    let pixels = unsafe {
        ffi::mmap(
            ptr::null_mut(),
            size,
            ffi::PROT_READ | ffi::PROT_WRITE,
            ffi::MAP_SHARED,
            fd,
            map.offset as i64,
        )
    };
    if pixels as usize == usize::MAX {
        let error = system_error();
        destroy_handle(fd, create.handle);
        return Err(error);
    }
    let mut framebuffer_id = 0;
    if unsafe {
        ffi::drmModeAddFB(
            fd,
            u32::from(mode.hdisplay),
            u32::from(mode.vdisplay),
            24,
            32,
            create.pitch,
            create.handle,
            &mut framebuffer_id,
        )
    } < 0
    {
        let error = system_error();
        unsafe { ffi::munmap(pixels, size) };
        destroy_handle(fd, create.handle);
        return Err(error);
    }
    let buffer = Buffer {
        framebuffer_id,
        handle: create.handle,
        pixels: pixels.cast(),
        size,
        pitch: create.pitch as usize,
        damage: DamageSet::EMPTY,
        inflight: None,
    };
    scene.render(
        buffer.pixels,
        buffer.pitch,
        Rect::full(usize::from(mode.hdisplay), usize::from(mode.vdisplay)),
    );
    Ok(buffer)
}

fn set_crtc(
    fd: i32,
    crtc_id: u32,
    connector_id: u32,
    mut mode: DrmMode,
    framebuffer_id: u32,
) -> Result<(), DisplayError> {
    let mut connector_id = connector_id;
    (unsafe {
        ffi::drmModeSetCrtc(
            fd,
            crtc_id,
            framebuffer_id,
            0,
            0,
            &mut connector_id,
            1,
            &mut mode,
        )
    } >= 0)
        .then_some(())
        .ok_or_else(system_error)
}

fn cleanup_buffer(fd: i32, buffer: Option<Buffer>) {
    let Some(buffer) = buffer else {
        return;
    };
    assert!(
        buffer.inflight.is_none(),
        "framebuffer destroyed while presenter still owns damage"
    );
    unsafe { ffi::drmModeRmFB(fd, buffer.framebuffer_id) };
    unsafe { ffi::munmap(buffer.pixels.cast::<c_void>(), buffer.size) };
    destroy_handle(fd, buffer.handle);
}

fn destroy_handle(fd: i32, handle: u32) {
    let mut handle = handle;
    unsafe {
        ffi::drmIoctl(
            fd,
            ffi::DRM_IOCTL_MODE_DESTROY_DUMB,
            (&mut handle as *mut u32).cast(),
        )
    };
}

fn system_error() -> DisplayError {
    match ffi::errno() {
        ffi::EBUSY | ffi::EINTR | ffi::EINVAL => DisplayError::Transient,
        ffi::ENOMEM => DisplayError::OutOfMemory,
        _ => DisplayError::System,
    }
}
