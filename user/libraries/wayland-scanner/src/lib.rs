//! Wayland 协议 XML → Rust 类型生成器，供 `wayland-wire` 的 `build.rs` 使用。
//!
//! 只实现 wire 格式所需的最小子集：接口、请求、事件、枚举与参数类型。不生成对象句柄或分发循环，
//! 这些属于使用方（compositor 或 `classic-ui`）。

pub mod protocol;
mod rust;
pub mod xml;

pub use rust::generate;

/// 解析若干协议 XML 文本并合并生成一份 Rust 源码。
///
/// # Errors
///
/// 任何一份 XML 的解析或校验失败，或接口跨文件引用无法解析。
pub fn generate_from_xml(sources: &[&str]) -> Result<String, String> {
    let mut interfaces = Vec::new();
    for source in sources {
        interfaces.extend(protocol::parse(source)?);
    }
    generate(&interfaces)
}

#[cfg(test)]
mod tests;
