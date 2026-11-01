//! 运行时堆所在的 VMAR；由启动消息安装，服务也用它映射私有工作内存。
//!
//! 系统服务目标使用 [`allocator`] 的分级分配器，宿主测试由宿主拥有分配和 panic。

use crate::Vmar;
use crate::sync::Once;

#[cfg(target_os = "none")]
mod allocator;

#[cfg(target_os = "none")]
#[global_allocator]
static HEAP: allocator::Heap = allocator::Heap;

// OWNER: 启动消息唯一安装；缺失时服务不能映射工作内存。
static VMAR: Once<Vmar> = Once::new();

/// 安装分配用的 VMAR。
pub(crate) fn initialize(vmar: Vmar) {
    VMAR.call_once(|| vmar);
}

/// 分配所用的 VMAR（服务用它映射自己的内存，与堆处于同一区域）。
///
/// # Panics
///
/// 运行时启动前调用。
pub fn vmar() -> &'static Vmar {
    VMAR.get().expect("runtime heap VMAR not installed")
}
