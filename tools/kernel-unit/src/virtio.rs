// virtqueue 与 DMA buffer 的纯叶子；transport 只提供 queue 需要的地址类型。

pub(crate) mod transport {
    #[derive(Clone, Copy)]
    pub(in crate::virtio) struct VirtQueueAddresses {
        pub(in crate::virtio) descriptor: u64,
        pub(in crate::virtio) driver: u64,
        pub(in crate::virtio) device: u64,
    }
}

#[path = "../../../kernel/src/devices/virtio/queue.rs"]
pub(crate) mod queue;

#[path = "../../../kernel/src/devices/virtio/queue/dma.rs"]
pub(crate) mod dma;
