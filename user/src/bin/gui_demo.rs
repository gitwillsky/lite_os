#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{uds_connect, shm_create, shm_map, shm_close, mmap_flags, write, poll, PollFd};
use user_lib::{munmap};

#[unsafe(no_mangle)]
fn main() -> i32 {
    // 连接到 compositor
    let cfd = uds_connect("/tmp/litewm.sock");
    if cfd < 0 { println!("gui_demo: connect failed: {}", cfd); return -1; }
    let fd = cfd as usize;

    // 准备一块 160x120 RGBA buffer
    let w = 160u32; let h = 120u32; let stride = (w * 4) as usize;
    let size = (stride * h as usize) as usize;
    let handle = shm_create(size);
    if handle <= 0 { println!("gui_demo: shm_create failed"); return -1; }
    let va = shm_map(handle as usize, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
    if va <= 0 { println!("gui_demo: shm_map failed"); return -1; }
    let ptr = va as *mut u8;

    // 画棋盘格
    for y in 0..h {
        for x in 0..w {
            let off = (y as usize)*stride + (x as usize)*4;
            let c = if ((x/16 + y/16) & 1) == 0 { 0xFFCC8844u32 } else { 0xFF4488CCu32 };
            unsafe {
                *ptr.add(off+0) = (c >> 16) as u8;
                *ptr.add(off+1) = (c >> 8) as u8;
                *ptr.add(off+2) = (c >> 0) as u8;
                *ptr.add(off+3) = (c >> 24) as u8;
            }
        }
    }

    // 发送一帧：len(4)+kind(4)+payload
    // payload: handle:u32,w:u32,h:u32,stride:u32,dx:i32,dy:i32
    let payload_len = 24usize;
    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    hdr[4..8].copy_from_slice(&(1u32).to_le_bytes());
    let _ = write(fd, &hdr);
    let mut payload = [0u8; 24];
    payload[0..4].copy_from_slice(&((handle as u32)).to_le_bytes());
    payload[4..8].copy_from_slice(&(w as u32).to_le_bytes());
    payload[8..12].copy_from_slice(&(h as u32).to_le_bytes());
    payload[12..16].copy_from_slice(&((stride as u32)).to_le_bytes());
    payload[16..20].copy_from_slice(&((40i32).to_le_bytes()));
    payload[20..24].copy_from_slice(&((80i32).to_le_bytes()));
    let _ = write(fd, &payload);

    // 简单等待输入
    let mut pfds = [PollFd { fd: fd as i32, events: user_lib::poll_flags::POLLIN, revents: 0 }];
    let _ = poll(&mut pfds, 1000);

    let _ = munmap(va as usize, size);
    let _ = shm_close(handle as usize);
    0
}


