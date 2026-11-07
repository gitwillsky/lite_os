//! 块设备会话：VMO 登记与区间描述；块内容不进入 channel 消息。
use crate::directory::{self, Class, Directory, OpenedDevice, Session, SessionState};
use crate::{Reader, Writer, call, take_handle};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use device_model::{
    BlockBuffer, BlockBufferPool, BlockDevice, BlockError, BlockOperation, BlockRequest,
};
use native_runtime::native_abi::{self, Status, rights};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Handle, Vmo};

const MAGIC: u64 = u64::from_le_bytes(*b"LBLOCK03");
const FLUSH: u32 = 3;
const INFO: u32 = 4;
const CONNECT: u32 = 5;
const ATTACH: u32 = 6;
const DETACH: u32 = 7;
const TRANSFER: u32 = 8;
const TRANSFER_BATCH: u32 = 9;
/// 一条批量消息最多携带的传输数（每项 36 字节，远小于 channel 消息上限）。
const MAX_BATCH: usize = 64;
const WORKERS: usize = 8;

fn encode_error(error: BlockError) -> Status {
    match error {
        BlockError::InvalidBlock => native_abi::ERR_OUT_OF_RANGE,
        BlockError::IoError => native_abi::ERR_IO,
        BlockError::DeviceError => native_abi::ERR_INTERNAL,
        BlockError::OutOfMemory => native_abi::ERR_NO_MEMORY,
    }
}
fn decode_error(status: Status) -> BlockError {
    match status {
        native_abi::ERR_OUT_OF_RANGE | native_abi::ERR_INVALID_ARGS => BlockError::InvalidBlock,
        native_abi::ERR_IO => BlockError::IoError,
        native_abi::ERR_NO_MEMORY | native_abi::ERR_NO_RESOURCES => BlockError::OutOfMemory,
        _ => BlockError::DeviceError,
    }
}
fn describe(writer: &mut Writer, device: &dyn BlockDevice) {
    writer
        .u64(MAGIC)
        .bytes(device.disk_name())
        .u64(device.block_size() as u64)
        .u64(device.block_count())
        .u64(device.max_transfer_blocks() as u64)
        .u64(device.queue_depth() as u64);
}

/// 子范围只平移设备区间，登记仍由实际硬件提供者拥有。
struct BlockRange {
    device: Arc<dyn BlockDevice>,
    first: u64,
    count: u64,
}
impl BlockRange {
    fn translate(&self, first: u64, count: usize) -> Result<u64, BlockError> {
        if count == 0
            || first
                .checked_add(count as u64)
                .is_none_or(|end| end > self.count)
        {
            return Err(BlockError::InvalidBlock);
        }
        self.first
            .checked_add(first)
            .ok_or(BlockError::InvalidBlock)
    }
}
impl BlockDevice for BlockRange {
    fn disk_name(&self) -> &[u8] {
        self.device.disk_name()
    }
    fn attach_buffer(&self, vmo: Vmo) -> Result<Arc<dyn BlockBuffer>, BlockError> {
        self.device.attach_buffer(vmo)
    }
    fn transfer(
        &self,
        operation: BlockOperation,
        first: u64,
        count: usize,
        buffer: &dyn BlockBuffer,
        offset: u64,
    ) -> Result<(), BlockError> {
        self.device.transfer(
            operation,
            self.translate(first, count)?,
            count,
            buffer,
            offset,
        )
    }
    fn transfer_batch(&self, requests: &[BlockRequest<'_>]) -> Result<(), BlockError> {
        let mut translated: Vec<BlockRequest<'_>> = Vec::new();
        translated
            .try_reserve_exact(requests.len())
            .map_err(|_| BlockError::OutOfMemory)?;
        for request in requests {
            translated.push(BlockRequest {
                first_block: self.translate(request.first_block, request.blocks)?,
                ..*request
            });
        }
        self.device.transfer_batch(&translated)
    }
    fn read_block_ranges(&self, ranges: &mut [(usize, &mut [u8])]) -> Result<(), BlockError> {
        let block_size = self.block_size();
        let mut translated: Vec<(usize, &mut [u8])> = Vec::new();
        translated
            .try_reserve_exact(ranges.len())
            .map_err(|_| BlockError::OutOfMemory)?;
        for (first, output) in ranges.iter_mut() {
            if !output.len().is_multiple_of(block_size) {
                return Err(BlockError::InvalidBlock);
            }
            let device_first = self.translate(*first as u64, output.len() / block_size)?;
            translated.push((
                usize::try_from(device_first).map_err(|_| BlockError::InvalidBlock)?,
                &mut **output,
            ));
        }
        self.device.read_block_ranges(&mut translated)
    }
    fn max_transfer_blocks(&self) -> usize {
        self.device.max_transfer_blocks()
    }
    fn queue_depth(&self) -> usize {
        self.device.queue_depth()
    }
    fn read_blocks(&self, first: usize, output: &mut [u8]) -> Result<usize, BlockError> {
        if !output.len().is_multiple_of(self.block_size()) {
            return Err(BlockError::InvalidBlock);
        }
        let first = self.translate(first as u64, output.len() / self.block_size())?;
        self.device.read_blocks(
            usize::try_from(first).map_err(|_| BlockError::InvalidBlock)?,
            output,
        )
    }
    fn write_blocks(&self, first: usize, input: &[u8]) -> Result<usize, BlockError> {
        if !input.len().is_multiple_of(self.block_size()) {
            return Err(BlockError::InvalidBlock);
        }
        let first = self.translate(first as u64, input.len() / self.block_size())?;
        self.device.write_blocks(
            usize::try_from(first).map_err(|_| BlockError::InvalidBlock)?,
            input,
        )
    }
    fn flush(&self) -> Result<(), BlockError> {
        self.device.flush()
    }
    fn block_size(&self) -> usize {
        self.device.block_size()
    }
    fn block_count(&self) -> u64 {
        self.count
    }
}

/// 登记表在 SessionEnd 之前析构；实际传输先克隆条目，再在表锁外等待 DMA。
struct BlockSession {
    device: Arc<dyn BlockDevice>,
    buffers: Mutex<Vec<Option<Arc<dyn BlockBuffer>>>>,
}
impl BlockSession {
    fn new(device: Arc<dyn BlockDevice>) -> Self {
        Self {
            device,
            buffers: Mutex::new(Vec::new()),
        }
    }
    fn index(id: u64) -> Result<usize, Status> {
        usize::try_from(id.checked_sub(1).ok_or(native_abi::ERR_INVALID_ARGS)?)
            .map_err(|_| native_abi::ERR_INVALID_ARGS)
    }
}
fn serve_device(session: SessionState<BlockSession>) -> Result<(Channel, Channel), Status> {
    let (client, server) = Channel::create()?;
    let server = directory::serve_session(
        server,
        WORKERS,
        b"block-server",
        Arc::try_new(
            move |ordinal: u32,
                  request: &mut Reader<'_>,
                  mut handles: Vec<Handle>,
                  reply: &mut Writer| {
                let session = session.get();
                let device = &session.device;
                match ordinal {
                    INFO => describe(reply, &**device),
                    FLUSH => device.flush().map_err(encode_error)?,
                    ATTACH => {
                        let buffer = device
                            .attach_buffer(Vmo::from_handle(take_handle(&mut handles, 0)?))
                            .map_err(encode_error)?;
                        let mut buffers = session.buffers.lock();
                        let index = if let Some(index) = buffers.iter().position(Option::is_none) {
                            buffers[index] = Some(buffer);
                            index
                        } else {
                            buffers
                                .try_reserve(1)
                                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                            let index = buffers.len();
                            buffers.push(Some(buffer));
                            index
                        };
                        reply.u64(index as u64 + 1);
                    }
                    DETACH => {
                        let index = BlockSession::index(request.u64()?)?;
                        let retired = session
                            .buffers
                            .lock()
                            .get_mut(index)
                            .and_then(Option::take)
                            .ok_or(native_abi::ERR_NOT_FOUND)?;
                        drop(retired);
                    }
                    TRANSFER => {
                        let operation = match request.u32()? {
                            0 => BlockOperation::Read,
                            1 => BlockOperation::Write,
                            _ => return Err(native_abi::ERR_INVALID_ARGS),
                        };
                        let index = BlockSession::index(request.u64()?)?;
                        let first = request.u64()?;
                        let count = request.usize()?;
                        let offset = request.u64()?;
                        let buffer = session
                            .buffers
                            .lock()
                            .get(index)
                            .and_then(Option::as_ref)
                            .cloned()
                            .ok_or(native_abi::ERR_NOT_FOUND)?;
                        device
                            .transfer(operation, first, count, &*buffer, offset)
                            .map_err(encode_error)?;
                    }
                    TRANSFER_BATCH => {
                        let count = request.u32()? as usize;
                        if count == 0 || count > MAX_BATCH {
                            return Err(native_abi::ERR_INVALID_ARGS);
                        }
                        // 先解析并克隆全部登记（表锁内），再在锁外并发提交整批。
                        let mut entries: Vec<(BlockOperation, u64, usize, u64, usize)> =
                            Vec::new();
                        entries
                            .try_reserve_exact(count)
                            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        let mut buffers: Vec<Arc<dyn BlockBuffer>> = Vec::new();
                        buffers
                            .try_reserve_exact(count)
                            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        {
                            let table = session.buffers.lock();
                            for slot in 0..count {
                                let operation = match request.u32()? {
                                    0 => BlockOperation::Read,
                                    1 => BlockOperation::Write,
                                    _ => return Err(native_abi::ERR_INVALID_ARGS),
                                };
                                let index = BlockSession::index(request.u64()?)?;
                                let first = request.u64()?;
                                let blocks = request.usize()?;
                                let offset = request.u64()?;
                                buffers.push(
                                    table
                                        .get(index)
                                        .and_then(Option::as_ref)
                                        .cloned()
                                        .ok_or(native_abi::ERR_NOT_FOUND)?,
                                );
                                entries.push((operation, first, blocks, offset, slot));
                            }
                        }
                        let mut requests: Vec<BlockRequest<'_>> = Vec::new();
                        requests
                            .try_reserve_exact(count)
                            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        for &(operation, first_block, blocks, buffer_offset, slot) in &entries {
                            requests.push(BlockRequest {
                                operation,
                                first_block,
                                blocks,
                                buffer: &*buffers[slot],
                                buffer_offset,
                            });
                        }
                        device.transfer_batch(&requests).map_err(encode_error)?;
                    }
                    CONNECT => {
                        let first = request.u64()?;
                        let count = request.u64()?;
                        if count == 0
                            || first
                                .checked_add(count)
                                .is_none_or(|end| end > device.block_count())
                        {
                            return Err(native_abi::ERR_OUT_OF_RANGE);
                        }
                        let range: Arc<dyn BlockDevice> = Arc::try_new(BlockRange {
                            device: device.clone(),
                            first,
                            count,
                        })
                        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        let (range, _) =
                            serve_device(SessionState::derived(BlockSession::new(range)))?;
                        return Ok(alloc::vec![range.into_handle()]);
                    }
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    Ok((client, server))
}

/// 发布一个块设备；每次打开产生独立登记表。
///
/// # Errors
/// 目录或会话分配失败。
pub fn publish(directory: &Directory, device: Arc<dyn BlockDevice>) -> Result<(), Status> {
    let mut description = directory::description();
    describe(&mut description, &*device);
    directory
        .add(
            Class::Block,
            &description,
            true,
            Box::new(move |end| {
                let (client, server) =
                    serve_device(SessionState::new(BlockSession::new(device.clone()), end))?;
                Ok(Session {
                    handles: alloc::vec![client.into_handle()],
                    server,
                })
            }),
        )
        .map(drop)
}

/// 可退休的会话 transport；短锁只保活调用的 channel，不跨 I/O。
struct RemoteSession {
    channel: Mutex<Option<Arc<Channel>>>,
}
impl RemoteSession {
    fn channel(&self) -> Result<Arc<Channel>, Status> {
        self.channel
            .lock()
            .clone()
            .ok_or(native_abi::ERR_PEER_CLOSED)
    }
    fn detach(&self, id: u64) {
        let Ok(channel) = self.channel() else { return };
        let mut message = [0u8; 16];
        message[4..8].copy_from_slice(&DETACH.to_le_bytes());
        message[8..].copy_from_slice(&id.to_le_bytes());
        if channel.write(&message, Vec::new()).is_err() {
            // Drop 不分配或等待。无法交付 detach 时退休整个会话；已有调用的 Arc 继续保活到完成，
            // 随后最后的 channel close 让服务端回收全部登记，不能静默遗留固定 VMO。
            let retired = self.channel.lock().take();
            drop(retired);
        }
    }
}
struct RemoteBuffer {
    vmo: Vmo,
    id: u64,
    session: Weak<RemoteSession>,
}
impl BlockBuffer for RemoteBuffer {
    fn vmo(&self) -> &Vmo {
        &self.vmo
    }
}
impl Drop for RemoteBuffer {
    fn drop(&mut self) {
        if let Some(session) = self.session.upgrade() {
            session.detach(self.id);
        }
    }
}

/// 远端块设备；CPU 字节适配与直接 VMO 传输使用同一登记数据面。
pub struct RemoteBlock {
    session: Arc<RemoteSession>,
    name: Vec<u8>,
    block_size: usize,
    block_count: u64,
    max_transfer_blocks: usize,
    queue_depth: usize,
    bytes: BlockBufferPool,
}
fn from_description(channel: Channel, mut reader: Reader<'_>) -> Result<Arc<RemoteBlock>, Status> {
    if reader.u64()? != MAGIC {
        return Err(native_abi::ERR_NOT_SUPPORTED);
    }
    let name = reader.bytes()?;
    let mut owned_name = Vec::new();
    owned_name
        .try_reserve_exact(name.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    owned_name.extend_from_slice(name);
    let block_size = reader.usize()?;
    let block_count = reader.u64()?;
    let max_transfer_blocks = reader.usize()?;
    let queue_depth = reader.usize()?;
    if block_size == 0
        || max_transfer_blocks == 0
        || queue_depth == 0
        || max_transfer_blocks.checked_mul(block_size).is_none()
    {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let channel = Arc::try_new(channel).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let session = Arc::try_new(RemoteSession {
        channel: Mutex::new(Some(channel)),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    Arc::try_new(RemoteBlock {
        session,
        name: owned_name,
        block_size,
        block_count,
        max_transfer_blocks,
        queue_depth,
        bytes: BlockBufferPool::default(),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}
/// 从子范围连接查询不可变设备属性并建立 client。
///
/// # Errors
/// 传输、协议版本、属性或分配失败。
pub fn from_channel(channel: Channel) -> Result<Arc<RemoteBlock>, Status> {
    let reply = call(&channel, &Writer::request(INFO), Vec::new())?;
    from_description(channel, reply.reader())
}
/// 接管目录打开的块设备会话。
///
/// # Errors
/// 设备类型、版本、属性、句柄或分配失败。
pub fn connect(mut announcement: OpenedDevice) -> Result<Arc<RemoteBlock>, Status> {
    if announcement.class != Class::Block {
        return Err(native_abi::ERR_WRONG_TYPE);
    }
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    from_description(channel, announcement.reader())
}
impl RemoteBlock {
    /// 一条消息提交至多 [`MAX_BATCH`] 个传输；校验与单个 `transfer` 相同。
    fn send_batch(&self, requests: &[BlockRequest<'_>]) -> Result<(), BlockError> {
        let mut message = Writer::request(TRANSFER_BATCH);
        message.u32(requests.len() as u32);
        for request in requests {
            let owner: &dyn core::any::Any = request.buffer;
            let buffer = owner
                .downcast_ref::<RemoteBuffer>()
                .ok_or(BlockError::InvalidBlock)?;
            if buffer.session.as_ptr() != Arc::as_ptr(&self.session)
                || request.blocks == 0
                || request.blocks > self.max_transfer_blocks
                || request
                    .first_block
                    .checked_add(request.blocks as u64)
                    .is_none_or(|end| end > self.block_count)
            {
                return Err(BlockError::InvalidBlock);
            }
            message
                .u32(match request.operation {
                    BlockOperation::Read => 0,
                    BlockOperation::Write => 1,
                })
                .u64(buffer.id)
                .u64(request.first_block)
                .u64(request.blocks as u64)
                .u64(request.buffer_offset);
        }
        let channel = self.session.channel().map_err(decode_error)?;
        call(&channel, &message, Vec::new())
            .map_err(decode_error)
            .map(drop)
    }
}

impl BlockDevice for RemoteBlock {
    fn disk_name(&self) -> &[u8] {
        &self.name
    }
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn block_count(&self) -> u64 {
        self.block_count
    }
    fn max_transfer_blocks(&self) -> usize {
        self.max_transfer_blocks
    }
    fn queue_depth(&self) -> usize {
        self.queue_depth
    }
    fn attach_buffer(&self, vmo: Vmo) -> Result<Arc<dyn BlockBuffer>, BlockError> {
        let channel = self.session.channel().map_err(decode_error)?;
        let exported = vmo.duplicate(rights::SAME).map_err(decode_error)?;
        let reply = call(
            &channel,
            &Writer::request(ATTACH),
            alloc::vec![exported.into_handle()],
        )
        .map_err(decode_error)?;
        let id = reply.reader().u64().map_err(decode_error)?;
        if id == 0 {
            return Err(BlockError::DeviceError);
        }
        let buffer = RemoteBuffer {
            vmo,
            id,
            session: Arc::downgrade(&self.session),
        };
        // Arc::try_new 失败也会 Drop buffer，归还刚登记的 id。
        Arc::try_new(buffer)
            .map(|buffer| buffer as Arc<dyn BlockBuffer>)
            .map_err(|_| BlockError::OutOfMemory)
    }
    fn transfer(
        &self,
        operation: BlockOperation,
        first: u64,
        count: usize,
        buffer: &dyn BlockBuffer,
        offset: u64,
    ) -> Result<(), BlockError> {
        let owner: &dyn core::any::Any = buffer;
        let buffer = owner
            .downcast_ref::<RemoteBuffer>()
            .ok_or(BlockError::InvalidBlock)?;
        if buffer.session.as_ptr() != Arc::as_ptr(&self.session)
            || count == 0
            || count > self.max_transfer_blocks
            || first
                .checked_add(count as u64)
                .is_none_or(|end| end > self.block_count)
        {
            return Err(BlockError::InvalidBlock);
        }
        let mut request = Writer::request(TRANSFER);
        request
            .u32(match operation {
                BlockOperation::Read => 0,
                BlockOperation::Write => 1,
            })
            .u64(buffer.id)
            .u64(first)
            .u64(count as u64)
            .u64(offset);
        let channel = self.session.channel().map_err(decode_error)?;
        call(&channel, &request, Vec::new())
            .map_err(decode_error)
            .map(drop)
    }
    fn transfer_batch(&self, requests: &[BlockRequest<'_>]) -> Result<(), BlockError> {
        let mut first_error = None;
        for chunk in requests.chunks(MAX_BATCH) {
            let result = self.send_batch(chunk);
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    fn read_block_ranges(&self, ranges: &mut [(usize, &mut [u8])]) -> Result<(), BlockError> {
        self.bytes.read_many(self, ranges)
    }
    fn read_blocks(&self, first: usize, output: &mut [u8]) -> Result<usize, BlockError> {
        self.bytes.read(self, first, output)
    }
    fn write_blocks(&self, first: usize, input: &[u8]) -> Result<usize, BlockError> {
        self.bytes.write(self, first, input)
    }
    fn flush(&self) -> Result<(), BlockError> {
        let channel = self.session.channel().map_err(decode_error)?;
        call(&channel, &Writer::request(FLUSH), Vec::new())
            .map_err(decode_error)
            .map(drop)
    }
    fn share_range(&self, first: u64, count: u64) -> Option<Handle> {
        if count == 0
            || first
                .checked_add(count)
                .is_none_or(|end| end > self.block_count)
        {
            return None;
        }
        let mut request = Writer::request(CONNECT);
        request.u64(first).u64(count);
        let channel = self.session.channel().ok()?;
        let mut reply = call(&channel, &request, Vec::new()).ok()?;
        take_handle(&mut reply.handles, 0).ok()
    }
}
