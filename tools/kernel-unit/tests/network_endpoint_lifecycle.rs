#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! 完整实际网络 endpoint lifecycle；仅 NIC/时钟/任务唤醒是边界。
extern crate alloc;
include!("../src/network_endpoint_fixture.rs");
