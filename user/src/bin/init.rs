#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use user_lib::{exec, exit, fork, gfx, wait, yield_, open, read, close};
use user_lib::open_flags;

// 简单的 GUI 引导：在用户态驱动所有可视化
#[inline(always)]
fn gui_create() -> bool {
    gfx::gui_create_context()
}
#[inline(always)]
fn gui_clear(color: u32) {
    gfx::gui_clear(color)
}
#[inline(always)]
fn gui_fill_rect(x: i32, y: i32, w: u32, h: u32, color: u32) {
    gfx::gui_fill_rect_xywh(x, y, w, h, color)
}
#[inline(always)]
fn gui_draw_text_big(x: i32, y: i32, text: &str, color: u32, scale: u32) {
    gfx::draw_string_scaled(x, y, text, color, scale)
}
#[inline(always)]
fn gui_flush() {
    gfx::gui_flush()
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    // Start initial shell
    if gui_create() {
        gui_clear(0xFF000000);
        let candidates = [
            "/fonts/NotoSans-Regular.ttf",
            "/fonts/SourceHanSansCN-VF.ttf",
        ];
        let mut loaded = false;
        for &path in &candidates {
            if let Some(bytes) = load_font_static(path) {
                gfx::set_default_font(bytes);
                loaded = true;
                break;
            }
        }

        // 阶段1：核心初始化提示
        if loaded {
            let _ = gfx::draw_text(40, 60, "Kernel starting...", 24, 0xFFFFFFFF);
            let _ = gfx::draw_text(40, 90, "Initializing drivers...", 24, 0xFFFFFFFF);
        } else {
            gui_draw_text_big(40, 60, "Kernel starting...", 0xFFFFFFFF, 2);
            gui_draw_text_big(40, 90, "Initializing drivers...", 0xFFFFFFFF, 2);
        }
        gui_flush();

        // 阶段2：加载界面（简化的全屏进度条）
        let (mut w, mut h) = gfx::screen_size();
        if w == 0 || h == 0 {
            w = 1280;
            h = 800;
        }
        // for p in 0..=100 {
        //     // 背景条纹蓝色
        //     for i in 0..10 {
        //         let y = (h / 10 * i) as i32;
        //         let c = 0xFF003C64u32 + ((9 - i) as u32) * 0x00010102;
        //         gui_fill_rect(0, y, w, h / 10, c);
        //     }
        //     let bw = w * 3 / 5;
        //     let bh = 22u32;
        //     let bx = (w - bw) / 2;
        //     let by = h * 2 / 3;
        //     gui_fill_rect(bx as i32, by as i32, bw as u32, bh as u32, 0xFF3A3A3A);
        //     let filled = bw * p / 100;
        //     if filled > 0 {
        //         gui_fill_rect(bx as i32 + 1, by as i32 + 1, filled - 1, bh - 2, 0xFF0A56B5);
        //     }
        //     gui_draw_text_big(
        //         (w / 2 - 40) as i32,
        //         (by + bh + 28) as i32,
        //         "Loading...",
        //         0xFFFFFFFF,
        //         2,
        //     );
        //     gui_flush();
        // }

        // 阶段3：展示中文/UTF-8 文本（使用默认字体）
        gui_clear(0xFF000000);
        let msg = "你好, LiteOS! 🌟 (TTF/UTF-8 渲染成功)";
        let y = (h / 2 + 10) as i32;
        if !gfx::draw_text(40, y, msg, 32, 0xFFFFFFFF) {
            // 回退：ASCII 位图字体
            gui_draw_text_big((w / 2 - 90) as i32, (h / 2) as i32, "Launching Shell", 0xFFFFFFFF, 2);
        }
        gui_flush();
    }

    spawn_shell();

    // Main process reaping loop
    loop {
        let mut exit_code: i32 = 0;
        let exited_pid = wait(&mut exit_code);

        if exited_pid == -1 {
            yield_();
            continue;
        }
    }
}

fn spawn_shell() {
    let pid = fork();
    if pid == 0 {
        let exit_code = exec("/bin/shell") as i32;
        exit(exit_code);
    } else if pid > 0 {
        // shell started
    } else {
        println!("init: failed to fork shell process");
    }
}

// 以小块读取字体；系统已支持按需扩栈，栈/堆方案均可，这里使用堆缓冲以保持占用可控
fn load_font_static(path: &str) -> Option<&'static [u8]> {
    let fd = open(path, open_flags::O_RDONLY) as i32;
    if fd < 0 { return None; }
    let mut data: Vec<u8> = Vec::new();
    let mut scratch = alloc::vec![0u8; 16 * 1024];
    loop {
        let n = read(fd as usize, &mut scratch);
        if n <= 0 { break; }
        let n_usize = n as usize;
        data.extend_from_slice(&scratch[..n_usize]);
        if n_usize < scratch.len() { break; }
    }
    let _ = close(fd as usize);
    if data.is_empty() { None } else { Some(Box::leak(data.into_boxed_slice())) }
}
