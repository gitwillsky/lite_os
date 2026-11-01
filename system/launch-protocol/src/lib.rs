#![no_std]

//! 根服务与各系统服务之间的启动约定：bootfs 中的映像路径、链接基址与角色句柄下标。
//!
//! 角色句柄下标是启动消息（`native_abi::startup`）中的绝对下标，从 `ROLE_HANDLES` 开始。

use native_abi::startup::ROLE_HANDLES;

/// Linux 兼容层服务。
///
/// 运行形态（Starnix 模型）：映像装载在共享进程的共享区，每个 Linux 进程是该共享进程的一个成员；
/// 成员共用句柄表，服务线程在任一成员中运行时看到同一份服务状态。
pub mod linuxd {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/linuxd";
    /// 静态链接基址；必须位于两种架构的共享区内（`SHARED_START` 不超过该值）。
    pub const BASE: usize = 0x20_0000_0000;
    /// 共享进程（创建 Linux 进程所需的 `process_create_shared` 原型）。
    pub const SHARED_PROCESS: usize = ROLE_HANDLES;
    /// 容纳全部 Linux 进程的 Job；服务 panic 时整体终止。
    pub const JOB: usize = ROLE_HANDLES + 1;
    /// 根 resource（设备驱动在服务内运行）。
    pub const RESOURCE: usize = ROLE_HANDLES + 2;
    /// 平台 DTB VMO。
    pub const DEVICE_TREE: usize = ROLE_HANDLES + 3;
}

/// 根服务。
pub mod root_server {
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x20_0000;
}
