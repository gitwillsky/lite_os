use core::{ffi::c_void, ptr, slice};

use crate::{
    atlas::{self, Atlas, FontMetrics},
    ffi::{self, DrmClip, DrmDumbCreate, DrmDumbMap, DrmMode},
    model::{
        ATTR_BLINK, ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_INVERSE, ATTR_UNDERLINE, Cell, Grid,
        Model,
    },
};

const MAX_CLIPS: usize = 32;
const BACKGROUND: u32 = 0x00101418;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DisplayError {
    System,
    Transient,
    OutOfMemory,
    OverBudget,
}

pub struct Display {
    fd: i32,
    crtc_id: u32,
    connector_id: u32,
    active: Option<Buffer>,
    framebuffer_budget: usize,
}

pub struct CandidateBuffer {
    fd: i32,
    buffer: Option<Buffer>,
}

struct Buffer {
    framebuffer_id: u32,
    handle: u32,
    pixels: *mut u32,
    size: usize,
    pitch: usize,
    width: usize,
    height: usize,
    mode: DrmMode,
}

impl Display {
    pub fn open(fd: i32) -> Result<Self, DisplayError> {
        let resources = unsafe { ffi::drmModeGetResources(fd) };
        if resources.is_null() {
            return Err(system_error());
        }
        let resources_ref = unsafe { &*resources };
        if resources_ref.crtc_count <= 0
            || resources_ref.connector_count <= 0
            || resources_ref.crtc_ids.is_null()
            || resources_ref.connector_ids.is_null()
        {
            unsafe { ffi::drmModeFreeResources(resources) };
            return Err(DisplayError::System);
        }
        let crtc_id = unsafe { *resources_ref.crtc_ids };
        let connector_id = unsafe { *resources_ref.connector_ids };
        unsafe { ffi::drmModeFreeResources(resources) };
        Ok(Self {
            fd,
            crtc_id,
            connector_id,
            active: None,
            framebuffer_budget: framebuffer_budget(),
        })
    }

    pub fn query_mode(&self) -> Result<DrmMode, DisplayError> {
        let connector = unsafe { ffi::drmModeGetConnector(self.fd, self.connector_id) };
        if connector.is_null() {
            return Err(system_error());
        }
        let connector_ref = unsafe { &*connector };
        if connector_ref.mode_count <= 0 || connector_ref.modes.is_null() {
            unsafe { ffi::drmModeFreeConnector(connector) };
            return Err(DisplayError::System);
        }
        let mode = unsafe { *connector_ref.modes };
        unsafe { ffi::drmModeFreeConnector(connector) };
        if mode.hdisplay == 0 || mode.vdisplay == 0 {
            return Err(DisplayError::System);
        }
        Ok(mode)
    }

    pub fn prepare<G: Grid>(
        &self,
        mode: DrmMode,
        grid: &G,
        atlas: &Atlas,
        metrics: FontMetrics,
    ) -> Result<CandidateBuffer, DisplayError> {
        let bytes = usize::from(mode.hdisplay)
            .checked_mul(usize::from(mode.vdisplay))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(DisplayError::System)?;
        if bytes > self.framebuffer_budget {
            return Err(DisplayError::OverBudget);
        }
        let mut create = DrmDumbCreate {
            width: u32::from(mode.hdisplay),
            height: u32::from(mode.vdisplay),
            bpp: 32,
            ..DrmDumbCreate::default()
        };
        if unsafe {
            ffi::drmIoctl(
                self.fd,
                ffi::DRM_IOCTL_MODE_CREATE_DUMB,
                (&mut create as *mut DrmDumbCreate).cast(),
            )
        } < 0
        {
            return Err(system_error());
        }
        let size = usize::try_from(create.size).map_err(|_| DisplayError::System)?;
        let required = usize::try_from(create.pitch)
            .ok()
            .and_then(|pitch| pitch.checked_mul(usize::from(mode.vdisplay)))
            .ok_or(DisplayError::System)?;
        if create.pitch < u32::from(mode.hdisplay) * 4 || required > size {
            destroy_handle(self.fd, create.handle);
            return Err(DisplayError::System);
        }
        if size > self.framebuffer_budget {
            destroy_handle(self.fd, create.handle);
            return Err(DisplayError::OverBudget);
        }
        let mut map = DrmDumbMap {
            handle: create.handle,
            ..DrmDumbMap::default()
        };
        if unsafe {
            ffi::drmIoctl(
                self.fd,
                ffi::DRM_IOCTL_MODE_MAP_DUMB,
                (&mut map as *mut DrmDumbMap).cast(),
            )
        } < 0
        {
            let error = system_error();
            destroy_handle(self.fd, create.handle);
            return Err(error);
        }
        let pixels = unsafe {
            ffi::mmap(
                ptr::null_mut(),
                size,
                ffi::PROT_READ | ffi::PROT_WRITE,
                ffi::MAP_SHARED,
                self.fd,
                map.offset as i64,
            )
        };
        if pixels as usize == usize::MAX {
            let error = system_error();
            destroy_handle(self.fd, create.handle);
            return Err(error);
        }
        let mut framebuffer_id = 0;
        if unsafe {
            ffi::drmModeAddFB(
                self.fd,
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
            destroy_handle(self.fd, create.handle);
            return Err(error);
        }
        let mut candidate = CandidateBuffer {
            fd: self.fd,
            buffer: Some(Buffer {
                framebuffer_id,
                handle: create.handle,
                pixels: pixels.cast(),
                size,
                pitch: create.pitch as usize,
                width: usize::from(mode.hdisplay),
                height: usize::from(mode.vdisplay),
                mode,
            }),
        };
        render_full(candidate.buffer.as_mut().unwrap(), grid, atlas, metrics);
        Ok(candidate)
    }

    pub fn commit(&mut self, candidate: &mut CandidateBuffer) -> Result<(), DisplayError> {
        let buffer = candidate.buffer.as_ref().ok_or(DisplayError::System)?;
        let mut connector_id = self.connector_id;
        let mut mode = buffer.mode;
        if unsafe {
            ffi::drmModeSetCrtc(
                self.fd,
                self.crtc_id,
                buffer.framebuffer_id,
                0,
                0,
                &mut connector_id,
                1,
                &mut mode,
            )
        } < 0
        {
            return Err(commit_error());
        }
        let next = candidate.buffer.take().unwrap();
        if let Some(old) = self.active.replace(next) {
            cleanup_buffer(self.fd, old);
        }
        Ok(())
    }

    pub fn mode(&self) -> Option<DrmMode> {
        self.active.as_ref().map(|buffer| buffer.mode)
    }

    pub fn present(
        &mut self,
        model: &mut Model,
        atlas: &Atlas,
        metrics: FontMetrics,
    ) -> Result<(), DisplayError> {
        let buffer = self.active.as_mut().ok_or(DisplayError::System)?;
        let mut clips = [DrmClip::default(); MAX_CLIPS];
        let mut clip_count = 0usize;
        let mut union = None::<(usize, usize, usize, usize)>;
        for row in 0..model.rows() {
            let Some((first, end)) = model.dirty_span(row) else {
                continue;
            };
            render_cells(buffer, model, atlas, metrics, row, first, end);
            let cell_width = metrics.width();
            let cell_height = metrics.height();
            let rectangle = (
                first * cell_width,
                row * cell_height,
                (end * cell_width).min(buffer.width),
                ((row + 1) * cell_height).min(buffer.height),
            );
            union = Some(match union {
                None => rectangle,
                Some((x1, y1, x2, y2)) => (
                    x1.min(rectangle.0),
                    y1.min(rectangle.1),
                    x2.max(rectangle.2),
                    y2.max(rectangle.3),
                ),
            });
            if clip_count < MAX_CLIPS {
                clips[clip_count] = clip(rectangle)?;
                clip_count += 1;
            }
        }
        let Some(union) = union else {
            return Ok(());
        };
        render_cursor(buffer, model, metrics);
        if clip_count == MAX_CLIPS && model.rows() > MAX_CLIPS {
            clips[0] = clip(union)?;
            clip_count = 1;
        }
        if unsafe {
            ffi::drmModeDirtyFB(
                self.fd,
                buffer.framebuffer_id,
                clips.as_mut_ptr(),
                clip_count as u32,
            )
        } < 0
        {
            return Err(system_error());
        }
        for row in 0..model.rows() {
            model.clear_dirty(row);
        }
        Ok(())
    }
}

impl Drop for CandidateBuffer {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            cleanup_buffer(self.fd, buffer);
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        if let Some(buffer) = self.active.take() {
            cleanup_buffer(self.fd, buffer);
        }
    }
}

fn render_full<G: Grid>(buffer: &mut Buffer, grid: &G, atlas: &Atlas, metrics: FontMetrics) {
    for row in 0..buffer.height {
        let pixels = unsafe {
            slice::from_raw_parts_mut(
                (buffer.pixels as *mut u8)
                    .add(row * buffer.pitch)
                    .cast::<u32>(),
                buffer.width,
            )
        };
        pixels.fill(BACKGROUND);
    }
    for row in 0..grid.rows() {
        render_cells(buffer, grid, atlas, metrics, row, 0, grid.columns());
    }
    render_cursor(buffer, grid, metrics);
}

fn render_cells<G: Grid>(
    buffer: &mut Buffer,
    grid: &G,
    atlas: &Atlas,
    metrics: FontMetrics,
    row: usize,
    first: usize,
    end: usize,
) {
    for column in first..end {
        let cell = grid.cell(row, column);
        render_cell(
            buffer,
            atlas,
            metrics,
            row,
            column,
            cell,
            grid.reverse_screen(),
            grid.blink_visible(),
        );
    }
}

fn render_cell(
    buffer: &mut Buffer,
    atlas: &Atlas,
    metrics: FontMetrics,
    row: usize,
    column: usize,
    cell: Cell,
    reverse_screen: bool,
    blink_visible: bool,
) {
    let (mut foreground, mut background) = (cell.foreground, cell.background);
    if (cell.attributes & ATTR_INVERSE != 0) ^ reverse_screen {
        core::mem::swap(&mut foreground, &mut background);
    }
    if cell.attributes & ATTR_HIDDEN != 0 || cell.attributes & ATTR_BLINK != 0 && !blink_visible {
        foreground = background;
    }
    if cell.attributes & ATTR_DIM != 0 {
        foreground = (foreground & 0xfefefe) >> 1;
    }
    let glyph = atlas.glyph(cell.codepoint, cell.attributes & ATTR_BOLD != 0);
    let cell_width = metrics.width();
    let cell_height = metrics.height();
    for y in 0..cell_height {
        let pixel_y = row * cell_height + y;
        if pixel_y >= buffer.height {
            break;
        }
        let pixels = unsafe {
            slice::from_raw_parts_mut(
                (buffer.pixels as *mut u8)
                    .add(pixel_y * buffer.pitch)
                    .cast::<u32>()
                    .add(column * cell_width),
                cell_width.min(buffer.width.saturating_sub(column * cell_width)),
            )
        };
        for (x, pixel) in pixels.iter_mut().enumerate() {
            let alpha = if cell.attributes & ATTR_UNDERLINE != 0 && y + 3 >= cell_height {
                255
            } else {
                glyph[y * cell_width + x]
            };
            *pixel = atlas::blend(background, foreground, alpha);
        }
    }
}

fn render_cursor<G: Grid>(buffer: &mut Buffer, grid: &G, metrics: FontMetrics) {
    let Some((row, column)) = grid.cursor() else {
        return;
    };
    let y = (row + 1) * metrics.height() - 3;
    let x = column * metrics.width();
    for offset_y in 0..3 {
        if y + offset_y >= buffer.height || x >= buffer.width {
            continue;
        }
        let pixels = unsafe {
            slice::from_raw_parts_mut(
                (buffer.pixels as *mut u8)
                    .add((y + offset_y) * buffer.pitch)
                    .cast::<u32>()
                    .add(x),
                metrics.width().min(buffer.width - x),
            )
        };
        pixels.fill(0x00f8fafc);
    }
}

fn clip(rectangle: (usize, usize, usize, usize)) -> Result<DrmClip, DisplayError> {
    Ok(DrmClip {
        x1: u16::try_from(rectangle.0).map_err(|_| DisplayError::System)?,
        y1: u16::try_from(rectangle.1).map_err(|_| DisplayError::System)?,
        x2: u16::try_from(rectangle.2).map_err(|_| DisplayError::System)?,
        y2: u16::try_from(rectangle.3).map_err(|_| DisplayError::System)?,
    })
}

fn cleanup_buffer(fd: i32, buffer: Buffer) {
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

fn commit_error() -> DisplayError {
    match ffi::errno() {
        // SETCRTC 在第二次 GETCONNECTOR 后仍可与 display-info 换代或 adapter 内部
        // command 竞争；候选 framebuffer 保持有效，重试是唯一无损恢复路径。
        ffi::EBUSY | ffi::EINTR => DisplayError::Transient,
        ffi::ENOMEM => DisplayError::OutOfMemory,
        _ => DisplayError::System,
    }
}

fn framebuffer_budget() -> usize {
    let fd = unsafe {
        ffi::open(
            ffi::c_str(b"/proc/meminfo\0"),
            ffi::O_RDONLY | ffi::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return 16 * 1024 * 1024;
    }
    let mut bytes = [0u8; 256];
    let count = unsafe { ffi::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    unsafe { ffi::close(fd) };
    if count <= 0 {
        return 16 * 1024 * 1024;
    }
    let mut value = 0usize;
    let mut seen = false;
    for byte in &bytes[..count as usize] {
        if byte.is_ascii_digit() {
            seen = true;
            value = value
                .saturating_mul(10)
                .saturating_add(usize::from(byte - b'0'));
        } else if seen {
            break;
        }
    }
    value
        .checked_mul(1024)
        .map(|bytes| (bytes / 8).min(32 * 1024 * 1024))
        .filter(|bytes| *bytes != 0)
        .unwrap_or(16 * 1024 * 1024)
}
