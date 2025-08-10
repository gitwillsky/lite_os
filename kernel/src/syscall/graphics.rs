use crate::drivers::{get_global_framebuffer, with_global_framebuffer};
use crate::task::current_user_token;
use crate::memory::page_table::translated_byte_buffer;
use crate::graphics::{Color, Point, Rect};
use crate::graphics::primitives::GraphicsRenderer;
use crate::graphics::font::FontRenderer;

pub const SYSCALL_GUI_CREATE_CONTEXT: usize = 300;
pub const SYSCALL_GUI_DESTROY_CONTEXT: usize = 301;
pub const SYSCALL_GUI_CLEAR_SCREEN: usize = 302;
pub const SYSCALL_GUI_DRAW_PIXEL: usize = 303;
pub const SYSCALL_GUI_DRAW_LINE: usize = 304;
pub const SYSCALL_GUI_DRAW_RECT: usize = 305;
pub const SYSCALL_GUI_FILL_RECT: usize = 306;
pub const SYSCALL_GUI_DRAW_CIRCLE: usize = 307;
pub const SYSCALL_GUI_FILL_CIRCLE: usize = 308;
pub const SYSCALL_GUI_DRAW_TEXT: usize = 309;
pub const SYSCALL_GUI_FLUSH: usize = 310;
pub const SYSCALL_GUI_GET_SCREEN_INFO: usize = 311;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GuiPoint {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GuiRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GuiColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GuiScreenInfo {
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
    pub pitch: u32,
}

impl From<GuiPoint> for Point {
    fn from(gp: GuiPoint) -> Self {
        Point::new(gp.x, gp.y)
    }
}

impl From<GuiRect> for Rect {
    fn from(gr: GuiRect) -> Self {
        Rect::new(gr.x, gr.y, gr.width, gr.height)
    }
}

impl From<GuiColor> for Color {
    fn from(gc: GuiColor) -> Self {
        Color::new_rgba(gc.r, gc.g, gc.b, gc.a)
    }
}

pub fn sys_gui_create_context() -> isize {
    info!("[GUI] Creating graphics context");

    if get_global_framebuffer().is_some() {
        1 // Return a dummy context ID
    } else {
        error!("[GUI] No framebuffer available");
        -1
    }
}

pub fn sys_gui_destroy_context(_context_id: usize) -> isize {
    info!("[GUI] Destroying graphics context");
    0
}

pub fn sys_gui_clear_screen(color: u32) -> isize {
    match with_global_framebuffer(|fb| {
        fb.clear(color)
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_draw_pixel(point: GuiPoint, color: GuiColor) -> isize {
    let point = Point::from(point);
    let color = Color::from(color);

    match with_global_framebuffer(|fb| {
        GraphicsRenderer::draw_pixel(fb, point, color)
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_draw_line(start: GuiPoint, end: GuiPoint, color: GuiColor) -> isize {
    let start = Point::from(start);
    let end = Point::from(end);
    let color = Color::from(color);

    match with_global_framebuffer(|fb| {
        GraphicsRenderer::draw_line(fb, start, end, color)
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_draw_rect(rect: GuiRect, color: GuiColor) -> isize {
    let rect = Rect::from(rect);
    let color = Color::from(color);

    match with_global_framebuffer(|fb| {
        GraphicsRenderer::draw_rect(fb, rect, color)
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_fill_rect(rect: GuiRect, color: GuiColor) -> isize {
    let rect = Rect::from(rect);
    let color = Color::from(color);

    match with_global_framebuffer(|fb| {
        GraphicsRenderer::fill_rect(fb, rect, color)
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_draw_circle(center: GuiPoint, radius: u32, color: GuiColor) -> isize {
    let center = Point::from(center);
    let color = Color::from(color);
    let circle = crate::graphics::geometry::Circle::new(center, radius);

    match with_global_framebuffer(|fb| {
        GraphicsRenderer::draw_circle(fb, circle, color)
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_fill_circle(center: GuiPoint, radius: u32, color: GuiColor) -> isize {
    let center = Point::from(center);
    let color = Color::from(color);
    let circle = crate::graphics::geometry::Circle::new(center, radius);

    match with_global_framebuffer(|fb| {
        GraphicsRenderer::fill_circle(fb, circle, color)
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_draw_text(text_ptr: *const u8, text_len: usize, pos: GuiPoint, color: GuiColor) -> isize {
    // 从用户空间安全读取字符串
    let token = current_user_token();
    let mut vec_buf: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(text_len);
    let buffers = translated_byte_buffer(token, text_ptr, text_len);
    for seg in buffers.iter() {
        vec_buf.extend_from_slice(seg);
    }
    let text = match core::str::from_utf8(&vec_buf) {
        Ok(s) => s,
        Err(_) => return -1,
    };

    let pos = Point::from(pos);
    let color = Color::from(color);

    match with_global_framebuffer(|fb| {
        fb.draw_string(text, pos, color)
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_flush() -> isize {
    match with_global_framebuffer(|fb| {
        fb.flush()
    }) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

pub fn sys_gui_get_screen_info(info_ptr: *mut GuiScreenInfo) -> isize {
    let screen_info = match with_global_framebuffer(|fb| {
        let info = fb.info();
        GuiScreenInfo {
            width: info.width,
            height: info.height,
            bytes_per_pixel: info.format.bytes_per_pixel(),
            pitch: info.pitch,
        }
    }) {
        Some(info) => info,
        None => return -1,
    };

    // 将结果安全写回用户空间
    let token = current_user_token();
    let size = core::mem::size_of::<GuiScreenInfo>();
    let mut buffers = translated_byte_buffer(token, info_ptr as *const u8, size);
    let src_bytes = unsafe {
        core::slice::from_raw_parts((&screen_info as *const GuiScreenInfo) as *const u8, size)
    };
    let mut copied = 0usize;
    for seg in buffers.iter_mut() {
        let remain = size - copied;
        let to_copy = core::cmp::min(remain, seg.len());
        seg[..to_copy].copy_from_slice(&src_bytes[copied..copied + to_copy]);
        copied += to_copy;
        if copied >= size { break; }
    }

    if copied == size { 0 } else { -1 }
}