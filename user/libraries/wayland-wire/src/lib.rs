//! Wayland wire 协议编解码：对象 ID、消息帧、参数读写，以及由协议 XML 生成的接口类型。
//!
//! 这里只处理字节与文件描述符队列，不拥有 socket：收发由使用方（compositor、`classic-ui`）通过
//! `sendmsg`/`recvmsg` 完成，并把收到的 fd 放进 [`wire::Reader`] 使用的队列。

extern crate liteos_runtime as _;
pub mod wire;

/// 由 `protocols/*.xml` 生成的接口模块、`Interface` 枚举与按接口分发的解码函数。
#[allow(clippy::all)]
pub mod protocol {
    include!(concat!(env!("OUT_DIR"), "/protocol.rs"));
}

#[cfg(test)]
mod tests;
