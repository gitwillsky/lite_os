// virtqueue 与 DMA buffer 的纯叶子；transport 只提供 queue 需要的地址类型。

pub(crate) mod transport {
    #[derive(Clone, Copy)]
    pub(in crate::drivers::virtio) struct VirtQueueAddresses {
        pub(in crate::drivers::virtio) descriptor: u64,
        pub(in crate::drivers::virtio) driver: u64,
        pub(in crate::drivers::virtio) device: u64,
    }
}

#[path = "../../../../tools/kernel-view/drivers/virtio/queue/queue/queue.rs"]
pub(crate) mod queue;

#[path = "../../../../kernel/src/drivers/virtio/queue/dma.rs"]
pub(crate) mod dma;
