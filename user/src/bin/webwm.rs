#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use user_lib::{gfx, open, read, close};
use user_lib::syscall::{open_flags, poll, PollFd};

mod webcore;
use webcore::{DomNode, StyleSheet, LayoutBox};

// 简易文件读取到内存（小文件友好）
fn read_small_file(path: &str) -> Option<Vec<u8>> {
    let fd = open(path, open_flags::O_RDONLY) as i32;
    if fd < 0 { return None; }
    let mut buf = vec![0u8; 64 * 1024];
    let n = read(fd as usize, &mut buf);
    let _ = close(fd as usize);
    if n <= 0 { return None; }
    buf.truncate(n as usize);
    Some(buf)
}

fn read_all(path: &str) -> Option<Vec<u8>> {
    let fd = open(path, open_flags::O_RDONLY) as i32;
    if fd < 0 { return None; }
    let mut out: Vec<u8> = Vec::new();
    loop {
        let mut chunk = vec![0u8; 64 * 1024];
        let n = read(fd as usize, &mut chunk);
        if n > 0 {
            let nn = n as usize;
            chunk.truncate(nn);
            out.extend_from_slice(&chunk);
            if nn < 64 * 1024 { break; }
        } else {
            break;
        }
    }
    let _ = close(fd as usize);
    if out.is_empty() { None } else { Some(out) }
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

    // 加载字体并注册为默认字体
    if let Some(font) = read_all("/fonts/SourceHanSansCN-VF.ttf").or_else(|| read_all("/fonts/NotoSans-Regular.ttf")) {
        let leaked: &'static [u8] = Box::leak(font.into_boxed_slice());
        gfx::set_default_font(leaked);
    }

    // 加载资源
    let html_bytes = read_small_file("/usr/share/desktop/index.html")
        .unwrap_or_else(|| b"<body><div id=topbar style=\"background-color:#2f3136;height:32px;color:#ffffff;\">Lite OS Web Desktop</div><div style=\"margin:12px;padding:12px;background-color:#36393f;color:#dcdfe4;\">Hello Web Desktop</div></body>".to_vec());
    let css_bytes = read_small_file("/usr/share/desktop/style.css").unwrap_or_default();

    // 解析 DOM & CSS（极简子集）
    let dom = webcore::html::parse_document(core::str::from_utf8(&html_bytes).unwrap_or(""));
    let stylesheet = webcore::css::parse_stylesheet(core::str::from_utf8(&css_bytes).unwrap_or(""));

    // 应用样式并布局
    let (sw, sh) = gfx::screen_size();
    let mut styled_root = webcore::style::build_style_tree(&dom, &stylesheet);
    let layout_root = webcore::layout::layout_tree(&mut styled_root, sw as i32, sh as i32);

    // 绘制
    webcore::paint::paint_tree(&layout_root);
    gfx::gui_flush();

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


