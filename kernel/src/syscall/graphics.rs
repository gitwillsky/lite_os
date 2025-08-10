use crate::drivers::{get_global_framebuffer, with_global_framebuffer};
use crate::memory::page_table::translated_byte_buffer;
use crate::task::current_user_token;
use crate::drivers::PixelFormat;

pub const SYSCALL_GUI_CREATE_CONTEXT: usize = 300;
pub const SYSCALL_GUI_DESTROY_CONTEXT: usize = 301;
pub const SYSCALL_GUI_CLEAR_SCREEN: usize = 302;
pub const SYSCALL_GUI_PRESENT: usize = 312; // 以 RGBA8888 像素提交整帧
pub const SYSCALL_GUI_FLUSH: usize = 310;
pub const SYSCALL_GUI_GET_SCREEN_INFO: usize = 311;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GuiScreenInfo {
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
    pub pitch: u32,
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
    match with_global_framebuffer(|fb| fb.clear(color)) {
        Some(Ok(_)) => 0,
        _ => -1,
    }
}

// 以 RGBA8888（u32: 0xAARRGGBB）整帧提交到帧缓冲
pub fn sys_gui_present(buf_ptr: *const u8, buf_len: usize) -> isize {
    let token = current_user_token();
    let mut user_bufs = translated_byte_buffer(token, buf_ptr, buf_len);

    match with_global_framebuffer(|fb| {
        let info = *fb.info();
        let expected_bytes = (info.width as usize) * (info.height as usize) * 4usize;
        if buf_len < expected_bytes {
            return -1;
        }

        // 快路径：帧缓冲就是 RGBA8888 且逐行无 padding，直接线性拷贝
        if info.format == PixelFormat::RGBA8888 && info.pitch as usize == (info.width as usize) * 4 {
            let mut copied: usize = 0;
            let dst_ptr = fb.buffer_ptr();
            let dst_size = fb.buffer_size();
            if dst_size < expected_bytes { return -1; }

            for seg in user_bufs.iter() {
                if copied >= expected_bytes { break; }
                let to_copy = core::cmp::min(seg.len(), expected_bytes - copied);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        seg.as_ptr(),
                        dst_ptr.add(copied),
                        to_copy,
                    );
                }
                copied += to_copy;
            }
            if copied < expected_bytes { return -1; }
            fb.mark_dirty();
            return 0;
        }

        // 兼容路径：逐像素写入并由帧缓冲实现做颜色转换（较慢）
        let mut read_offset = 0usize;
        let mut next_byte = |user_bufs: &mut [&mut [u8]]| -> Option<u8> {
            let mut acc = 0usize;
            for seg in user_bufs.iter() {
                if read_offset < acc + seg.len() {
                    let idx = read_offset - acc;
                    let v = seg[idx];
                    read_offset += 1;
                    return Some(v);
                }
                acc += seg.len();
            }
            None
        };

        for y in 0..info.height {
            for x in 0..info.width {
                let b0 = next_byte(&mut user_bufs).unwrap_or(0);
                let b1 = next_byte(&mut user_bufs).unwrap_or(0);
                let b2 = next_byte(&mut user_bufs).unwrap_or(0);
                let b3 = next_byte(&mut user_bufs).unwrap_or(0);
                let rgba8888 = ((b3 as u32) << 24)
                    | ((b0 as u32) << 16)
                    | ((b1 as u32) << 8)
                    | (b2 as u32);
                let _ = fb.write_pixel(x, y, rgba8888);
            }
        }

        fb.mark_dirty();
        0
    }) {
        Some(ret) => ret,
        None => -1,
    }
}

pub fn sys_gui_flush() -> isize {
    match with_global_framebuffer(|fb| fb.flush()) {
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