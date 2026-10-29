//! 硬件驱动按总线分组；device 拥有设备类接口，具体总线 adapter 只实现这些接口。
//! 子领域的依赖分别由架构契约检查；本入口不转发旧命名空间或装配设备。
pub(crate) mod pci;
pub(crate) mod usb;
pub(crate) mod virtio;
