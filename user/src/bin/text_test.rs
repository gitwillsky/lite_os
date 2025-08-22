#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::boxed::Box;
use user_lib::gfx;
use user_lib::webcore::{RenderEngine, StandardRenderEngine};

#[unsafe(no_mangle)]
fn main() -> i32 {
    if !gfx::gui_create_context() {
        println!("text_test: 获取GUI上下文失败");
        return -1;
    }

    // 清屏并绘制背景色
    gfx::gui_clear(0xFF1e3a5f); // 使用和桌面相同的背景色
    gfx::gui_flush();

    // 加载默认字体
    load_font("/fonts/SourceHanSansCN-VF.ttf");

    // 创建渲染引擎
    let mut engine = StandardRenderEngine::new();

    // 设置视口大小
    let (sw, sh) = gfx::screen_size();
    println!("[text_test] Screen size: {}x{}", sw, sh);
    engine.set_viewport(sw, sh);

    // 加载简单的白色文本测试
    let test_html = r#"
        <html>
            <head>
                <style>
                    body { 
                        color: white; 
                        font-size: 20px; 
                        background-color: #1e3a5f;
                        padding: 20px;
                    }
                    h1 { 
                        color: #ffff00; 
                        font-size: 32px; 
                        margin-bottom: 20px;
                    }
                    .test-text {
                        color: #00ff00;
                        font-size: 24px;
                        margin: 10px 0;
                    }
                </style>
            </head>
            <body>
                <h1>Text Rendering Test</h1>
                <p>This is white text on dark background.</p>
                <div class="test-text">This is green text.</div>
                <span>Span text content</span>
            </body>
        </html>
    "#;

    println!("[text_test] Loading test HTML...");
    engine.load_html(test_html);

    // 执行渲染
    println!("[text_test] Rendering...");
    let render_result = engine.render();

    // 显示生成的绘制命令统计
    println!(
        "[text_test] Generated {} draw commands",
        render_result.commands.len()
    );
    let mut text_commands = 0;
    let mut rect_commands = 0;

    for cmd in &render_result.commands {
        match cmd {
            user_lib::webcore::paint::DrawCommand::DrawText { .. } => text_commands += 1,
            user_lib::webcore::paint::DrawCommand::FillRect { .. } => rect_commands += 1,
            _ => {}
        }
    }

    println!(
        "[text_test] Text commands: {}, Rect commands: {}",
        text_commands, rect_commands
    );

    // 执行绘制命令
    execute_draw_commands(&render_result);
    gfx::gui_flush();
    println!("[text_test] Test rendered - should see colored text");

    // 简单等待，然后退出
    let sleep_time = user_lib::syscall::TimeSpec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    user_lib::syscall::nanosleep(&sleep_time, core::ptr::null_mut()); // 等待5秒

    0
}

fn load_font(path: &str) {
    use user_lib::webcore::loader;
    println!("[text_test] Loading font: {}", path);
    if let Some(font_data) = loader::read_all(path) {
        let leaked: &'static [u8] = Box::leak(font_data.into_boxed_slice());
        gfx::set_default_font(leaked);
        println!("[text_test] Font loaded successfully: {}", path);
    } else {
        println!("[text_test] Failed to load font: {}", path);
    }
}

fn execute_draw_commands(result: &user_lib::webcore::RenderResult) {
    use user_lib::webcore::paint::DrawCommand;

    for (i, cmd) in result.commands.iter().enumerate() {
        match cmd {
            DrawCommand::FillRect {
                x,
                y,
                width,
                height,
                color,
            } => {
                gfx::gui_fill_rect_xywh(*x, *y, *width, *height, *color);
                println!(
                    "[text_test] FillRect #{}: ({}, {}) {}x{} color={:#x}",
                    i, x, y, width, height, color
                );
            }
            DrawCommand::DrawText {
                x,
                y,
                text,
                color,
                size,
            } => {
                println!(
                    "[text_test] DrawText #{}: ({}, {}) '{}' size={} color={:#x}",
                    i, x, y, text, size, color
                );
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
                ..
            } => {
                // 暂时绘制占位符
                gfx::gui_fill_rect_xywh(*x, *y, *width, *height, 0xFF808080);
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
