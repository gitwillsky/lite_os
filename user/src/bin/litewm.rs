#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::sleep_ms;
use user_lib::gfx;

#[unsafe(no_mangle)]
fn main() -> i32 {
    // 获取 GUI 上下文独占
    if !gfx::gui_create_context() {
        println!("litewm: 获取GUI上下文失败（可能已有其它进程占用）");
        return -1;
    }

    // 读取屏幕信息并清屏
    let (w, h) = gfx::screen_size();
    // 背景：深灰
    gfx::gui_clear(0xFF202020);

    // 绘制一个简单的标题与软件光标，用以验证刷新
    gfx::set_default_font(include_bytes!("../../../NotoSans-Regular.ttf"));
    let title = "LiteWM Compositor";
    gfx::draw_text(20, 40, title, 24, 0xFFFFFFFF);

    // 画一个矩形窗口占位
    let win_w = (w as i32 * 3) / 5;
    let win_h = (h as i32 * 3) / 5;
    let win_x = (w as i32 - win_w) / 2;
    let win_y = (h as i32 - win_h) / 2;
    gfx::gui_fill_rect_xywh(win_x, win_y, win_w as u32, win_h as u32, 0xFF2E5FE3);

    // 标题
    gfx::draw_text(win_x + 16, win_y + 28, "Hello, Desktop!", 20, 0xFFFFFFFF);

    // 刷新一次
    gfx::gui_flush();

    // 简单心跳动画：在右下角闪烁一个小方块
    let mut on = true;
    for _ in 0..120 { // ~2秒
        let size = 10;
        let x = w as i32 - size - 8;
        let y = h as i32 - size - 8;
        let color = if on { 0xFF00FF00 } else { 0xFF004400 };
    gfx::gui_fill_rect_xywh(x, y, size as u32, size as u32, color);
        gfx::gui_flush();
        on = !on;
        sleep_ms(16);
    }

    0
}


