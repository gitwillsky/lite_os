#![no_std]
#![no_main]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

//! netstack：网络服务。
//!
//! 拥有 IPv4 协议栈（smoltcp）、网卡、接口与路由、TCP/UDP/ICMP/RAW endpoint 与 packet socket。
//! 启动时只有带回环的默认网络；netcfg 经安装协议加入网卡（netstack 重启后由它重新安装）。经 outgoing 接受
//! 控制与安装连接；
//! 使用者为每个网络域创建域 channel，经域 channel 创建 socket；每个 socket 一条 channel，全部操作不阻塞，
//! 就绪变化经 socket 的 Event 通知使用者（协议见 `network_protocol`）。

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

#[allow(
    unused_imports,
    reason = "日志宏经 `crate::` 路径调用的地方需要根级名字"
)]
pub(crate) use native_runtime::{debug, error, info, warn};

use native_runtime::Startup;

native_runtime::entry!(main);

fn main(mut startup: Startup) -> i64 {
    let outgoing =
        native_runtime::Channel::from_handle(startup.take(launch_protocol::netstack::OUTGOING));
    packet::init();
    worker::start().expect("netstack protocol worker");
    dispatcher::start().expect("netstack dispatcher");
    let network = server::start().expect("netstack default network");
    service_directory::serve_directory(
        outgoing,
        b"netstack-outgoing",
        alloc::sync::Arc::try_new(move |protocol: &[u8], channel: native_runtime::Channel| {
            if protocol == service_directory::NETWORK {
                server::serve_control(network.clone(), channel)
            } else if protocol == service_directory::NETWORK_INSTALLER {
                server::serve_installer(network.clone(), channel)
            } else {
                Err(native_runtime::native_abi::ERR_NOT_FOUND)
            }
        })
        .expect("netstack outgoing connector"),
    )
    .expect("netstack outgoing service");
    info!("netstack ready");
    // 控制、分派与协议推进都在服务线程上；主线程只结束自己。
    native_runtime::task::exit_thread()
}
