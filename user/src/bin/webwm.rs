#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use user_lib::gfx;
use user_lib::syscall::{open_flags, poll, PollFd};
use user_lib::read;
use user_lib::webcore::{RenderEngine, StandardRenderEngine};
use alloc::boxed::Box;

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
    if !engine.load_html_from_file("/usr/share/desktop/desktop.html") {
        println!("[webwm] Failed to load desktop HTML, using fallback");
        engine.load_html(r#"<html><body><h1>WebWM</h1><p>Failed to load desktop.html</p></body></html>"#);
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
            DrawCommand::FillRect { x, y, width, height, color } => {
                gfx::gui_fill_rect_xywh(*x, *y, *width, *height, *color);
            }
            DrawCommand::DrawText { x, y, text, color, size } => {
                if !gfx::draw_text(*x, *y, text, *size, *color) {
                    let scale = if *size >= 16 { *size / 8 } else { 1 };
                    let asc = gfx::font_ascent(*size);
                    let top_y = *y - (asc * scale as i32);
                    gfx::draw_string_scaled(*x, top_y, text, *color, scale);
                }
            }
            DrawCommand::DrawImage { x, y, width, height, .. } => {
                // 暂时绘制占位符
                gfx::gui_fill_rect_xywh(*x, *y, *width, *height, 0xFF808080);
            }
            DrawCommand::DrawLine { x1, y1, x2, y2, color, width } => {
                // TODO: 实现线条绘制
                let _ = (x1, y1, x2, y2, color, width);
            }
        }
    }
}

fn run_event_loop(engine: &mut StandardRenderEngine) {
    let input0 = user_lib::open("/dev/input/event0", open_flags::O_RDONLY) as i32;
    let input1 = user_lib::open("/dev/input/event1", open_flags::O_RDONLY) as i32;
    let mut pfds: [PollFd; 2] = [
        PollFd { fd: input0, events: user_lib::poll_flags::POLLIN, revents: 0 },
        PollFd { fd: input1, events: user_lib::poll_flags::POLLIN, revents: 0 },
    ];
    let mut tmp = [0u8; 128];

    loop {
        let _ = poll(&mut pfds, 1000);
        for i in 0..2 {
            if pfds[i].fd >= 0 && (pfds[i].revents & user_lib::poll_flags::POLLIN) != 0 {
                let _ = read(pfds[i].fd as usize, &mut tmp);
                // TODO: 解析输入事件并传递给引擎
            }
        }

        // 更新引擎状态（动画等）
        let update_result = engine.update(16); // 假设 60 FPS
        if update_result.redraw_needed {
            let render_result = engine.render();
            execute_draw_commands(&render_result);
            gfx::gui_flush();
        }
    }
}