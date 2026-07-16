use core::{ffi::c_void, ptr};

use crate::{
    ffi::{self, DrmClip, DrmDumbCreate, DrmDumbMap, DrmMode},
    scene::{Rect, Scene},
};

const MAX_DAMAGE_RECTS: usize = 32;

struct Buffer {
    framebuffer_id: u32,
    handle: u32,
    pixels: *mut u32,
    size: usize,
    pitch: usize,
    damage: DamageSet,
    // DIRTYFB 成功后，inactive resource 已可直接 SET_SCANOUT；缺失该 fact 会在 page-flip
    // 因 EBUSY/EINTR 重试时重复传输相同 geometry damage。
    prepared_for_flip: bool,
}

#[derive(Clone, Copy)]
struct DamageSet {
    rectangles: [Rect; MAX_DAMAGE_RECTS],
    count: usize,
}

impl DamageSet {
    const EMPTY: Self = Self {
        rectangles: [Rect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        }; MAX_DAMAGE_RECTS],
        count: 0,
    };

    fn push(&mut self, mut rectangle: Rect) {
        if rectangle.x1 >= rectangle.x2 || rectangle.y1 >= rectangle.y2 {
            return;
        }
        // 1. 先合并相交或相邻区域，避免同一帧重复传输重叠像素。
        let mut index = 0;
        while index < self.count {
            if touches(self.rectangles[index], rectangle) {
                rectangle = rectangle.union(self.rectangles[index]);
                self.count -= 1;
                self.rectangles[index] = self.rectangles[self.count];
                index = 0;
            } else {
                index += 1;
            }
        }
        // 2. 有空位时保留离散矩形，pointer 只传输旧、新光标覆盖的像素。
        if self.count < MAX_DAMAGE_RECTS {
            self.rectangles[self.count] = rectangle;
            self.count += 1;
            return;
        }
        // 3. 固定数组耗尽时合并为单一区域，保持无分配且不丢失 damage。
        for current in &self.rectangles {
            rectangle = rectangle.union(*current);
        }
        self.rectangles[0] = rectangle;
        self.count = 1;
    }

    fn clear(&mut self) {
        self.count = 0;
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn rectangles(&self) -> &[Rect] {
        &self.rectangles[..self.count]
    }
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
    buffers: [Option<Buffer>; 2],
}

pub struct Display {
    fd: i32,
    crtc_id: u32,
    connector_id: u32,
    mode: DrmMode,
    buffers: [Option<Buffer>; 2],
    front: usize,
    flip_pending: bool,
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
            buffers: [None, None],
            front: 0,
            flip_pending: false,
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
            buffers: create_pair(self.fd, mode, scene)?,
        })
    }

    pub fn commit(&mut self, candidate: &mut Candidate) -> Result<(), DisplayError> {
        if self.flip_pending {
            return Err(DisplayError::Transient);
        }
        let framebuffer_id = candidate.buffers[0]
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
        let next = core::mem::take(&mut candidate.buffers);
        cleanup_pair(self.fd, core::mem::replace(&mut self.buffers, next));
        self.mode = candidate.mode;
        self.front = 0;
        self.flip_pending = false;
        Ok(())
    }

    pub fn damage(&mut self, rectangle: Rect) {
        for buffer in self.buffers.iter_mut().flatten() {
            buffer.damage.push(rectangle);
            buffer.prepared_for_flip = false;
        }
    }

    pub fn has_active_damage(&self) -> bool {
        self.buffers[self.front]
            .as_ref()
            .is_some_and(|buffer| !buffer.damage.is_empty())
    }

    pub fn has_flip_damage(&self) -> bool {
        self.buffers[self.front ^ 1]
            .as_ref()
            .is_some_and(|buffer| !buffer.damage.is_empty())
    }

    pub fn flip_pending(&self) -> bool {
        self.flip_pending
    }

    pub fn present_damage(&mut self, scene: &Scene) -> Result<bool, ()> {
        if self.flip_pending {
            return Ok(false);
        }
        let Some(buffer) = self.buffers[self.front].as_mut() else {
            return Err(());
        };
        if buffer.damage.is_empty() {
            return Ok(false);
        }
        let damage = buffer.damage;
        let mut clips = [DrmClip::default(); MAX_DAMAGE_RECTS];
        for (index, rectangle) in damage.rectangles().iter().copied().enumerate() {
            scene.render(buffer.pixels, buffer.pitch, rectangle);
            clips[index] = clip(rectangle)?;
        }
        if unsafe {
            ffi::drmModeDirtyFB(
                self.fd,
                buffer.framebuffer_id,
                clips.as_mut_ptr(),
                damage.count as u32,
            )
        } < 0
        {
            return if matches!(ffi::errno(), ffi::EBUSY | ffi::EINTR) {
                Ok(false)
            } else {
                Err(())
            };
        }
        buffer.damage.clear();
        Ok(true)
    }

    pub fn present_flip(&mut self, scene: &Scene) -> Result<bool, ()> {
        if self.flip_pending {
            return Ok(false);
        }
        let back = self.front ^ 1;
        let Some(buffer) = self.buffers[back].as_mut() else {
            return Err(());
        };
        if buffer.damage.is_empty() {
            return Ok(false);
        }
        let damage = buffer.damage;
        if !buffer.prepared_for_flip {
            let mut clips = [DrmClip::default(); MAX_DAMAGE_RECTS];
            for (index, rectangle) in damage.rectangles().iter().copied().enumerate() {
                scene.render(buffer.pixels, buffer.pitch, rectangle);
                clips[index] = clip(rectangle)?;
            }
            if unsafe {
                ffi::drmModeDirtyFB(
                    self.fd,
                    buffer.framebuffer_id,
                    clips.as_mut_ptr(),
                    damage.count as u32,
                )
            } < 0
            {
                return if matches!(ffi::errno(), ffi::EBUSY | ffi::EINTR) {
                    Ok(false)
                } else {
                    Err(())
                };
            }
            buffer.prepared_for_flip = true;
        }
        if unsafe {
            ffi::drmModePageFlip(
                self.fd,
                self.crtc_id,
                buffer.framebuffer_id,
                ffi::DRM_MODE_PAGE_FLIP_EVENT,
                ptr::null_mut(),
            )
        } < 0
        {
            return if matches!(ffi::errno(), ffi::EBUSY | ffi::EINTR | ffi::EINVAL) {
                Ok(false)
            } else {
                Err(())
            };
        }
        buffer.damage.clear();
        buffer.prepared_for_flip = false;
        self.flip_pending = true;
        Ok(true)
    }

    pub fn read_events(&mut self) -> Result<(), ()> {
        let mut bytes = [0u8; 256];
        let count = unsafe { ffi::read(self.fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count <= 0 {
            return Err(());
        }
        let mut offset = 0usize;
        while offset < count as usize {
            let header = bytes.get(offset..offset + 8).ok_or(())?;
            let kind = u32::from_ne_bytes(header[..4].try_into().map_err(|_| ())?);
            let length = u32::from_ne_bytes(header[4..8].try_into().map_err(|_| ())?) as usize;
            if length < 8
                || offset
                    .checked_add(length)
                    .is_none_or(|end| end > count as usize)
            {
                return Err(());
            }
            if kind == ffi::DRM_EVENT_FLIP_COMPLETE {
                if !self.flip_pending {
                    return Err(());
                }
                self.front ^= 1;
                self.flip_pending = false;
            }
            offset += length;
        }
        Ok(())
    }
}

impl Drop for Candidate {
    fn drop(&mut self) {
        cleanup_pair(self.fd, core::mem::take(&mut self.buffers));
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        cleanup_pair(self.fd, core::mem::take(&mut self.buffers));
    }
}

fn create_pair(fd: i32, mode: DrmMode, scene: &Scene) -> Result<[Option<Buffer>; 2], DisplayError> {
    let mut pair = [None, None];
    pair[0] = Some(create_buffer(fd, mode, scene)?);
    pair[1] = match create_buffer(fd, mode, scene) {
        Ok(buffer) => Some(buffer),
        Err(error) => {
            cleanup_pair(fd, pair);
            return Err(error);
        }
    };
    Ok(pair)
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
        prepared_for_flip: false,
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

fn cleanup_pair(fd: i32, pair: [Option<Buffer>; 2]) {
    for buffer in pair.into_iter().flatten() {
        unsafe { ffi::drmModeRmFB(fd, buffer.framebuffer_id) };
        unsafe { ffi::munmap(buffer.pixels.cast::<c_void>(), buffer.size) };
        destroy_handle(fd, buffer.handle);
    }
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

fn clip(rectangle: Rect) -> Result<DrmClip, ()> {
    Ok(DrmClip {
        x1: u16::try_from(rectangle.x1).map_err(|_| ())?,
        y1: u16::try_from(rectangle.y1).map_err(|_| ())?,
        x2: u16::try_from(rectangle.x2).map_err(|_| ())?,
        y2: u16::try_from(rectangle.y2).map_err(|_| ())?,
    })
}

fn touches(first: Rect, second: Rect) -> bool {
    first.x1 <= second.x2 && second.x1 <= first.x2 && first.y1 <= second.y2 && second.y1 <= first.y2
}
