#![no_std]
#![no_main]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

//! netd：网络服务。
//!
//! 拥有 IPv4 协议栈（smoltcp）、接口与路由、TCP/UDP/ICMP/RAW endpoint 与 packet socket。linuxd 经控制
//! channel 转交设备目录中的网卡并为每个网络域创建域 channel，经域 channel 创建 socket；每个 socket
//! 一条 channel，全部操作不阻塞，就绪变化经 socket 的 Event 通知使用者（协议见 `network_protocol`）。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod dispatcher;
mod identity;
mod inet;
mod namespace;
mod packet;
mod receive;
mod server;
mod time;
mod worker;

// 可睡眠同步原语在系统服务间共享；协议引擎经 `crate::sync` 使用它们。
use service_sync as sync;

#[allow(unused_imports, reason = "日志宏经 `crate::` 路径调用的地方需要根级名字")]
pub(crate) use native_runtime::{debug, error, info, warn};

use native_runtime::Startup;

native_runtime::entry!(main);

fn main(mut startup: Startup) -> i64 {
    let control = native_runtime::Channel::from_handle(startup.take(launch_protocol::netd::CONTROL));
    packet::init();
    worker::start().expect("netd protocol worker");
    dispatcher::start().expect("netd dispatcher");
    server::serve_control(control).expect("netd control service");
    info!("netd ready");
    // 控制、分派与协议推进都在服务线程上；主线程只结束自己。
    native_runtime::task::exit_thread()
}
