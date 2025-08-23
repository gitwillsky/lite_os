#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use user_lib::gfx;
use user_lib::read;
use user_lib::syscall::{PollFd, open_flags, poll, stat};
use user_lib::webcore::{RenderEngine, StandardRenderEngine};

#[repr(C)]
#[derive(Clone, Copy)]
struct FileStat {
    size: u64,
    file_type: u32,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    atime: u64,
    mtime: u64,
    ctime: u64,
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    if !gfx::gui_create_context() {
        println!("webwm: 获取GUI上下文失败");
        return -1;
    }

    // 清屏并绘制背景色
    gfx::gui_clear(0xFF202225);
    gfx::gui_flush();

    // 创建渲染引擎
    let mut engine = StandardRenderEngine::new();

    // 设置视口大小
    let (sw, sh) = gfx::screen_size();
    println!("[webwm] Screen size: {}x{}", sw, sh);
    engine.set_viewport(sw, sh);

    // 从文件系统加载桌面HTML
    // 渲染引擎会自动处理HTML中的CSS链接和字体引用
    println!("[webwm] Loading desktop HTML from filesystem...");
    let desktop_path = "/usr/share/desktop/desktop.html";
    if !engine.load_html_from_file(desktop_path) {
        println!("[webwm] Failed to load desktop HTML, using fallback");
        engine.load_html(
            r#"<html><body><h1>WebWM</h1><p>Failed to load desktop.html</p></body></html>"#,
        );
    }

    // 处理字体加载
    let font_paths = engine.get_font_paths();
    if !font_paths.is_empty() {
        println!("[webwm] Loading {} fonts...", font_paths.len());
        for font_path in font_paths {
            load_font(&font_path);
        }
    } else {
        // 加载默认字体
        load_font("/fonts/SourceHanSansCN-VF.ttf");
        load_font("/fonts/NotoSans-Regular.ttf");
    }

    // 执行渲染
    println!("[webwm] Rendering desktop...");
    let render_result = engine.render();

    // 执行绘制命令
    execute_draw_commands(&render_result);
    gfx::gui_flush();
    println!("[webwm] Desktop rendered");

    // 事件循环
    run_event_loop(&mut engine);

    0
}

fn load_font(path: &str) {
    use user_lib::webcore::loader;
    println!("[webwm] Loading font: {}", path);
    if let Some(font_data) = loader::read_all(path) {
        let leaked: &'static [u8] = Box::leak(font_data.into_boxed_slice());
        gfx::set_default_font(leaked);
        println!("[webwm] Font loaded: {}", path);
    } else {
        println!("[webwm] Failed to load font: {}", path);
    }
}

fn execute_draw_commands(result: &user_lib::webcore::RenderResult) {
    use user_lib::webcore::paint::DrawCommand;

    for cmd in &result.commands {
        match cmd {
            DrawCommand::FillRect {
                x,
                y,
                width,
                height,
                color,
            } => {
                gfx::gui_fill_rect_xywh(*x, *y, *width, *height, *color);
            }
            DrawCommand::DrawText {
                x,
                y,
                text,
                color,
                size,
            } => {
                if !gfx::draw_text(*x, *y, text, *size, *color) {
                    let scale = if *size >= 16 { *size / 8 } else { 1 };
                    let asc = gfx::font_ascent(*size);
                    let top_y = *y - (asc * scale as i32);
                    gfx::draw_string_scaled(*x, top_y, text, *color, scale);
                }
            }
            DrawCommand::DrawImage {
                x,
                y,
                width,
                height,
                target_width,
                target_height,
                image_data,
            } => {
                if *target_width == *width && *target_height == *height {
                    gfx::blit_rgba(
                        *x,
                        *y,
                        *width,
                        *height,
                        image_data.as_ptr(),
                        (*width as usize) * 4,
                    );
                } else {
                    let scaled = scale_rgba_nearest(
                        image_data,
                        *width,
                        *height,
                        *target_width,
                        *target_height,
                    );
                    gfx::blit_rgba(
                        *x,
                        *y,
                        *target_width,
                        *target_height,
                        scaled.as_ptr(),
                        (*target_width as usize) * 4,
                    );
                }
            }
            DrawCommand::DrawLine {
                x1,
                y1,
                x2,
                y2,
                color,
                width,
            } => {
                // TODO: 实现线条绘制
                let _ = (x1, y1, x2, y2, color, width);
            }
        }
    }
}

fn get_file_mtime(path: &str) -> u64 {
    let mut stat_buf = [0u8; 128];
    if stat(path, &mut stat_buf) == 0 {
        let file_stat = unsafe { *(stat_buf.as_ptr() as *const FileStat) };
        file_stat.mtime
    } else {
        0
    }
}

fn scale_rgba_nearest(src: &Vec<u8>, sw: u32, sh: u32, tw: u32, th: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((tw as usize) * (th as usize) * 4);
    out.resize((tw as usize) * (th as usize) * 4, 0);
    if sw == 0 || sh == 0 || tw == 0 || th == 0 {
        return out;
    }
    for ty in 0..th {
        let sy = (ty as u64 * sh as u64 / th as u64) as u32;
        let src_row = (sy as usize) * (sw as usize) * 4;
        let dst_row = (ty as usize) * (tw as usize) * 4;
        for tx in 0..tw {
            let sx = (tx as u64 * sw as u64 / tw as u64) as u32;
            let si = src_row + (sx as usize) * 4;
            let di = dst_row + (tx as usize) * 4;
            out[di + 0] = src[si + 0];
            out[di + 1] = src[si + 1];
            out[di + 2] = src[si + 2];
            out[di + 3] = src[si + 3];
        }
    }
    out
}

fn run_event_loop(engine: &mut StandardRenderEngine) {
    let input0 = user_lib::open("/dev/input/event0", open_flags::O_RDONLY) as i32;
    let input1 = user_lib::open("/dev/input/event1", open_flags::O_RDONLY) as i32;
    let mut pfds: [PollFd; 2] = [
        PollFd {
            fd: input0,
            events: user_lib::poll_flags::POLLIN,
            revents: 0,
        },
        PollFd {
            fd: input1,
            events: user_lib::poll_flags::POLLIN,
            revents: 0,
        },
    ];
    let mut tmp = [0u8; 128];

    let desktop_path = "/usr/share/desktop/desktop.html";
    let mut last_mtime = get_file_mtime(desktop_path);

    loop {
        let _ = poll(&mut pfds, 1000);
        for i in 0..2 {
            if pfds[i].fd >= 0 && (pfds[i].revents & user_lib::poll_flags::POLLIN) != 0 {
                let _ = read(pfds[i].fd as usize, &mut tmp);
            }
        }

        let current_mtime = get_file_mtime(desktop_path);
        if current_mtime != last_mtime {
            println!("[webwm] Detected change in desktop.html, reloading...");
            if engine.load_html_from_file(desktop_path) {
                let font_paths = engine.get_font_paths();
                if !font_paths.is_empty() {
                    for font_path in font_paths {
                        load_font(&font_path);
                    }
                } else {
                    load_font("/fonts/SourceHanSansCN-VF.ttf");
                    load_font("/fonts/NotoSans-Regular.ttf");
                }
                let render_result = engine.render();
                execute_draw_commands(&render_result);
                gfx::gui_flush();
            }
            last_mtime = current_mtime;
        }

        let update_result = engine.update(16);
        if update_result.redraw_needed {
            let render_result = engine.render();
            execute_draw_commands(&render_result);
            gfx::gui_flush();
        }
    }
}
