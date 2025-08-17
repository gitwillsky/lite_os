#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::boxed::Box;
// use alloc::string::String;
use user_lib::gfx;
use user_lib::syscall::{open_flags, poll, PollFd};
use user_lib::read;

mod webcore;
use webcore::{loader, document};

// 读取逻辑已抽到 webcore::loader

#[unsafe(no_mangle)]
fn main() -> i32 {
    if !gfx::gui_create_context() {
        println!("webwm: 获取GUI上下文失败");
        return -1;
    }

    // 清屏并绘制背景色
    gfx::gui_clear(0xFF202225);
    gfx::gui_flush();

    // 加载字体并注册为默认字体
    if let Some(font) = loader::read_all("/fonts/SourceHanSansCN-VF.ttf").or_else(|| loader::read_all("/fonts/NotoSans-Regular.ttf")) {
        let leaked: &'static [u8] = Box::leak(font.into_boxed_slice());
        gfx::set_default_font(leaked);
    }

    // 准备页面（HTML + CSS 外链加载与合并）
    println!("[webwm] About to call load_and_prepare...");
    let page = document::load_and_prepare(
        "/usr/share/desktop/index.html",
        b"<body><div style='background:#00309C;position:absolute;left:0;top:0;right:0;bottom:0'></div></body>"
    );
    println!("[webwm] load_and_prepare completed");

    // 应用样式并布局
    println!("[webwm] About to layout...");
    let (sw, sh) = gfx::screen_size();
    let layout_root = page.layout(sw as i32, sh as i32);
    println!("[webwm] About to paint...");
    webcore::paint::paint_layout_box(&layout_root);
    println!("[webwm] About to flush...");
    gfx::gui_flush();
    println!("[webwm] GUI flushed");

    // 简单事件循环：保持运行并消费输入事件
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
            }
        }
    }
}


