//! 平台设备事实与节点资源：DTB 解码、MMIO/IRQ resource 切分、IOMMU 与 BTI。
//!
//! 1. 启动时映射 DTB VMO 并提取设备事实（[`device_tree`]）。
//! 2. 每个节点只拿到覆盖自身窗口的 MMIO resource 与自身向量的 IRQ resource（最小权限）。
//! 3. DMA：每个节点一个 BTI；有 IOMMU 时 PCI 请求者按 `iommu-map` 取 stream，否则使用恒等映射。
//!    BTI 由 driver-manager 持有原件、宿主拿副本：宿主崩溃时未解除的固定页留在 BTI 隔离区，
//!    重启的宿主复位设备后释放（Zircon quarantine）。

mod device_tree;
mod pci_host;
mod virtio_mmio;

use native_runtime::native_abi;
use native_runtime::sync::Once;
use native_runtime::{Bti, Iommu, Resource, Vmo};

pub(crate) use device_tree::IommuFacts;
pub(crate) use pci_host::PciHost;
pub(crate) use virtio_mmio::{VirtioMmioTransport, VirtioMmioTransports};

// OWNER: 启动时由 DTB 解码一次的设备事实。
static FACTS: Once<device_tree::DeviceFacts> = Once::new();

/// 解码平台 DTB。
///
/// # Panics
///
/// DTB 无法映射或结构非法时 fail-stop：没有设备事实就无法装配任何驱动。
pub(crate) fn initialize(device_tree: &Vmo) {
    let size = device_tree.size().expect("device tree VMO size") as usize;
    let address = native_runtime::heap::vmar()
        .map(native_abi::vm::READ, 0, device_tree, 0, size)
        .expect("device tree mapping");
    // SAFETY: 映射只读、永不解除；DTB 在服务存活期间不变。
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, size) };
    // DTSpec v17 blob 的 last_comp_version 为 16（QEMU 如此生成）；dtb-walker 要求 17，与内核解码同样放行。
    // 映射按页对齐，不会出现对齐错误。
    let dtb = dtb_walker::Dtb::from_slice_filtered(bytes, |error| {
        matches!(error, dtb_walker::HeaderError::LastCompVersion(_))
    })
    .unwrap_or_else(|error| match error {
        dtb_walker::ConvertError::Header(error) => {
            panic!("platform device tree header is invalid: {error:?}")
        }
        dtb_walker::ConvertError::Truncated => {
            panic!("platform device tree is truncated ({size} bytes mapped)")
        }
    });
    FACTS.call_once(|| device_tree::parse(dtb));
}

fn facts() -> &'static device_tree::DeviceFacts {
    FACTS
        .get()
        .expect("platform facts read before initialization")
}

/// DTB 发现的 VirtIO-MMIO transport 表。
pub(crate) fn virtio_mmio_transports() -> &'static VirtioMmioTransports {
    &facts().virtio
}

/// DTB 发现的 PCI ECAM host。
pub(crate) fn pci_host() -> Option<&'static PciHost> {
    facts().pci.as_ref()
}

/// 覆盖 `[physical, physical + size)` 所在整页的 MMIO resource。
///
/// # Panics
///
/// 区间越界或与 RAM 重叠时 fail-stop。
pub(crate) fn mmio_resource(physical: usize, size: usize) -> (Resource, usize, usize) {
    let page = native_abi::PAGE_SIZE;
    let start = physical & !(page - 1);
    let end = (physical + size).next_multiple_of(page);
    let resource = Resource::create(
        crate::handles::resource(),
        native_abi::resource::MMIO,
        start as u64,
        (end - start) as u64,
    )
    .unwrap_or_else(|status| panic!("MMIO resource {start:#x}+{:#x}: {status}", end - start));
    (resource, start, end)
}

/// 把物理 MMIO 窗口映射到本进程，返回虚拟地址（只用于读取设备身份与 PCI 配置）。
///
/// # Panics
///
/// resource 或映射失败时 fail-stop。
pub(crate) fn map_device_window(physical: usize, size: usize) -> usize {
    let (resource, start, end) = mmio_resource(physical, size);
    let vmo = Vmo::create_physical(&resource, start as u64, (end - start) as u64)
        .unwrap_or_else(|status| panic!("MMIO VMO {start:#x}: {status}"));
    let base = native_runtime::heap::vmar()
        .map(
            native_abi::vm::READ | native_abi::vm::WRITE,
            0,
            &vmo,
            0,
            end - start,
        )
        .unwrap_or_else(|status| panic!("MMIO mapping {start:#x}: {status}"));
    base + (physical - start)
}

/// 单个中断向量的 IRQ resource。
///
/// # Panics
///
/// 向量越界时 fail-stop。
pub(crate) fn irq_resource(vector: u32) -> Resource {
    Resource::create(
        crate::handles::resource(),
        native_abi::resource::IRQ,
        u64::from(vector),
        1,
    )
    .unwrap_or_else(|status| panic!("IRQ {vector} resource: {status}"))
}

// OWNER: 恒等映射与硬件翻译属于不同 DMA 路由，各 Once 与服务同寿命。
// PROOF: DTB stream map 决定路由；不经 IOMMU 的 MMIO 设备不能使用硬件 domain 的 IOVA。
// FAILURE: 混用会使 MMIO 设备把 IOVA 当成物理地址，读写错误内存或永远不完成请求。
static IDENTITY_IOMMU: Once<Iommu> = Once::new();
static HARDWARE_IOMMU: Once<Iommu> = Once::new();

fn iommu(stream: Option<u32>) -> &'static Iommu {
    let root = crate::handles::resource();
    if stream.is_none() {
        return IDENTITY_IOMMU.call_once(|| {
            Iommu::create(root, native_abi::iommu::IDENTITY, &[])
                .unwrap_or_else(|status| panic!("identity IOMMU creation failed: {status}"))
        });
    }
    HARDWARE_IOMMU.call_once(|| {
        let IommuFacts {
            kind, base, size, ..
        } = facts().iommu.expect("DMA stream lacks IOMMU");
        let mut descriptor = [0u8; 16];
        descriptor[..8].copy_from_slice(&(base as u64).to_le_bytes());
        descriptor[8..].copy_from_slice(&(size as u64).to_le_bytes());
        Iommu::create(root, kind, &descriptor)
            .unwrap_or_else(|status| panic!("IOMMU creation failed: {status}"))
    })
}

/// 节点的 DMA 发起者：`stream` 为 PCI 请求者经 `iommu-map` 换算的 stream；不经 IOMMU 的设备传 `None`。
///
/// # Panics
///
/// BTI 创建失败时 fail-stop。
pub(crate) fn bti(stream: Option<u32>) -> Bti {
    Bti::create(iommu(stream), stream.unwrap_or(0))
        .unwrap_or_else(|status| panic!("BTI creation failed: {status}"))
}

/// 仅授予一个消息设备 ID；与该节点 BTI 一并交付，不授予原始 LPI/SPI 范围。
/// # Panics
/// 原生 authority 创建失败时 fail-stop。
pub(crate) fn msi_resource(device: u32) -> Resource {
    Resource::create(
        crate::handles::resource(),
        native_abi::resource::MSI,
        u64::from(device),
        1,
    )
    .unwrap_or_else(|status| panic!("MSI device {device:#x} resource: {status}"))
}
