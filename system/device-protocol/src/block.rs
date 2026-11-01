//! 块设备：整块读写与持久化屏障，块内容随请求/回复内联传递。

use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::{BlockDevice, BlockError};
use native_runtime::native_abi::{self, Status};
use native_runtime::Channel;

use crate::directory::{self, Announcement, Class};
use crate::{Reader, Writer, call, serve, take_handle};

const READ: u32 = 1;
const WRITE: u32 = 2;
const FLUSH: u32 = 3;

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

/// 驱动宿主：发布块设备并服务其请求。
///
/// # Errors
///
/// channel、线程创建或目录写入失败。
pub fn publish(directory: &Channel, device: Arc<dyn BlockDevice>) -> Result<(), Status> {
    let (client, server) = Channel::create()?;
    let block_size = device.block_size();
    let mut description = directory::description();
    description
        .bytes(device.disk_name())
        .u64(block_size as u64)
        .u64(device.block_count());
    serve(
        server,
        WORKERS,
        b"block-server",
        Arc::try_new(move |ordinal: u32, request: &mut Reader<'_>, _, reply: &mut Writer| {
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
                _ => return Err(native_abi::ERR_NOT_SUPPORTED),
            }
            Ok(Vec::new())
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    directory::announce(directory, Class::Block, &description, alloc::vec![client.into_handle()])
}

/// 使用者端的远端块设备。
pub struct RemoteBlock {
    channel: Channel,
    name: Vec<u8>,
    block_size: usize,
    block_count: u64,
}

/// 连接目录中的块设备。
///
/// # Errors
///
/// 描述或句柄不完整。
pub fn connect(mut announcement: Announcement) -> Result<Arc<RemoteBlock>, Status> {
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
        let data = reply.reader().bytes().map_err(|_| BlockError::DeviceError)?;
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
}
