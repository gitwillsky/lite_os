//! 块驱动提供者的非阻塞队列；文件系统的同步 BlockDevice 视图不承担 DMA 完成分派。
use super::*;
use alloc::boxed::Box;
use native_runtime::Event;

/// 队列拥有的命令；被接受的传输在完成被消费前保活缓冲，不能借用调用栈上的 VMO owner。
pub enum BlockCommand {
    /// 一段设备块与已经登记的 VMO 字节区间之间的传输。
    Transfer {
        /// 相对设备的读写方向。
        operation: BlockOperation,
        /// 本设备逻辑块区间起点。
        first_block: u64,
        /// 非零块数，不超过提供者协商上限。
        blocks: usize,
        /// 真正登记的缓冲 owner，接受后由队列保活至完成消费。
        buffer: Arc<dyn BlockBuffer>,
        /// 缓冲内字节偏移，按逻辑块对齐。
        buffer_offset: u64,
    },
    /// 把调用者已排序的先前写入推进到设备持久化边界。
    Flush,
}

/// 一次非阻塞准入的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSubmission {
    /// 请求已接收，之后恰有一个同 id 完成。
    Queued,
    /// 无需设备事务，命令已同步成功（例如不需要显式 flush 的设备）。
    Completed,
    /// 未接收；容量变化会通知队列 Event，调用者保留命令重试。
    Busy,
}

/// 已停止访问目标缓冲的完成记录。
#[derive(Debug, Clone, Copy)]
pub struct BlockIoCompletion {
    /// 提交者提供的原始身份，不是硬件 slot/head。
    pub id: u64,
    /// DMA 已结束后的结果；错误目标的内容不可用。
    pub result: Result<(), BlockError>,
}

/// 一个驱动提交者的队列；同一 owner 串行提交与收割，不跨设备等待持锁。
///
/// Drop 必须排空已接收请求或先撤销 DMA，再释放缓冲；不能把丢弃队列当作取消已经发布的 DMA。
pub trait BlockIoQueue: Send {
    /// 完成或容量变化通知；可与同设备其他队列共享，使用者须 clear → arm → drain 后再等待。
    fn event(&self) -> &Event;
    /// 尝试接收 `id`/`command`，不等待设备或容量；Queued 之后保持 id 唯一，直到 reap 消费。
    /// Busy 可保留 FIFO 准入登记；重试同一命令，换命令或 Drop 时由实现撤销旧登记/归还未发布的 grant。
    /// # Errors
    /// 参数、登记身份或设备错误；错误不接收命令、没有该命令的 DMA。
    fn try_submit(
        &mut self,
        id: u64,
        command: &BlockCommand,
    ) -> Result<BlockSubmission, BlockError>;
    /// 收割至多 output.len 个完成，返回实际数量；返回的记录都已停止 DMA 并归还本队列的借用。
    fn reap(&mut self, output: &mut [BlockIoCompletion]) -> usize;
    /// 本队列是否已没有接收但未收割的请求，用于持久化屏障及退出。
    fn is_idle(&self) -> bool;
}

/// 实际块驱动/范围装配的提交能力。远端存储 client 使用 BlockDevice，设备发布只接收本能力。
/// 两种视图复用同一硬件请求 owner；不得用同步调用或新增阻塞线程实现非阻塞队列。
pub trait BlockProvider: BlockDevice {
    /// 创建独立调用者身份/完成 owner；几何和硬件容量仍由实际设备唯一拥有。
    /// # Errors
    /// 有界队列存储分配失败或设备已失效。
    fn open_queue(self: Arc<Self>) -> Result<Box<dyn BlockIoQueue>, BlockError>;
}
