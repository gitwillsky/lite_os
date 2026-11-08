//! 消息存储在队列/调用者间唯一转移；每对存活 channel 只保留一份已清空的缓冲。
use super::*;
use alloc::sync::Weak;
use core::mem::{size_of, take};

/// 只含存储的消息载荷；空闲池中两个 Vec 的长度始终为零，不保活任何句柄。
#[derive(Default)]
pub(super) struct Buffers {
    bytes: Vec<u8>,
    handles: Vec<HandleEntry>,
}
impl Buffers {
    fn capacity_bytes(&self) -> usize {
        self.bytes
            .capacity()
            .saturating_add(self.handles.capacity() * size_of::<HandleEntry>())
    }
}
/// 一个完整消息的存储预算；空闲池不保留超过单条协议容量的历史分配。
const MAX_IDLE_BYTES: usize =
    native_abi::CHANNEL_MAX_BYTES + native_abi::CHANNEL_MAX_HANDLES * size_of::<HandleEntry>();

/// 一条 channel 消息；句柄只能借用或移入目标表，析构在锁外销毁未交付的句柄再回收空存储。
pub(crate) struct Message {
    buffers: Buffers,
    // OWNER: 弱引用只用于归还空 Vec，不形成 inbound -> ChannelShared 的保活环。
    recycle: Weak<ChannelShared>,
}
impl Message {
    /// 接收已有的内核启动/异常消息，写入端点时统一绑定存储回收目标。
    pub(crate) fn new(bytes: Vec<u8>, handles: Vec<HandleEntry>) -> Self {
        assert!(
            bytes.len() <= native_abi::CHANNEL_MAX_BYTES
                && handles.len() <= native_abi::CHANNEL_MAX_HANDLES,
            "kernel message exceeds channel capacity"
        );
        Self {
            buffers: Buffers { bytes, handles },
            recycle: Weak::new(),
        }
    }
    pub(super) fn bind(&mut self, shared: &Arc<ChannelShared>) {
        self.recycle = Arc::downgrade(shared);
    }
    /// 消息字节；观察不改变存储或其寿命。
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buffers.bytes
    }
    /// 独占消息字节，供用户复制与事务号编码；不得在端点队列内修改。
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buffers.bytes
    }
    /// 尚未转移的句柄；交付失败由本消息在锁外销毁。
    pub(crate) fn handles_mut(&mut self) -> &mut Vec<HandleEntry> {
        &mut self.buffers.handles
    }
    /// 实际消息字节数与句柄数。
    pub(crate) fn actual(&self) -> ChannelActual {
        ChannelActual {
            bytes: self.buffers.bytes.len() as u32,
            handles: self.buffers.handles.len() as u32,
        }
    }
    /// 队列计费包括两份 Vec 的容量，并按两份消息槽计入固定开销，为队列扩容留余量。
    pub(super) fn charge(&self) -> usize {
        self.buffers
            .capacity_bytes()
            .saturating_add(2 * size_of::<Self>())
    }
    pub(super) fn txid(&self) -> Option<u32> {
        let bytes = self.bytes().get(..native_abi::CHANNEL_TXID_BYTES)?;
        Some(u32::from_ne_bytes(bytes.try_into().expect("txid width")))
    }
}
impl Drop for Message {
    fn drop(&mut self) {
        // 1. 先在 channel 锁外释放能力；关闭携带的其他端点可以递归进入它自己的 channel。
        // 2. 池只能接收空存储，不暴露旧消息字节，也不保留消息拥有的对象。
        // 3. 只有两端仍打开才保留一份，覆盖的空存储在锁外释放；关闭/分配失败无特殊回收线程。
        self.buffers.handles.clear();
        self.buffers.bytes.clear();
        if self.buffers.capacity_bytes() == 0 || self.buffers.capacity_bytes() > MAX_IDLE_BYTES {
            return;
        }
        let Some(shared) = self.recycle.upgrade() else {
            return;
        };
        let retired = {
            let mut state = shared.state.lock();
            if !state.sides.iter().all(|side| side.open) {
                return;
            }
            core::mem::replace(&mut state.spare, take(&mut self.buffers))
        };
        drop(retired);
    }
}
impl Channel {
    /// 准备可写消息，复用本端点对的空存储；所有分配与用户缺页均在端点锁外。
    ///
    /// # Parameters
    /// - `length`、`handles`: 消息字节数与将要移入的句柄数。
    /// # Errors
    /// 超出协议容量返回 OUT_OF_RANGE，分配失败返回 NO_MEMORY；失败不消费调用者句柄。
    pub(crate) fn prepare_message(&self, length: usize, handles: usize) -> Result<Message, Status> {
        if length > native_abi::CHANNEL_MAX_BYTES || handles > native_abi::CHANNEL_MAX_HANDLES {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        let buffers = take(&mut self.shared.state.lock().spare);
        let mut message = Message {
            buffers,
            recycle: Arc::downgrade(&self.shared),
        };
        message
            .buffers
            .bytes
            .try_reserve_exact(length)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        message
            .buffers
            .handles
            .try_reserve_exact(handles)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        // Vec 长度在空闲池内为零；重新发表前覆盖全部有效字节，不向接收者暴露旧内容。
        message.buffers.bytes.resize(length, 0);
        Ok(message)
    }
}
