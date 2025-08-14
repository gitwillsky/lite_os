#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use user_lib::{poll, PollFd};
use user_lib::litegui::{LiteGuiConnection, ShmBuffer, submit_buffer};

#[unsafe(no_mangle)]
fn main() -> i32 {
    // 连接到 compositor
    let conn = match LiteGuiConnection::connect("/tmp/litewm.sock") {
        Some(c) => c,
        None => { println!("gui_demo: connect failed"); return -1; }
    };

    // 准备一块 160x120 RGBA buffer
    let w = 160u32; let h = 120u32;
    let buf = match ShmBuffer::new(w, h) { Some(b) => b, None => { println!("gui_demo: shm buffer failed"); return -1; } };
    let ptr = buf.ptr_mut();

    // 画棋盘格
    for y in 0..h {
        for x in 0..w {
            let off = (y as usize)*buf.stride + (x as usize)*4;
            let c = if ((x/16 + y/16) & 1) == 0 { 0xFFCC8844u32 } else { 0xFF4488CCu32 };
            unsafe {
                *ptr.add(off+0) = (c >> 16) as u8;
                *ptr.add(off+1) = (c >> 8) as u8;
                *ptr.add(off+2) = (c >> 0) as u8;
                *ptr.add(off+3) = (c >> 24) as u8;
            }
        }
    }

    // 发送一帧
    submit_buffer(&conn, &buf, 40, 80);

    // 简单等待输入
    let mut pfds = [PollFd { fd: conn.fd() as i32, events: user_lib::poll_flags::POLLIN, revents: 0 }];
    let _ = poll(&mut pfds, 1000);
    0
}


