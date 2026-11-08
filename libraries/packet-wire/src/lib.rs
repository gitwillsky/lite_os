#![no_std]
//! 报文字节机制；当前提供 RFC 1071 checksum，其余格式按路由计划在本 crate 统一实现。
pub mod checksum;
