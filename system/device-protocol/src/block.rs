//! 块设备：整块读写与持久化屏障，块内容随请求/回复内联传递。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::{BlockDevice, BlockError};
use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Handle};

use crate::directory::{self, Class, Directory, OpenedDevice, Session, SessionState};
use crate::{Reader, Writer, call, take_handle};

const READ: u32 = 1;
const WRITE: u32 = 2;
const FLUSH: u32 = 3;
const INFO: u32 = 4;
const CONNECT: u32 = 5;

/// 块请求在驱动内并发等待完成：工作线程数与驱动的请求槽数同量级。
const WORKERS: usize = 8;

const ERROR_INVALID_BLOCK: Status = native_abi::ERR_OUT_OF_RANGE;
const ERROR_IO: Status = native_abi::ERR_IO;
const ERROR_DEVICE: Status = native_abi::ERR_INTERNAL;

fn encode_error(error: BlockError) -> Status {
    match error {
        BlockError::InvalidBlock => ERROR_INVALID_BLOCK,
        BlockError::IoError => ERROR_IO,
        BlockError::DeviceError => ERROR_DEVICE,
        BlockError::OutOfMemory => native_abi::ERR_NO_MEMORY,
    }
}

fn decode_error(status: Status) -> BlockError {
    match status {
        ERROR_INVALID_BLOCK => BlockError::InvalidBlock,
        ERROR_IO => BlockError::IoError,
        native_abi::ERR_NO_MEMORY => BlockError::OutOfMemory,
        _ => BlockError::DeviceError,
    }
}

/// 设备的一段块范围（经 CONNECT 交给另一服务的连接只看到这一段）。
struct BlockRange {
    device: Arc<dyn BlockDevice>,
    first: u64,
    count: u64,
}

impl BlockDevice for BlockRange {
    fn disk_name(&self) -> &[u8] {
        self.device.disk_name()
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        self.device.read_block(self.translate(block_id)?, buf)
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        self.device.write_block(self.translate(block_id)?, buf)
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

impl BlockRange {
    fn translate(&self, block_id: usize) -> Result<usize, BlockError> {
        let block = block_id as u64;
        if block >= self.count {
            return Err(BlockError::InvalidBlock);
        }
        usize::try_from(self.first + block).map_err(|_| BlockError::InvalidBlock)
    }
}

/// 以 [`WORKERS`] 个线程服务一条连接，返回（客户端一端，服务端副本）。
fn serve_device(session: SessionState<Arc<dyn BlockDevice>>) -> Result<(Channel, Channel), Status> {
    let (client, server) = Channel::create()?;
    let block_size = session.get().block_size();
    let server = directory::serve_session(
        server,
        WORKERS,
        b"block-server",
        Arc::try_new(
            move |ordinal: u32, request: &mut Reader<'_>, _, reply: &mut Writer| {
                let device = session.get();
                match ordinal {
                    READ => {
                        let block = request.usize()?;
                        let mut data = alloc::vec![0u8; block_size];
                        device.read_block(block, &mut data).map_err(encode_error)?;
                        reply.bytes(&data);
                    }
                    WRITE => {
                        let block = request.usize()?;
                        let data = request.bytes()?;
                        device.write_block(block, data).map_err(encode_error)?;
                    }
                    FLUSH => device.flush().map_err(encode_error)?,
                    INFO => {
                        reply
                            .bytes(device.disk_name())
                            .u64(device.block_size() as u64)
                            .u64(device.block_count());
                    }
                    CONNECT => {
                        let first = request.u64()?;
                        let count = request.u64()?;
                        if first
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
                        let (range, _) = serve_device(SessionState::derived(range))?;
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

/// 驱动宿主：登记块设备（独占：同一时刻只有一个所有者，子范围连接由所有者派生）。
///
/// # Errors
///
/// 分配失败。
pub fn publish(directory: &Directory, device: Arc<dyn BlockDevice>) -> Result<(), Status> {
    let mut description = directory::description();
    description
        .bytes(device.disk_name())
        .u64(device.block_size() as u64)
        .u64(device.block_count());
    directory.add(
        Class::Block,
        &description,
        true,
        Box::new(move |end| {
            let (client, server) = serve_device(SessionState::new(device.clone(), end))?;
            Ok(Session {
                handles: alloc::vec![client.into_handle()],
                server,
            })
        }),
    )
}

/// 使用者端的远端块设备。
pub struct RemoteBlock {
    channel: Channel,
    name: Vec<u8>,
    block_size: usize,
    block_count: u64,
}

/// 以一条裸块协议连接（另一服务经 [`BlockDevice::share_range`] 交来）构造远端块设备。
///
/// # Errors
///
/// 设备信息查询失败。
pub fn from_channel(channel: Channel) -> Result<Arc<RemoteBlock>, Status> {
    let reply = call(&channel, &Writer::request(INFO), Vec::new())?;
    let mut reader = reply.reader();
    let name = reader.bytes()?.to_vec();
    let block_size = reader.usize()?;
    let block_count = reader.u64()?;
    Arc::try_new(RemoteBlock {
        channel,
        name,
        block_size,
        block_count,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

/// 连接目录中的块设备。
///
/// # Errors
///
/// 描述或句柄不完整。
pub fn connect(mut announcement: OpenedDevice) -> Result<Arc<RemoteBlock>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let mut reader = announcement.reader();
    let name = reader.bytes()?.to_vec();
    let block_size = reader.usize()?;
    let block_count = reader.u64()?;
    Arc::try_new(RemoteBlock {
        channel,
        name,
        block_size,
        block_count,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl BlockDevice for RemoteBlock {
    fn disk_name(&self) -> &[u8] {
        &self.name
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        if buf.len() != self.block_size {
            return Err(BlockError::InvalidBlock);
        }
        let mut request = Writer::request(READ);
        request.u64(block_id as u64);
        let reply = call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        let data = reply
            .reader()
            .bytes()
            .map_err(|_| BlockError::DeviceError)?;
        if data.len() != buf.len() {
            return Err(BlockError::DeviceError);
        }
        buf.copy_from_slice(data);
        Ok(buf.len())
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        if buf.len() != self.block_size {
            return Err(BlockError::InvalidBlock);
        }
        let mut request = Writer::request(WRITE);
        request.u64(block_id as u64).bytes(buf);
        call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        Ok(buf.len())
    }

    fn flush(&self) -> Result<(), BlockError> {
        call(&self.channel, &Writer::request(FLUSH), Vec::new()).map_err(decode_error)?;
        Ok(())
    }

    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn share_range(&self, first_block: u64, block_count: u64) -> Option<Handle> {
        let mut request = Writer::request(CONNECT);
        request.u64(first_block).u64(block_count);
        let mut reply = call(&self.channel, &request, Vec::new()).ok()?;
        take_handle(&mut reply.handles, 0).ok()
    }
}
