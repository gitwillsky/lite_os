//! devmgr 的设备平台：设备发现、MMIO 窗口、中断与 DMA 发起者。
//!
//! 1. 启动时映射 DTB VMO 并提取设备事实（[`device_tree`]）。
//! 2. MMIO 窗口：从根 resource 切出 MMIO resource，创建物理 VMO 并映射到服务的共享区。
//! 3. 中断：创建电平触发中断对象，由专属服务线程等待、调用处理函数并确认。
//! 4. DMA：设备经 BTI 访问内存；有 IOMMU 时每个 PCI 请求者一个 stream，否则使用恒等映射。

mod device_tree;
mod pci_host;
mod virtio_mmio;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use native_runtime::native_abi;
use native_runtime::sync::{Mutex, Once};
use native_runtime::{Bti, Interrupt, Iommu, Resource, Vmo};

pub(crate) use device_tree::IommuFacts;
pub(crate) use pci_host::PciHost;
pub(crate) use virtio_mmio::{VirtioMmioTransport, VirtioMmioTransports};

use crate::interrupt::{InterruptHandler, InterruptVector};

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

/// 把物理 MMIO 窗口映射到服务地址空间，返回虚拟地址。
///
/// # Panics
///
/// 窗口与 RAM 重叠、越界或映射失败时 fail-stop：驱动无法在没有寄存器的情况下继续装配。
pub(crate) fn map_device_window(physical: usize, size: usize) -> usize {
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

/// 注册设备中断：创建中断对象并启动等待线程。
///
/// # Panics
///
/// 中断对象创建或线程启动失败时带 label/vector fail-stop：静默返回会让设备永远收不到中断。
pub(crate) fn register_device_interrupt(
    vector: InterruptVector,
    handler: Arc<dyn InterruptHandler>,
    label: &'static str,
) {
    let resource = Resource::create(
        crate::handles::resource(),
        native_abi::resource::IRQ,
        u64::from(vector),
        1,
    )
    .unwrap_or_else(|status| panic!("{label} IRQ {vector} resource: {status}"));
    let interrupt = Interrupt::create(&resource, vector, native_abi::interrupt::LEVEL)
        .unwrap_or_else(|status| panic!("{label} IRQ {vector} registration failed: {status}"));
    let mut name = alloc::vec::Vec::from(b"irq:".as_slice());
    name.extend_from_slice(label.as_bytes());
    let thread = native_runtime::thread::spawn(&name, move || {
        loop {
            match interrupt.wait(native_abi::DEADLINE_INFINITE) {
                Ok(_) => {
                    if let Err(error) = handler.handle_interrupt(vector) {
                        warn!("{label} IRQ {vector} handler failed: {error}");
                    }
                    let _ = interrupt.ack();
                }
                Err(native_abi::ERR_CANCELED) => return,
                Err(status) => panic!("{label} IRQ {vector} wait failed: {status}"),
            }
        }
    })
    .unwrap_or_else(|status| panic!("{label} IRQ {vector} thread: {status}"));
    // 中断线程与服务同寿命，分离即可。
    drop(thread);
}

// OWNER: 按 stream 缓存的 BTI；IOMMU 与 BTI 在服务存活期间不销毁。
static BTIS: Mutex<BTreeMap<Option<u32>, &'static Bti>> = Mutex::new(BTreeMap::new());
// OWNER: 平台 IOMMU（有则为硬件后端，否则为恒等映射）。
static IOMMU: Once<Iommu> = Once::new();

fn iommu() -> &'static Iommu {
    IOMMU.call_once(|| {
        let root = crate::handles::resource();
        match facts().iommu {
            Some(IommuFacts { kind, base, size }) => {
                let mut descriptor = [0u8; 16];
                descriptor[..8].copy_from_slice(&(base as u64).to_le_bytes());
                descriptor[8..].copy_from_slice(&(size as u64).to_le_bytes());
                Iommu::create(root, kind, &descriptor)
                    .unwrap_or_else(|status| panic!("IOMMU creation failed: {status}"))
            }
            None => Iommu::create(root, native_abi::iommu::IDENTITY, &[])
                .unwrap_or_else(|status| panic!("identity IOMMU creation failed: {status}")),
        }
    })
}

/// DMA 发起者的 BTI：`stream` 为 PCI 请求者经 `iommu-map` 换算的 stream；不经 IOMMU 的设备传 `None`。
///
/// # Panics
///
/// BTI 创建失败时 fail-stop。
pub(crate) fn bti(stream: Option<u32>) -> &'static Bti {
    let key = stream.filter(|_| facts().iommu.is_some());
    let mut btis = BTIS.lock();
    if let Some(bti) = btis.get(&key) {
        return bti;
    }
    let bti: &'static Bti = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        Bti::create(iommu(), key.unwrap_or(0))
            .unwrap_or_else(|status| panic!("BTI creation failed: {status}")),
    ));
    btis.insert(key, bti);
    bti
}
