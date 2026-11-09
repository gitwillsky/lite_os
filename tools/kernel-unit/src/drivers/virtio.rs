// virtqueue、DMA buffer 与 PCI 寄存器 codec；transport 提供 queue 地址类型。

pub(crate) mod transport {
    #[derive(Clone, Copy)]
    pub(in crate::drivers::virtio) struct VirtQueueAddresses {
        pub(in crate::drivers::virtio) descriptor: u64,
        pub(in crate::drivers::virtio) driver: u64,
        pub(in crate::drivers::virtio) device: u64,
    }
}

#[path = "../../../../tools/source-view/driver-host/drivers/virtio/pci/pci/pci.rs"]
pub(crate) mod pci;

#[path = "../../../../tools/source-view/driver-host/drivers/virtio/queue/queue/queue.rs"]
pub(crate) mod queue;

#[path = "../../../../system/driver-host/src/drivers/virtio/queue/dma.rs"]
pub(crate) mod dma;
