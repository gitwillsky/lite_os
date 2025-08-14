#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use user_lib::gfx;
use user_lib::open_flags;
use user_lib::poll_flags;
use user_lib::{PollFd, mkdir, mmap_flags, munmap, open, poll, read, shm_close, shm_map};
use user_lib::{uds_accept, uds_listen};

#[unsafe(no_mangle)]
fn main() -> i32 {
    // 获取 GUI 上下文独占
    if !gfx::gui_create_context() {
        println!("litewm: 获取GUI上下文失败（可能已有其它进程占用）");
        return -1;
    }

    // 仅清屏为黑色，并刷新一次
    gfx::gui_clear(0xFF000000);
    gfx::gui_flush();

    // 确保 /tmp 存在，并在 UDS 上监听
    let _ = mkdir("/tmp");
    let listen_fd = uds_listen("/tmp/litewm.sock", 16) as i32;
    if listen_fd < 0 {
        println!("litewm: uds_listen failed: {}", listen_fd);
        return -1;
    }

    // 事件循环：UDS 监听 + 输入事件
    // 同时尝试 event0/event1（键盘/鼠标可能顺序不同）
    let input0 = open("/dev/input/event0", open_flags::O_RDONLY) as i32;
    let input1 = open("/dev/input/event1", open_flags::O_RDONLY) as i32;
    // 动态 fd 集合：index 0=listen，其后是客户端，再后是输入
    let mut client_fds: alloc::vec::Vec<i32> = alloc::vec::Vec::new();
    loop {
        // 本轮 poll 使用的客户端快照数量，避免 accept 后索引错位
        let clients_in_pfds = client_fds.len();
        // 构造 pfds 数组（只包含当前快照中的客户端）
        let mut pfds_vec: alloc::vec::Vec<PollFd> = alloc::vec::Vec::new();
        pfds_vec.push(PollFd {
            fd: listen_fd,
            events: poll_flags::POLLIN,
            revents: 0,
        });
        for i in 0..clients_in_pfds {
            let fd = client_fds[i];
            pfds_vec.push(PollFd {
                fd,
                events: poll_flags::POLLIN,
                revents: 0,
            });
        }
        pfds_vec.push(PollFd {
            fd: input0,
            events: poll_flags::POLLIN,
            revents: 0,
        });
        pfds_vec.push(PollFd {
            fd: input1,
            events: poll_flags::POLLIN,
            revents: 0,
        });

        let nready = poll(&mut pfds_vec[..], -1);
        if nready <= 0 {
            continue;
        }
        // 处理监听：accept 新客户端
        if (pfds_vec[0].revents & poll_flags::POLLIN) != 0 {
            let cfd = uds_accept("/tmp/litewm.sock") as i32;
            if cfd >= 0 {
                client_fds.push(cfd);
            }
        }
        // 处理客户端消息（仅处理本轮 pfds 中已有的客户端）
        let client_count = clients_in_pfds;
        for idx in 0..client_count {
            // pfds_vec 中：0=listen，1..=client_count 为客户端
            if (pfds_vec[1 + idx].revents & poll_flags::POLLIN) != 0 {
                let cfd = client_fds[idx] as usize;
                // 帧头：len:u32, kind:u32
                let mut hdr = [0u8; 8];
                let mut got = 0usize;
                while got < 8 {
                    let n = read(cfd, &mut hdr[got..]);
                    if n <= 0 {
                        break;
                    }
                    got += n as usize;
                }
                if got < 8 {
                    continue;
                }
                let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
                let kind = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
                let mut payload = alloc::vec![0u8; len];
                let mut off = 0usize;
                while off < len {
                    let n = read(cfd, &mut payload[off..]);
                    if n <= 0 {
                        break;
                    }
                    off += n as usize;
                }
                if off < len {
                    continue;
                }
                if kind == 1 && len >= 24 {
                    let leu32 = |i: usize| -> u32 {
                        u32::from_le_bytes([
                            payload[i],
                            payload[i + 1],
                            payload[i + 2],
                            payload[i + 3],
                        ])
                    };
                    let lei32 = |i: usize| -> i32 {
                        i32::from_le_bytes([
                            payload[i],
                            payload[i + 1],
                            payload[i + 2],
                            payload[i + 3],
                        ])
                    };
                    let handle = leu32(0) as usize;
                    let bw = leu32(4);
                    let bh = leu32(8);
                    let stride = leu32(12) as usize;
                    let dx = lei32(16);
                    let dy = lei32(20);
                    let va = shm_map(handle, mmap_flags::PROT_READ);
                    if va > 0 {
                        gfx::blit_rgba(dx, dy, bw, bh, va as *const u8, stride);
                        gfx::gui_flush();
                        let _ = munmap(va as usize, (stride * bh as usize) as usize);
                        let _ = shm_close(handle);
                    }
                }
            }
        }
        // 处理输入事件：每个事件 8 字节（移除调试打印，仅消费数据）
        for k in 0..2 {
            let base = 1 + client_count;
            if (pfds_vec[base + k].revents & poll_flags::POLLIN) != 0 && pfds_vec[base + k].fd >= 0
            {
                let mut buf = [0u8; 64];
                let _ = read(pfds_vec[base + k].fd as usize, &mut buf);
            }
        }
    }
}
