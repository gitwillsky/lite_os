use core::{ffi::c_void, ptr, slice};

use crate::{
    atlas::{self, Atlas, FontMetrics},
    ffi::{
        self, DrmClip, DrmConnector, DrmCrtc, DrmDirty, DrmDumbCreate, DrmDumbMap, DrmFramebuffer,
        DrmMode, DrmResources,
    },
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
    pub fn open() -> Result<Self, DisplayError> {
        let fd = unsafe {
            ffi::open(
                ffi::c_str(b"/dev/dri/card0\0"),
                ffi::O_RDWR | ffi::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(system_error());
        }
        let mut crtc_id = 0u32;
        let mut connector_id = 0u32;
        let mut resources = DrmResources {
            crtc_ids: (&mut crtc_id as *mut u32) as u64,
            connector_ids: (&mut connector_id as *mut u32) as u64,
            crtc_count: 1,
            connector_count: 1,
            ..DrmResources::default()
        };
        if unsafe {
            ffi::ioctl(
                fd,
                ffi::DRM_IOCTL_MODE_GETRESOURCES,
                (&mut resources as *mut DrmResources).cast(),
            )
        } < 0
        {
            let error = system_error();
            unsafe { ffi::close(fd) };
            return Err(error);
        }
        if resources.crtc_count == 0 || resources.connector_count == 0 {
            unsafe { ffi::close(fd) };
            return Err(DisplayError::System);
        }
        Ok(Self {
            fd,
            crtc_id,
            connector_id,
            active: None,
            framebuffer_budget: framebuffer_budget(),
        })
    }

    pub fn query_mode(&self) -> Result<DrmMode, DisplayError> {
        let mut mode = DrmMode::default();
        let mut connector = DrmConnector {
            modes: (&mut mode as *mut DrmMode) as u64,
            mode_count: 1,
            connector_id: self.connector_id,
            ..DrmConnector::default()
        };
        if unsafe {
            ffi::ioctl(
                self.fd,
                ffi::DRM_IOCTL_MODE_GETCONNECTOR,
                (&mut connector as *mut DrmConnector).cast(),
            )
        } < 0
        {
            return Err(system_error());
        }
        if connector.mode_count == 0 || mode.hdisplay == 0 || mode.vdisplay == 0 {
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
            ffi::ioctl(
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
            ffi::ioctl(
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
        let mut framebuffer = DrmFramebuffer {
            width: u32::from(mode.hdisplay),
            height: u32::from(mode.vdisplay),
            pitch: create.pitch,
            bpp: 32,
            depth: 24,
            handle: create.handle,
            ..DrmFramebuffer::default()
        };
        if unsafe {
            ffi::ioctl(
                self.fd,
                ffi::DRM_IOCTL_MODE_ADDFB,
                (&mut framebuffer as *mut DrmFramebuffer).cast(),
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
                framebuffer_id: framebuffer.framebuffer_id,
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
        let connector_id = self.connector_id;
        let mut crtc = DrmCrtc {
            connectors: (&connector_id as *const u32) as u64,
            connector_count: 1,
            crtc_id: self.crtc_id,
            framebuffer_id: buffer.framebuffer_id,
            mode_valid: 1,
            mode: buffer.mode,
            ..DrmCrtc::default()
        };
        if unsafe {
            ffi::ioctl(
                self.fd,
                ffi::DRM_IOCTL_MODE_SETCRTC,
                (&mut crtc as *mut DrmCrtc).cast(),
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
        let mut dirty = DrmDirty {
            framebuffer_id: buffer.framebuffer_id,
            flags: 0,
            color: 0,
            clip_count: clip_count as u32,
            clips: clips.as_ptr() as u64,
        };
        if unsafe {
            ffi::ioctl(
                self.fd,
                ffi::DRM_IOCTL_MODE_DIRTYFB,
                (&mut dirty as *mut DrmDirty).cast(),
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
        unsafe { ffi::close(self.fd) };
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
    let renderer = CellRenderer {
        atlas,
        metrics,
        reverse_screen: grid.reverse_screen(),
        blink_visible: grid.blink_visible(),
    };
    for column in first..end {
        renderer.render(buffer, row, column, grid.cell(row, column));
    }
}

/// Immutable facts shared by every cell painted in one grid span.
struct CellRenderer<'a> {
    atlas: &'a Atlas,
    metrics: FontMetrics,
    reverse_screen: bool,
    blink_visible: bool,
}

impl CellRenderer<'_> {
    fn render(&self, buffer: &mut Buffer, row: usize, column: usize, cell: Cell) {
        let (mut foreground, mut background) = (cell.foreground, cell.background);
        if (cell.attributes & ATTR_INVERSE != 0) ^ self.reverse_screen {
            core::mem::swap(&mut foreground, &mut background);
        }
        if cell.attributes & ATTR_HIDDEN != 0
            || cell.attributes & ATTR_BLINK != 0 && !self.blink_visible
        {
            foreground = background;
        }
        if cell.attributes & ATTR_DIM != 0 {
            foreground = (foreground & 0xfefefe) >> 1;
        }
        let glyph = self
            .atlas
            .glyph(cell.codepoint, cell.attributes & ATTR_BOLD != 0);
        let cell_width = self.metrics.width();
        let cell_height = self.metrics.height();
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
    let mut framebuffer_id = buffer.framebuffer_id;
    unsafe {
        ffi::ioctl(
            fd,
            ffi::DRM_IOCTL_MODE_RMFB,
            (&mut framebuffer_id as *mut u32).cast(),
        )
    };
    unsafe { ffi::munmap(buffer.pixels.cast::<c_void>(), buffer.size) };
    destroy_handle(fd, buffer.handle);
}

fn destroy_handle(fd: i32, handle: u32) {
    let mut handle = handle;
    unsafe {
        ffi::ioctl(
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
