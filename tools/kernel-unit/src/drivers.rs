#[path = "../../../kernel/src/drivers/block.rs"]
pub(crate) mod block;

#[path = "../../../kernel/src/drivers/io_completion.rs"]
pub(crate) mod io_completion;

pub(crate) mod hal {
    #[derive(Clone, Copy)]
    pub(in crate::drivers) struct VirtQueueAddresses {
        pub(in crate::drivers) descriptor: u64,
        pub(in crate::drivers) driver: u64,
        pub(in crate::drivers) device: u64,
    }
}

#[path = "../../../kernel/src/drivers/virtio_queue.rs"]
pub(crate) mod virtio_queue;

#[path = "../../../kernel/src/drivers/virtio_queue/dma.rs"]
pub(crate) mod virtio_dma;
