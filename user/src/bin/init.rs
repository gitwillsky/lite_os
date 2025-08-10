#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use user_lib::{exec, exit, fork, gfx, read_file, wait, yield_};

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
    let mut shell_pid = None;

    // Start initial shell
    if gui_create() {
        // 阶段1：黑底白字核心初始化提示（用户态模拟，避免内核耦合）
        gui_clear(0xFF000000);
        gui_draw_text_big(40, 60, "Kernel starting...", 0xFFFFFFFF, 2);
        gui_draw_text_big(40, 90, "Initializing drivers...", 0xFFFFFFFF, 2);
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

        // 阶段3：加载字体并用 TTF 渲染 UTF-8 文本
        gui_clear(0xFF000000);
        let mut font_buf: Vec<u8> = alloc::vec![0u8; 8 * 1024 * 1024];
        let read_n = read_file("/fonts/NotoSans-Regular.ttf", &mut font_buf);
        if read_n > 0 {
            font_buf.truncate(read_n as usize);
            let font_bytes: &'static [u8] = Box::leak(font_buf.into_boxed_slice());
            let msg = "你好, LiteOS! 🌟 (TTF/UTF-8 渲染成功)";
            let y = (h / 2 + 10) as i32;
            let _ = gfx::draw_text_ttf(40, y, msg, 32, 0xFFFFFFFF, font_bytes);
            gui_flush();
        } else {
            // 回退：ASCII 位图字体
            gui_draw_text_big(
                (w / 2 - 90) as i32,
                (h / 2) as i32,
                "Launching Shell",
                0xFFFFFFFF,
                2,
            );
            gui_flush();
        }
    }

    spawn_shell(&mut shell_pid);

    // Main process reaping loop
    loop {
        let mut exit_code: i32 = 0;
        let exited_pid = wait(&mut exit_code);

        if exited_pid == -1 {
            yield_();
            continue;
        }

        // // Check if the shell exited
        // if let Some(current_shell_pid) = shell_pid {
        //     if exited_pid as usize == current_shell_pid {
        //         shell_pid = None;
        //         spawn_shell(&mut shell_pid);
        //     }
        // }
    }
}

fn spawn_shell(shell_pid: &mut Option<usize>) {
    let pid = fork();
    if pid == 0 {
        let exit_code = exec("/bin/shell") as i32;
        exit(exit_code);
    } else if pid > 0 {
        *shell_pid = Some(pid as usize);
    } else {
        println!("init: failed to fork shell process");
    }
}
