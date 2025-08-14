#![allow(dead_code)]

use alloc::vec::Vec;

// 极简协议：长度前缀(u32) + kind(u32) + payload
// kind 定义
pub const MSG_BUFFER_COMMIT: u32 = 1; // payload: handle:u32,w:u32,h:u32,stride:u32,dx:i32,dy:i32

pub fn encode_u32_le(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn encode_i32_le(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn encode_frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + payload.len());
    encode_u32_le(&mut buf, payload.len() as u32);
    encode_u32_le(&mut buf, kind);
    buf.extend_from_slice(payload);
    buf
}

pub fn build_payload_buffer_commit(handle: u32, w: u32, h: u32, stride: u32, dx: i32, dy: i32) -> Vec<u8> {
    let mut p = Vec::with_capacity(24);
    encode_u32_le(&mut p, handle);
    encode_u32_le(&mut p, w);
    encode_u32_le(&mut p, h);
    encode_u32_le(&mut p, stride);
    encode_i32_le(&mut p, dx);
    encode_i32_le(&mut p, dy);
    p
}


