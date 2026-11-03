#![no_std]
#![no_main]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

//! ext4：ext4 文件系统服务。
//!
//! 每次挂载由 linuxd 经控制 channel 请求：携带块设备连接，ext4 打开 ext4 并在一条新 channel 上以
//! FUSE 线协议服务该挂载；linuxd 的 FUSE 客户端经这条 channel 访问文件。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod content;
mod ext4;
mod fuse_server;
mod time;
mod vfs;

// 可睡眠同步原语在系统服务间共享；ext4 引擎经 `crate::sync` 使用它们。
use service_sync as sync;

#[allow(
    unused_imports,
    reason = "日志宏经 `crate::` 路径调用的地方需要根级名字"
)]
pub(crate) use native_runtime::{debug, error, info, warn};

use native_runtime::Startup;

native_runtime::entry!(main);

fn main(mut startup: Startup) -> i64 {
    let control =
        native_runtime::Channel::from_handle(startup.take(launch_protocol::ext4::CONTROL));
    fuse_server::serve_control(control)
}
