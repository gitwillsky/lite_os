use crate::drivers::{get_global_framebuffer, with_global_framebuffer, framebuffer::Rect};
use crate::memory::page_table::translated_byte_buffer;
use crate::memory::{KERNEL_SPACE, address::VirtualAddress};
use crate::task::current_user_token;
use crate::drivers::PixelFormat;

pub const SYSCALL_GUI_CREATE_CONTEXT: usize = 300;
pub const SYSCALL_GUI_DESTROY_CONTEXT: usize = 301;
pub const SYSCALL_GUI_CLEAR_SCREEN: usize = 302;
pub const SYSCALL_GUI_PRESENT: usize = 312; // 以 RGBA8888 像素提交整帧
pub const SYSCALL_GUI_FLUSH: usize = 310;
pub const SYSCALL_GUI_GET_SCREEN_INFO: usize = 311;
pub const SYSCALL_GUI_FLUSH_RECTS: usize = 313; // 刷新多个矩形区域
pub const SYSCALL_GUI_PRESENT_RECTS: usize = 314; // 仅提交多个矩形
pub const SYSCALL_GUI_MAP_FRAMEBUFFER: usize = 315; // 将帧缓冲映射到用户态

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GuiScreenInfo {
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
    pub pitch: u32,
    // 新增：像素格式，用户态据此决定 RGBA/BGRA 等通道顺序
    // 取值约定：0=RGBA8888, 1=BGRA8888, 2=RGB888, 3=BGR888, 4=RGB565
    pub format: u32,
}

pub fn sys_gui_create_context() -> isize {
    if get_global_framebuffer().is_some() {
        1 // Return a dummy context ID
    } else {
        error!("[GUI] No framebuffer available");
        -1
    }
}

pub fn sys_gui_destroy_context(_context_id: usize) -> isize {
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

pub fn sys_gui_flush_rects(rects_ptr: *const Rect, rects_len: usize) -> isize {
    if rects_ptr.is_null() || rects_len == 0 { return sys_gui_flush(); }
    let token = current_user_token();
    let bytes = core::mem::size_of::<Rect>() * rects_len;
    let mut bufs = translated_byte_buffer(token, rects_ptr as *const u8, bytes);
    // 将用户 rects 拷到内核临时栈上
    let mut rects: alloc::vec::Vec<Rect> = alloc::vec::Vec::with_capacity(rects_len);
    let mut copied = 0usize;
    while copied < bytes {
        if bufs.is_empty() { break; }
        let seg = bufs.remove(0);
        let to = core::cmp::min(seg.len(), bytes - copied);
        let p = seg.as_ptr();
        let slice = unsafe { core::slice::from_raw_parts(p, to) };
        rects.extend_from_slice(unsafe { core::slice::from_raw_parts(slice.as_ptr() as *const Rect, to / core::mem::size_of::<Rect>()) });
        copied += to;
    }
    if rects.is_empty() { return -1; }
    let ret = match with_global_framebuffer(|fb| fb.flush_rects(&rects)) {
        Some(Ok(_)) => 0,
        _ => -1,
    };
    ret
}

// 以 RGBA8888 提交若干矩形，buf 中紧密按行存放每个矩形的像素，无行间 padding
pub fn sys_gui_present_rects(buf_ptr: *const u8, buf_len: usize, rects_ptr: *const Rect, rects_len: usize) -> isize {
    if buf_ptr.is_null() || rects_ptr.is_null() || rects_len == 0 { return -1; }
    let token = current_user_token();
    // 读取矩形数组
    let rect_bytes = core::mem::size_of::<Rect>() * rects_len;
    let mut rect_bufs = translated_byte_buffer(token, rects_ptr as *const u8, rect_bytes);
    let mut rects: alloc::vec::Vec<Rect> = alloc::vec::Vec::with_capacity(rects_len);
    let mut copied = 0usize;
    while copied < rect_bytes {
        if rect_bufs.is_empty() { break; }
        let seg = rect_bufs.remove(0);
        let to = core::cmp::min(seg.len(), rect_bytes - copied);
        let seg_ptr = seg.as_ptr();
        let slice = unsafe { core::slice::from_raw_parts(seg_ptr, to) };
        let nrect = to / core::mem::size_of::<Rect>();
        if nrect > 0 {
            rects.extend_from_slice(unsafe { core::slice::from_raw_parts(slice.as_ptr() as *const Rect, nrect) });
        }
        copied += to;
    }
    if rects.is_empty() { return -1; }

    // 读取像素缓冲分段迭代器
    let mut pix_bufs = translated_byte_buffer(token, buf_ptr, buf_len);
    let mut read_offset: usize = 0;
    let mut next_chunk = |need: usize, pix_bufs: &mut [&mut [u8]], read_offset: &mut usize| -> Option<&[u8]> {
        if pix_bufs.is_empty() { return None; }
        let mut acc = 0usize;
        for seg in pix_bufs.iter() {
            let seg_off = if *read_offset >= acc { *read_offset - acc } else { return None };
            if seg_off < seg.len() {
                let remain = seg.len() - seg_off;
                let take = core::cmp::min(remain, need);
                let ptr = unsafe { seg.as_ptr().add(seg_off) };
                *read_offset += take;
                return Some(unsafe { core::slice::from_raw_parts(ptr, take) });
            }
            acc += seg.len();
        }
        None
    };

    let ok = with_global_framebuffer(|fb| {
        let info = *fb.info();
        // 仅实现 RGBA8888 快路径；其它格式退化为逐像素写
        let is_fast = info.format == crate::drivers::PixelFormat::RGBA8888;
        for r in rects.iter() {
            let w = r.width as usize; let h = r.height as usize;
            let bytes_per_row = w * 4;
            let mut row = 0usize;
            while row < h {
                // 取一行像素
                let mut remaining = bytes_per_row;
                let mut row_bytes: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(bytes_per_row);
                while remaining > 0 {
                    if let Some(chunk) = next_chunk(remaining, &mut pix_bufs, &mut read_offset) {
                        row_bytes.extend_from_slice(chunk);
                        remaining -= chunk.len();
                    } else {
                        return -1; // 缓冲不足
                    }
                }
                if is_fast {
                    // 直接按 pitch 拷贝
                    let dst_y = r.y as usize + row;
                    if dst_y >= info.height as usize { break; }
                    let dst_x = r.x as usize;
                    let dst_off = info.pixel_offset(dst_x as u32, dst_y as u32).unwrap_or(0);
                    unsafe {
                        core::ptr::copy_nonoverlapping(row_bytes.as_ptr(), fb.buffer_ptr().add(dst_off), bytes_per_row);
                    }
                } else {
                    // 逐像素写入
                    let mut i = 0usize;
                    for dx in 0..w {
                        if i + 4 <= row_bytes.len() {
                            let r8 = row_bytes[i]; let g8 = row_bytes[i+1]; let b8 = row_bytes[i+2]; let a8 = row_bytes[i+3];
                            let color = ((a8 as u32) << 24) | ((r8 as u32) << 16) | ((g8 as u32) << 8) | (b8 as u32);
                            let _ = fb.write_pixel(r.x + dx as u32, r.y + row as u32, color);
                            i += 4;
                        }
                    }
                }
                row += 1;
            }
        }
        fb.mark_dirty();
        0
    }).unwrap_or(-1);
    ok
}

// 将设备帧缓冲映射到当前进程的用户空间，返回用户虚拟地址
// 不考虑兼容性：直接将整个帧缓冲区映射为用户可读写
pub fn sys_gui_map_framebuffer(user_addr_out: *mut usize) -> isize {
    let (fb_va, fb_size) = match with_global_framebuffer(|fb| {
        let info = fb.info().clone();
        let va = VirtualAddress::from(fb.buffer_ptr() as usize);
        (va, info.buffer_size)
    }) {
        Some(x) => x,
        None => return -1,
    };

    // 在当前进程内存空间建立同一物理页的用户映射
    let page_size = crate::memory::PAGE_SIZE;
    let page_count = (fb_size + page_size - 1) / page_size;
    let pt_token = KERNEL_SPACE.wait().lock().token();
    let pt = crate::memory::page_table::PageTable::from_token(pt_token);
    let current = crate::task::current_task().unwrap();
    let mut user_mm = current.mm.memory_set.lock();

    // 选择一段用户地址作为映射基址：从高地址开始探测空洞，避免与现有映射冲突
    // 避开用户堆(USER_HEAP_BASE=0x4000_0000)与常规mmap起点(0x5000_0000)，将帧缓冲放到更高的用户地址
    let mut attempt_base = 0x7000_0000usize;
    let max_base = 0x8000_0000usize;
    let stride = ((page_count * page_size + 0x1_0000) & !0xFFF) // 下一次尝试按映射大小+一点余量对齐
        .max(0x10_0000); // 至少 1MB 步长

    let user_base = 'FIND: loop {
        let mut ok = true;
        // 预探测：检查该区间是否全部可映射；若遇到已映射则跳过到下一段
        for i in 0..page_count {
            let dst_va = VirtualAddress::from(attempt_base + i * page_size);
            if let Some(existing) = user_mm.get_page_table().translate(dst_va.floor()) {
                if existing.is_valid() {
                    ok = false; break;
                }
            }
        }
        if ok {
            // 真正执行映射
            for i in 0..page_count {
                let src_va = VirtualAddress::from(fb_va.as_usize() + i * page_size);
                let vpn = src_va.floor();
                let pte = match pt.translate(vpn) { Some(p) => p, None => return -1 };
                let dst_va = VirtualAddress::from(attempt_base + i * page_size);
                if let Err(e) = user_mm.get_page_table_mut().map(
                    dst_va.into(),
                    crate::memory::address::PhysicalAddress::from(pte.ppn()).into(),
                    crate::memory::page_table::PTEFlags::R | crate::memory::page_table::PTEFlags::W | crate::memory::page_table::PTEFlags::U,
                ) {
                    // 回滚已映射页
                    for j in 0..i {
                        let va_rollback = VirtualAddress::from(attempt_base + j * page_size);
                        let _ = user_mm.get_page_table_mut().unmap(va_rollback.into());
                    }
                    ok = false;
                    break;
                }
            }
            if ok { break VirtualAddress::from(attempt_base); }
        }
        attempt_base = attempt_base.saturating_add(stride);
        if attempt_base >= max_base { return -1; }
    };

    // 重要：在返回用户指针之前释放用户内存集的锁，避免后续用户地址翻译/缺页处理与该锁产生互相等待
    drop(user_mm);

    // 写回用户态输出参数（使用直接引用，避免分段缓冲潜在问题）
    let token = crate::task::current_user_token();
    let user_out_ref: &mut usize = crate::memory::page_table::translated_ref_mut(token, user_addr_out);
    *user_out_ref = user_base.as_usize();
    0
}

pub fn sys_gui_get_screen_info(info_ptr: *mut GuiScreenInfo) -> isize {
    let screen_info = match with_global_framebuffer(|fb| {
        let info = fb.info();
        let fmt_code: u32 = match info.format {
            PixelFormat::RGBA8888 => 0,
            PixelFormat::BGRA8888 => 1,
            PixelFormat::RGB888 => 2,
            PixelFormat::BGR888 => 3,
            PixelFormat::RGB565 => 4,
        };
        GuiScreenInfo {
            width: info.width,
            height: info.height,
            bytes_per_pixel: info.format.bytes_per_pixel(),
            pitch: info.pitch,
            format: fmt_code,
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