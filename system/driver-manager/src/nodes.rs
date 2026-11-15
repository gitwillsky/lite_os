//! 节点拓扑与驱动匹配：把平台事实变成按装配顺序排列的节点，每个节点带绑定的驱动与它独占的资源。
//!
//! 顺序决定设备在汇聚目录中的登记顺序（使用者据此分配 `vda` 等名称），因此是确定的：
//! 1. 首个 VirtIO-MMIO GPU（开机画面最早上屏）；
//! 2. PCI 上的 VirtIO console 与 block；
//! 3. 其余 VirtIO-MMIO transport，按 DTB 顺序；
//! 4. PCI xHCI 主控（USB 功能与它同宿主）。

use alloc::vec::Vec;

use launch_protocol::driver_host::{Bus, Driver, Node};
use mmio::MmioBus;
use native_runtime::{Bti, Resource};

use crate::pci::Function;
use crate::platform::{self, VirtioMmioTransport};

/// VirtIO 1.2 §5 device ID。
const NETWORK: u32 = 1;
const BLOCK: u32 = 2;
const CONSOLE: u32 = 3;
const ENTROPY: u32 = 4;
const GPU: u32 = 16;
const INPUT: u32 = 18;
const SOUND: u32 = 25;
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
/// PCI class code：serial bus / USB / xHCI。
const XHCI_CLASS: u32 = 0x0c_0330;
const PCI_CONFIG_BYTES: usize = 4096;

/// 一个已匹配驱动的节点：坐标、驱动与只属于它的资源（宿主每次启动拿到副本）。
pub(crate) struct BoundNode {
    pub(crate) node: Node,
    pub(crate) bti: Bti,
    pub(crate) irq: Option<Resource>,
    pub(crate) mmio: Vec<Resource>,
}

/// 枚举平台并匹配驱动。
///
/// # Panics
///
/// PCI 枚举失败时 fail-stop：BAR 不完整的设备不能交给驱动。
pub(crate) fn enumerate() -> Vec<BoundNode> {
    let functions = crate::pci::enumerate(platform::pci_host()).expect("PCI root bus enumeration");
    let transports: Vec<(VirtioMmioTransport, u32)> = platform::virtio_mmio_transports()
        .iter()
        .filter_map(|transport| Some((*transport, virtio_mmio_device(transport)?)))
        .collect();
    let mut nodes = Vec::new();
    let first_gpu = transports.iter().position(|(_, device)| *device == GPU);
    if let Some(index) = first_gpu {
        nodes.extend(mmio_node(&transports[index].0, GPU));
    }
    for function in &functions {
        if let Some(driver) = virtio_pci_driver(function) {
            nodes.push(pci_node(function, driver));
        }
    }
    for (index, (transport, device)) in transports.iter().enumerate() {
        if Some(index) != first_gpu {
            nodes.extend(mmio_node(transport, *device));
        }
    }
    for function in &functions {
        if function
            .config
            .read_u32(8)
            .is_ok_and(|class| class >> 8 == XHCI_CLASS)
        {
            nodes.push(pci_node(function, Driver::Xhci));
        }
    }
    nodes
}

/// VirtIO-MMIO §4.2.2：读出 device ID；0 表示 transport 存在但未挂接设备。
fn virtio_mmio_device(transport: &VirtioMmioTransport) -> Option<u32> {
    let base = platform::map_device_window(transport.base_addr, transport.size);
    let device = MmioBus::new(base, transport.size)
        .ok()
        .and_then(|bus| bus.read_u32(0x08).ok());
    match device {
        Some(0) => None,
        Some(device) => Some(device),
        None => {
            warn!("Invalid VirtIO MMIO window at {:#x}", transport.base_addr);
            None
        }
    }
}

fn mmio_node(transport: &VirtioMmioTransport, device: u32) -> Option<BoundNode> {
    let driver = match device {
        NETWORK => Driver::VirtioNetwork,
        BLOCK => Driver::VirtioBlock,
        CONSOLE => Driver::VirtioConsole,
        ENTROPY => Driver::VirtioEntropy,
        GPU => Driver::VirtioGpu,
        INPUT => Driver::VirtioInput,
        SOUND => Driver::VirtioSound,
        _ => {
            info!(
                "Unsupported VirtIO device ID {} at {:#x}",
                device, transport.base_addr
            );
            return None;
        }
    };
    Some(BoundNode {
        node: Node {
            driver,
            bus: Bus::VirtioMmio {
                base: transport.base_addr as u64,
                size: transport.size as u64,
                irq: transport.irq,
            },
        },
        bti: platform::bti(None),
        irq: Some(platform::irq_resource(transport.irq)),
        mmio: alloc::vec![platform::mmio_resource(transport.base_addr, transport.size).0],
    })
}

/// UTM 产品路径通过 PCI 暴露 virtio-console 与 virtio-block；其余 VirtIO PCI 类型未绑定驱动。
fn virtio_pci_driver(function: &Function) -> Option<Driver> {
    let config = &function.config;
    if config.read_u16(0x00).ok()? != PCI_VENDOR_VIRTIO {
        return None;
    }
    let pci_device = config.read_u16(0x02).ok()?;
    let device = if (0x1040..=0x107f).contains(&pci_device) {
        u32::from(pci_device - 0x1040)
    } else if (0x1000..=0x103f).contains(&pci_device) {
        u32::from(config.read_u16(0x2e).ok()?)
    } else {
        return None;
    };
    match device {
        NETWORK => Some(Driver::VirtioNetwork),
        CONSOLE => Some(Driver::VirtioConsole),
        BLOCK => Some(Driver::VirtioBlock),
        _ => None,
    }
}

fn pci_node(function: &Function, driver: Driver) -> BoundNode {
    let mut mmio =
        alloc::vec![platform::mmio_resource(function.config_address, PCI_CONFIG_BYTES).0];
    mmio.extend(
        function
            .bars
            .iter()
            .flatten()
            .map(|bar| platform::mmio_resource(bar.address as usize, bar.size as usize).0),
    );
    BoundNode {
        node: Node {
            driver,
            bus: Bus::Pci {
                config: function.config_address as u64,
                bars: function.bars,
                interrupt: function.interrupt,
            },
        },
        bti: platform::bti(function.stream),
        irq: function.interrupt.map(platform::irq_resource),
        mmio,
    }
}
