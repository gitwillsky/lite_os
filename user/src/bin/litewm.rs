#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{sleep_ms, shm_create, shm_map, mmap_flags, shm_close};
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

    // 共享内存自检：创建一块 200x100 RGBA8888，画渐变并贴到窗口内
    let test_w: u32 = 200; let test_h: u32 = 100; let stride = (test_w * 4) as usize;
    let size = (stride * test_h as usize) as usize;
    let handle = shm_create(size as usize);
    if handle > 0 {
        let va = shm_map(handle as usize, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
        if va > 0 {
            let ptr = va as *mut u8;
            // 填充渐变
            for y in 0..test_h {
                for x in 0..test_w {
                    let off = (y as usize) * stride + (x as usize) * 4;
                    let r = (x * 255 / test_w) as u8;
                    let gch = (y * 255 / test_h) as u8;
                    let b = 64u8;
                    unsafe {
                        *ptr.add(off + 0) = r;
                        *ptr.add(off + 1) = gch;
                        *ptr.add(off + 2) = b;
                        *ptr.add(off + 3) = 255;
                    }
                }
            }
            // 贴图到窗口内
            gfx::blit_rgba(win_x + 24, win_y + 64, test_w, test_h, va as *const u8, stride);
            // 不立即释放，留待进程退出时清理；此处演示 close 接口
            let _ = shm_close(handle as usize);
        }
    }

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


